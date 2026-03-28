//! Process management helpers for pipeline subprocesses.
//!
//! ## Algorithm overview
//! - Bounded reads prevent untrusted subprocess output from exploding memory.
//! - Process groups allow clean termination of ffmpeg/yt-dlp trees.
//!
//! ## Hot-path markers
//! `read_bounded` is on the output processing path for ffmpeg and yt-dlp.

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

const MAX_STDERR_LINE_BYTES: usize = 4096;

pub(crate) struct BoundedRead {
    pub(crate) data: Vec<u8>,
    pub(crate) exceeded: bool,
}

pub(crate) struct SubprocessGuard {
    child: Child,
    pid: Option<i32>,
    // Armed until a successful wait; ensures drop kills orphaned children.
    armed: bool,
}

#[derive(Debug)]
pub(crate) struct JsonSubprocessOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr_tail: Box<str>,
}

#[derive(Debug)]
pub(crate) enum JsonSubprocessError {
    Io(std::io::Error),
    Timeout,
    OutputLimit { stderr_tail: Box<str> },
}

impl SubprocessGuard {
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
        let std_command = command.as_std();
        let args = std_command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        trace!(
            program = %std_command.get_program().to_string_lossy(),
            ?args,
            "Spawning subprocess"
        );
        configure_process_group(command);
        let child = command.spawn()?;
        let pid = child.id().map(|id| id as i32);
        debug!(pid = pid.unwrap_or_default(), "Spawned subprocess");
        Ok(Self {
            child,
            pid,
            armed: true,
        })
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.armed = false;
        }
        match &status {
            Ok(exit) => trace!(code = exit.code(), "Subprocess exited"),
            Err(error) => warn!(error = %error, "Failed waiting for subprocess"),
        }
        status
    }
}

impl Drop for SubprocessGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        debug!(
            pid = self.pid.unwrap_or_default(),
            "Cleaning up subprocess in Drop"
        );

        // Best-effort cleanup to avoid leaving ffmpeg/yt-dlp processes behind.
        #[cfg(unix)]
        {
            if let Some(pid) = self.pid {
                use nix::sys::signal::{Signal, kill, killpg};
                use nix::sys::wait::{WaitPidFlag, waitpid};
                use nix::unistd::Pid;

                let pid = Pid::from_raw(pid);
                let _ = killpg(pid, Signal::SIGKILL);
                let _ = kill(pid, Signal::SIGKILL);
                let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
            }
        }

        #[cfg(not(unix))]
        {
            let _ = self.child.start_kill();
        }
    }
}

pub(crate) async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    mut notify: Option<mpsc::Sender<()>>,
) -> std::io::Result<BoundedRead> {
    // Invariant: `data.len()` never exceeds `limit` by construction.
    trace!(limit, "Starting bounded read");
    let mut data = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        if !exceeded {
            let remaining = limit.saturating_sub(data.len());
            if read <= remaining {
                if let Some(slice) = buffer.get(..read) {
                    data.extend_from_slice(slice);
                }
            } else {
                if remaining > 0
                    && let Some(slice) = buffer.get(..remaining)
                {
                    data.extend_from_slice(slice);
                }
                exceeded = true;
                warn!(limit, read, "Bounded read exceeded limit");
                if let Some(tx) = notify.take() {
                    // Best-effort signal to cancel the owning process.
                    let _ = tx.try_send(());
                }
            }
        }
    }

    trace!(bytes = data.len(), exceeded, "Completed bounded read");
    Ok(BoundedRead { data, exceeded })
}

pub(crate) async fn run_json_subprocess(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_lines: usize,
) -> Result<JsonSubprocessOutput, JsonSubprocessError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut guard = SubprocessGuard::spawn(command).map_err(JsonSubprocessError::Io)?;

    let stdout = guard.child_mut().stdout.take().ok_or_else(|| {
        JsonSubprocessError::Io(std::io::Error::other("subprocess stdout missing"))
    })?;
    let stderr = guard.child_mut().stderr.take().ok_or_else(|| {
        JsonSubprocessError::Io(std::io::Error::other("subprocess stderr missing"))
    })?;

    let (limit_tx, mut limit_rx) = mpsc::channel::<()>(1);
    let limit_guard = limit_tx.clone();
    let stdout_handle = tokio::spawn(read_bounded(stdout, stdout_limit, Some(limit_tx)));
    let stderr_handle = tokio::spawn(read_stderr_tail(stderr, stderr_lines));

    enum WaitOutcome {
        Exit(std::io::Result<std::process::ExitStatus>),
        Timeout,
        OutputLimit,
    }

    let wait_outcome = tokio::select! {
        res = tokio::time::timeout(timeout, guard.wait()) => {
            match res {
                Ok(status) => WaitOutcome::Exit(status),
                Err(_) => WaitOutcome::Timeout,
            }
        }
        _ = limit_rx.recv() => WaitOutcome::OutputLimit,
    };
    drop(limit_guard);

    match wait_outcome {
        WaitOutcome::Exit(status) => {
            let status = status.map_err(JsonSubprocessError::Io)?;
            let stdout = stdout_handle
                .await
                .map_err(|error| JsonSubprocessError::Io(std::io::Error::other(error.to_string())))?
                .map_err(JsonSubprocessError::Io)?;
            let stderr_tail = stderr_handle
                .await
                .map_err(|error| JsonSubprocessError::Io(std::io::Error::other(error.to_string())))?
                .map_err(JsonSubprocessError::Io)?;

            if stdout.exceeded {
                return Err(JsonSubprocessError::OutputLimit { stderr_tail });
            }

            Ok(JsonSubprocessOutput {
                status,
                stdout: stdout.data,
                stderr_tail,
            })
        }
        WaitOutcome::Timeout => {
            kill_process_group(guard.child_mut()).await;
            let _ = guard.wait().await;
            stdout_handle.abort();
            stderr_handle.abort();
            Err(JsonSubprocessError::Timeout)
        }
        WaitOutcome::OutputLimit => {
            kill_process_group(guard.child_mut()).await;
            let _ = guard.wait().await;
            let _ = stdout_handle.await;
            let stderr_tail = match stderr_handle.await {
                Ok(Ok(stderr_tail)) => stderr_tail,
                _ => "unknown error".into(),
            };
            Err(JsonSubprocessError::OutputLimit { stderr_tail })
        }
    }
}

