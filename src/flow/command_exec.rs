use super::SuiteCommandKind;
use crate::agent::AgentCancelledError;
use crate::config::TestSuiteConfig;
use crate::domain::{AgentOutput, WaitState};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub fn suite_command_cwd(project_dir: &Path, suite: &TestSuiteConfig) -> PathBuf {
    project_dir.join(&suite.test_root)
}

pub fn suite_command_for_kind(
    suite: &TestSuiteConfig,
    kind: SuiteCommandKind,
    target_override: Option<&str>,
) -> Option<String> {
    match kind {
        SuiteCommandKind::Test => Some(replace_target_placeholder(
            &suite.test_command,
            suite,
            target_override,
        )),
        SuiteCommandKind::Lint => suite
            .lint_command
            .as_ref()
            .map(|cmd| replace_target_placeholder(cmd, suite, target_override)),
        SuiteCommandKind::PostGreen => suite.post_green_command.clone(),
    }
}

pub fn execute_suite_command(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cancel_signal: &Arc<AtomicBool>,
    timeout_seconds: Option<u64>,
) -> Result<AgentOutput> {
    let mut process = Command::new("sh");
    process.arg("-lc").arg(command);
    process.current_dir(cwd);
    process.envs(env.iter());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to run command: {command}"))?;
    let (output, wait_state) =
        wait_for_command_with_cancel(&mut child, cancel_signal, timeout_seconds)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut merged_output = format!("{stdout}\n{stderr}").trim().to_owned();
    if wait_state == WaitState::TimedOut {
        let timeout_seconds = timeout_seconds.unwrap_or_default();
        if merged_output.is_empty() {
            merged_output = format!(
                "suite command timed out after {timeout_seconds} second(s) and was terminated."
            );
        } else {
            merged_output = format!(
                "suite command timed out after {timeout_seconds} second(s) and was terminated.\n{merged_output}"
            );
        }
    }
    Ok(AgentOutput {
        exit_code: if wait_state == WaitState::TimedOut {
            124
        } else {
            output.status.code().unwrap_or(1)
        },
        command: command.to_owned(),
        merged_output,
        stdout,
        stderr,
    })
}

pub fn execute_suite_cleanup_command(
    cleanup_command: Option<&str>,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_seconds: Option<u64>,
) -> Result<Option<AgentOutput>> {
    let Some(command) = cleanup_command
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let cancel_signal = Arc::new(AtomicBool::new(false));
    let out = execute_suite_command(command, cwd, env, &cancel_signal, timeout_seconds)?;
    Ok(Some(out))
}

pub fn configure_process_group(process: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
}

pub fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            let pgid = nix::unistd::Pid::from_raw(pid);
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
            std::thread::sleep(Duration::from_millis(200));
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn replace_target_placeholder(
    command: &str,
    suite: &TestSuiteConfig,
    target_override: Option<&str>,
) -> String {
    let target = target_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| suite.default_target.clone())
        .unwrap_or_else(|| ".".to_owned());
    command.replace("{target}", &target)
}

fn wait_for_command_with_cancel(
    child: &mut std::process::Child,
    cancel_signal: &Arc<AtomicBool>,
    timeout_seconds: Option<u64>,
) -> Result<(std::process::Output, WaitState)> {
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());
    let timeout = timeout_seconds
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs);
    let started = Instant::now();

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break (status, WaitState::Completed);
        }

        if cancel_signal.load(Ordering::SeqCst) {
            terminate_process_tree(child);
            break (child.wait()?, WaitState::Cancelled);
        }

        if timeout
            .map(|limit| started.elapsed() >= limit)
            .unwrap_or(false)
        {
            terminate_process_tree(child);
            break (child.wait()?, WaitState::TimedOut);
        }

        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = join_pipe_reader(stdout_reader, "stdout")?;
    let stderr = join_pipe_reader(stderr_reader, "stderr")?;

    if status.1 == WaitState::Cancelled {
        return Err(anyhow!(AgentCancelledError));
    }

    Ok((
        std::process::Output {
            status: status.0,
            stdout,
            stderr,
        },
        status.1,
    ))
}

fn spawn_pipe_reader<T>(pipe: Option<T>) -> JoinHandle<Result<Vec<u8>>>
where
    T: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut data = Vec::new();
        if let Some(mut stream) = pipe {
            stream.read_to_end(&mut data)?;
        }
        Ok(data)
    })
}

fn join_pipe_reader(handle: JoinHandle<Result<Vec<u8>>>, stream_name: &str) -> Result<Vec<u8>> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("failed reading {stream_name} stream")),
        Err(_) => Err(anyhow!("{stream_name} reader thread panicked")),
    }
}
