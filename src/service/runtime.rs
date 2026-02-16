use crate::git::has_transient_lock_contention_signature;
use crate::orchestrator::{
    OrchestratorError, OrchestratorResult, retry_with_policy_and_hook_and_delay,
};
use anyhow::anyhow;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const TRANSIENT_LOCK_RETRY_ATTEMPTS: usize = 3;
pub(super) const TRANSIENT_LOCK_MAX_ATTEMPTS: usize = TRANSIENT_LOCK_RETRY_ATTEMPTS + 1;
pub(super) const TRANSIENT_LOCK_RETRY_DELAY: Duration = Duration::from_secs(10);

pub(crate) fn worktree_root_for_project(project_dir: &Path, project_name: &str) -> PathBuf {
    let parent_dir = project_dir.parent().unwrap_or(project_dir);
    parent_dir.join(format!("{project_name}__worktrees"))
}

pub(crate) fn worker_worktree_dir_name(job_id: &str) -> String {
    format!("chief_{job_id}")
}

pub(crate) fn retry_transient_lock_contention_with_delay<T, F, H, S>(
    initial_error: anyhow::Error,
    mut operation: F,
    mut on_retry: H,
    sleep: S,
) -> OrchestratorResult<T>
where
    F: FnMut() -> OrchestratorResult<T>,
    H: FnMut(usize, usize, &anyhow::Error, Duration),
    S: FnMut(Duration),
{
    let mut first_error = Some(initial_error);
    let outcome = retry_with_policy_and_hook_and_delay(
        TRANSIENT_LOCK_MAX_ATTEMPTS,
        |_attempt, _total| {
            if let Some(err) = first_error.take() {
                Err(OrchestratorError::retryable(err))
            } else {
                operation()
            }
        },
        |_attempt, _total, err| {
            if is_transient_lock_contention_error(err) {
                Some(TRANSIENT_LOCK_RETRY_DELAY)
            } else {
                None
            }
        },
        |attempt, _total, err, delay| {
            on_retry(attempt, TRANSIENT_LOCK_RETRY_ATTEMPTS, err, delay);
        },
        sleep,
    );

    match outcome {
        Err(OrchestratorError::Retryable(err)) if is_transient_lock_contention_error(&err) => {
            let detail = err.to_string();
            Err(OrchestratorError::unrecoverable(anyhow!(
                "transient lock/contention retry budget exhausted after {TRANSIENT_LOCK_RETRY_ATTEMPTS} retries: {detail}"
            )))
        }
        other => other,
    }
}

pub(crate) fn is_transient_lock_contention_error(err: &anyhow::Error) -> bool {
    if has_transient_lock_contention_signature(&err.to_string()) {
        return true;
    }

    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && matches!(
                io_err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            )
        {
            return true;
        }

        if has_transient_lock_contention_signature(&cause.to_string()) {
            return true;
        }
    }

    false
}

pub(crate) fn is_known_unrecoverable_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && matches!(
                io_err.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::ReadOnlyFilesystem
            )
        {
            return true;
        }

        if let Some(sqlite_err) = cause.downcast_ref::<rusqlite::Error>()
            && is_unrecoverable_sqlite_error(sqlite_err)
        {
            return true;
        }
    }

    let text = err.to_string().to_ascii_lowercase();
    text.contains("agent binary")
        || text.contains("template load failed")
        || text.contains("is not a git repository")
}

fn is_unrecoverable_sqlite_error(err: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;

    match err.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy)
        | Some(ErrorCode::DatabaseLocked)
        | Some(ErrorCode::OperationInterrupted)
        | Some(ErrorCode::OperationAborted) => false,
        Some(_) => true,
        None => matches!(
            err,
            rusqlite::Error::InvalidPath(_) | rusqlite::Error::SqliteSingleThreadedMode
        ),
    }
}
