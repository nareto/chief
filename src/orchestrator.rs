use anyhow::Error;
use std::fmt;
use std::time::Duration;

pub type OrchestratorResult<T> = std::result::Result<T, OrchestratorError>;

#[derive(Debug)]
pub enum OrchestratorError {
    Retryable(Error),
    Unrecoverable(Error),
}

impl OrchestratorError {
    pub fn retryable(error: Error) -> Self {
        Self::Retryable(error)
    }

    pub fn unrecoverable(error: Error) -> Self {
        Self::Unrecoverable(error)
    }

    pub fn as_error(&self) -> &Error {
        match self {
            Self::Retryable(err) | Self::Unrecoverable(err) => err,
        }
    }

    pub fn is_unrecoverable(&self) -> bool {
        matches!(self, Self::Unrecoverable(_))
    }

    pub fn into_error(self) -> Error {
        match self {
            Self::Retryable(err) | Self::Unrecoverable(err) => err,
        }
    }
}

impl fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retryable(err) => write!(f, "retryable failure: {err:#}"),
            Self::Unrecoverable(err) => write!(f, "unrecoverable failure: {err:#}"),
        }
    }
}

pub fn retry_with_policy<T, F>(max_retries: usize, operation: F) -> OrchestratorResult<T>
where
    F: FnMut(usize, usize) -> OrchestratorResult<T>,
{
    retry_with_policy_and_hook(max_retries, operation, |_attempt, _max_retries, _err| {})
}

pub fn retry_with_policy_and_hook<T, F, H>(
    max_retries: usize,
    mut operation: F,
    mut on_retry: H,
) -> OrchestratorResult<T>
where
    F: FnMut(usize, usize) -> OrchestratorResult<T>,
    H: FnMut(usize, usize, &Error),
{
    let max_retries = max_retries.max(1);
    for attempt in 1..=max_retries {
        match operation(attempt, max_retries) {
            Ok(value) => return Ok(value),
            Err(OrchestratorError::Unrecoverable(err)) => {
                return Err(OrchestratorError::Unrecoverable(err));
            }
            Err(OrchestratorError::Retryable(err)) => {
                if attempt >= max_retries {
                    return Err(OrchestratorError::Retryable(err));
                }
                on_retry(attempt, max_retries, &err);
            }
        }
    }

    unreachable!("retry loop should always return or error");
}

pub fn retry_with_policy_and_hook_and_delay<T, F, P, H, S>(
    max_retries: usize,
    mut operation: F,
    mut retry_policy: P,
    mut on_retry: H,
    mut sleep: S,
) -> OrchestratorResult<T>
where
    F: FnMut(usize, usize) -> OrchestratorResult<T>,
    P: FnMut(usize, usize, &Error) -> Option<Duration>,
    H: FnMut(usize, usize, &Error, Duration),
    S: FnMut(Duration),
{
    let max_retries = max_retries.max(1);
    for attempt in 1..=max_retries {
        match operation(attempt, max_retries) {
            Ok(value) => return Ok(value),
            Err(OrchestratorError::Unrecoverable(err)) => {
                return Err(OrchestratorError::Unrecoverable(err));
            }
            Err(OrchestratorError::Retryable(err)) => {
                if attempt >= max_retries {
                    return Err(OrchestratorError::Retryable(err));
                }
                let Some(delay) = retry_policy(attempt, max_retries, &err) else {
                    return Err(OrchestratorError::Retryable(err));
                };
                on_retry(attempt, max_retries, &err, delay);
                if !delay.is_zero() {
                    sleep(delay);
                }
            }
        }
    }

    unreachable!("retry loop should always return or error");
}

#[cfg(test)]
mod tests {
    use super::{OrchestratorError, retry_with_policy, retry_with_policy_and_hook_and_delay};
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn retries_until_success() {
        let attempts = AtomicUsize::new(0);
        let out = retry_with_policy(4, |_attempt, _max| {
            let current = attempts.fetch_add(1, Ordering::SeqCst);
            if current < 2 {
                Err(OrchestratorError::retryable(anyhow!("transient")))
            } else {
                Ok("ok")
            }
        })
        .expect("third attempt should succeed");

        assert_eq!(out, "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn unrecoverable_stops_immediately() {
        let attempts = AtomicUsize::new(0);
        let err = retry_with_policy(4, |_attempt, _max| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(OrchestratorError::unrecoverable(anyhow!("fatal")))
        })
        .expect_err("must stop on unrecoverable");

        assert!(matches!(err, OrchestratorError::Unrecoverable(_)));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn delay_policy_applies_and_can_be_observed() {
        let attempts = AtomicUsize::new(0);
        let mut retry_callbacks = Vec::new();
        let mut sleep_calls = Vec::new();
        let out = retry_with_policy_and_hook_and_delay(
            4,
            |_attempt, _max| {
                let current = attempts.fetch_add(1, Ordering::SeqCst);
                if current < 2 {
                    Err(OrchestratorError::retryable(anyhow!("transient")))
                } else {
                    Ok("ok")
                }
            },
            |_attempt, _max, _err| Some(Duration::from_secs(10)),
            |attempt, max, _err, delay| {
                retry_callbacks.push((attempt, max, delay.as_secs()));
            },
            |delay| sleep_calls.push(delay.as_secs()),
        )
        .expect("third attempt should succeed");

        assert_eq!(out, "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(retry_callbacks, vec![(1, 4, 10), (2, 4, 10)]);
        assert_eq!(sleep_calls, vec![10, 10]);
    }

    #[test]
    fn delay_policy_can_stop_retries_early() {
        let attempts = AtomicUsize::new(0);
        let err = retry_with_policy_and_hook_and_delay(
            5,
            |_attempt, _max| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(OrchestratorError::retryable(anyhow!("retryable")))
            },
            |_attempt, _max, _err| None,
            |_attempt, _max, _err, _delay| panic!("retry hook should not run"),
            |_delay| panic!("sleep should not run"),
        )
        .expect_err("policy should stop retries");

        assert!(matches!(err, OrchestratorError::Retryable(_)));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