pub(crate) fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        // Create a fresh process group for clean termination of child trees.
        command.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

pub(crate) async fn kill_process_group(child: &mut Child) {
    trace!(
        pid = child.id().unwrap_or_default(),
        "Killing subprocess process group"
    );
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            // Kill the process group to stop ffmpeg/yt-dlp subprocess trees.
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }

    let _ = child.kill().await;
}

fn push_stderr_tail_line(
    tail: &mut Vec<u8>,
    line: &[u8],
    line_count: &mut usize,
    max_lines: usize,
) {
    tail.extend_from_slice(line);
    tail.push(b'\n');
    *line_count += 1;

    if max_lines == 0 {
        tail.clear();
        *line_count = 0;
        return;
    }

    if *line_count <= max_lines {
        return;
    }

    if let Some(first_newline) = tail.iter().position(|&byte| byte == b'\n') {
        tail.drain(..=first_newline);
        *line_count -= 1;
    } else {
        tail.clear();
        *line_count = 0;
    }
}

async fn read_stderr_tail<R: AsyncRead + Unpin>(
    stderr: R,
    max_lines: usize,
) -> Result<Box<str>, std::io::Error> {
    if max_lines == 0 {
        let mut reader = BufReader::new(stderr);
        let mut sink = Vec::with_capacity(256);
        while reader.read_until(b'\n', &mut sink).await? != 0 {
            sink.clear();
        }
        return Ok("".into());
    }

    let mut reader = BufReader::new(stderr);
    let mut tail = Vec::with_capacity(max_lines.saturating_mul(256));
    let mut line_count = 0;
    let mut buffer = Vec::with_capacity(256);

    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer).await?;
        if read == 0 {
            break;
        }

        if buffer.len() > MAX_STDERR_LINE_BYTES {
            buffer.truncate(MAX_STDERR_LINE_BYTES);
        }

        if buffer.last() == Some(&b'\n') {
            buffer.pop();
        }

        push_stderr_tail_line(&mut tail, &buffer, &mut line_count, max_lines);
    }

    if tail.last() == Some(&b'\n') {
        tail.pop();
    }

    Ok(String::from_utf8_lossy(&tail).into_owned().into_boxed_str())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    #[tokio::test]
    async fn read_bounded_keeps_data_when_under_limit() {
        let (mut writer, reader) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let _ = writer.write_all(b"hello world").await;
        });

        let result = read_bounded(reader, 32, None)
            .await
            .expect("bounded read should succeed");
        assert_eq!(result.data, b"hello world");
        assert!(!result.exceeded);
    }

    #[tokio::test]
    async fn read_bounded_truncates_and_notifies_on_overflow() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let (tx, mut rx) = mpsc::channel(1);

        tokio::spawn(async move {
            let payload = vec![b'x'; 96];
            let _ = writer.write_all(&payload).await;
        });

        let result = read_bounded(reader, 16, Some(tx))
            .await
            .expect("bounded read should succeed");
        assert_eq!(result.data.len(), 16);
        assert!(result.exceeded);
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn read_stderr_tail_keeps_last_lines_with_empty_entries() {
        let (mut writer, reader) = tokio::io::duplex(128);
        tokio::spawn(async move {
            let _ = writer.write_all(b"first\n\nthird\n").await;
        });

        let tail = read_stderr_tail(reader, 2)
            .await
            .expect("stderr tail read should succeed");

        assert_eq!(tail.as_ref(), "\nthird");
    }

    #[tokio::test]
    async fn read_stderr_tail_truncates_long_lines_before_joining() {
        let (mut writer, reader) = tokio::io::duplex(MAX_STDERR_LINE_BYTES * 2);
        tokio::spawn(async move {
            let long_line = vec![b'x'; MAX_STDERR_LINE_BYTES + 32];
            let _ = writer.write_all(&long_line).await;
            let _ = writer.write_all(b"\n").await;
        });

        let tail = read_stderr_tail(reader, 1)
            .await
            .expect("stderr tail read should succeed");

        assert_eq!(tail.len(), MAX_STDERR_LINE_BYTES);
        assert!(tail.as_bytes().iter().all(|&byte| byte == b'x'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_json_subprocess_times_out_and_kills_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo preparing >&2; sleep 10"]);

        let result = run_json_subprocess(&mut command, Duration::from_millis(50), 1024, 4).await;

        assert!(matches!(result, Err(JsonSubprocessError::Timeout)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_json_subprocess_reports_output_overflow() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "echo overflow >&2; while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
        ]);

        let result = run_json_subprocess(&mut command, Duration::from_secs(1), 64, 4).await;

        let error = result.expect_err("subprocess should fail once stdout exceeds the limit");
        assert!(matches!(error, JsonSubprocessError::OutputLimit { .. }));
        if let JsonSubprocessError::OutputLimit { stderr_tail } = error {
            assert!(stderr_tail.contains("overflow"));
        }
    }
}
