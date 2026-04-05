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
    #[cfg(windows)]
    job: Option<WindowsJob>,
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
        #[cfg(windows)]
        let job = attach_child_to_job(&child).map_or_else(
            |error| {
                warn!(
                    error = %error,
                    "Failed to attach subprocess to a Windows Job Object; cleanup is best-effort"
                );
                None
            },
            Some,
        );
        Ok(Self {
            child,
            pid,
            armed: true,
            #[cfg(windows)]
            job,
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

    pub(crate) async fn kill_process_tree(&mut self) {
        trace!(
            pid = self.child.id().unwrap_or_default(),
            "Killing subprocess process tree"
        );
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                use nix::sys::signal::{Signal, killpg};
                use nix::unistd::Pid;
                // Kill the process group to stop ffmpeg/yt-dlp subprocess trees.
                let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            if let Some(job) = &self.job
                && let Err(error) = job.terminate()
            {
                warn!(error = %error, "Failed to terminate Windows Job Object");
            }
        }

        let _ = self.child.kill().await;
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
            #[cfg(windows)]
            {
                if let Some(job) = &self.job {
                    if let Err(error) = job.terminate() {
                        warn!(error = %error, "Failed to terminate Windows Job Object in Drop");
                    }
                } else {
                    let _ = self.child.start_kill();
                }
            }
            #[cfg(not(windows))]
            {
                let _ = self.child.start_kill();
            }
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
    let mut data = Vec::with_capacity(limit.min(4096));
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
    let stdout = async {
        read_bounded(stdout, stdout_limit, Some(limit_tx))
            .await
            .map_err(JsonSubprocessError::Io)
    };
    let stderr = async {
        read_stderr_tail(stderr, stderr_lines)
            .await
            .map_err(JsonSubprocessError::Io)
    };
    let process = supervise_json_subprocess(guard, timeout, &mut limit_rx);

    let (stdout, stderr_tail, status) = tokio::try_join!(stdout, stderr, process)?;
    let Some(status) = status.filter(|_| !stdout.exceeded) else {
        return Err(JsonSubprocessError::OutputLimit { stderr_tail });
    };

    Ok(JsonSubprocessOutput {
        status,
        stdout: stdout.data,
        stderr_tail,
    })
}

async fn supervise_json_subprocess(
    mut guard: SubprocessGuard,
    timeout: Duration,
    limit_rx: &mut mpsc::Receiver<()>,
) -> Result<Option<std::process::ExitStatus>, JsonSubprocessError> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    tokio::select! {
        status = guard.wait() => status.map(Some).map_err(JsonSubprocessError::Io),
        _ = &mut deadline => {
            kill_process_group(&mut guard).await;
            let _ = guard.wait().await;
            Err(JsonSubprocessError::Timeout)
        }
        limit = limit_rx.recv() => {
            match limit {
                Some(()) => {
                    kill_process_group(&mut guard).await;
                    let _ = guard.wait().await;
                    Ok(None)
                }
                None => guard.wait().await.map(Some).map_err(JsonSubprocessError::Io),
            }
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

pub(crate) async fn kill_process_group(guard: &mut SubprocessGuard) {
    guard.kill_process_tree().await;
}

#[cfg(windows)]
struct WindowsJob {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WindowsJob {
    #[allow(unsafe_code)]
    fn create_kill_on_close() -> std::io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // Safety: a null name/attributes is the documented way to create an
        // unnamed job object. The returned handle is immediately wrapped in
        // `OwnedHandle`, transferring ownership to Rust RAII.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // Safety: `handle` is a fresh job-object handle we own, and the struct
        // pointer/size pair matches the Windows API contract exactly.
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            // Safety: `handle` was created above and has not been wrapped yet.
            unsafe {
                let _ = std::os::windows::io::OwnedHandle::from_raw_handle(handle);
            }
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self {
            // Safety: ownership of the fresh handle transfers into `OwnedHandle`
            // exactly once here, and the handle remains valid for the lifetime
            // of the guard.
            handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
        })
    }

    #[allow(unsafe_code)]
    fn assign_child(&self, child: &Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let Some(raw_handle) = child.raw_handle() else {
            return Err(std::io::Error::other("subprocess handle missing"));
        };

        // Safety: both handles are live OS handles borrowed for the duration of
        // the call only; ownership remains with `OwnedHandle`/`Child`.
        let assigned = unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle() as HANDLE, raw_handle as HANDLE)
        };
        if assigned == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn terminate(&self) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        // Safety: the job handle is owned by `self`; terminating it is the
        // intended shutdown path when Azalea needs to kill the full child tree.
        let terminated = unsafe { TerminateJobObject(self.handle.as_raw_handle() as _, 1) };
        if terminated == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn attach_child_to_job(child: &Child) -> std::io::Result<WindowsJob> {
    let job = WindowsJob::create_kill_on_close()?;
    job.assign_child(child)?;
    Ok(job)
}

struct StderrTailBuffer {
    lines: Vec<Vec<u8>>,
    next: usize,
    len: usize,
}

impl StderrTailBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            lines: vec![Vec::new(); capacity],
            next: 0,
            len: 0,
        }
    }

    fn push(&mut self, mut line: Vec<u8>) -> Vec<u8> {
        if self.lines.is_empty() {
            line.clear();
            return line;
        }

        let slot = match self.lines.get_mut(self.next) {
            Some(slot) => slot,
            None => unreachable!("ring index must stay within capacity"),
        };
        std::mem::swap(slot, &mut line);
        self.next += 1;
        if self.next == self.lines.len() {
            self.next = 0;
        }
        self.len = self.len.saturating_add(1).min(self.lines.len());
        line
    }

    fn into_boxed_str(self) -> Box<str> {
        if self.len == 0 {
            return "".into();
        }

        let total_bytes = self.total_bytes() + self.len.saturating_sub(1);
        let mut tail = Vec::with_capacity(total_bytes);

        for (offset, line) in self.iter_ordered_lines().enumerate() {
            if offset > 0 {
                tail.push(b'\n');
            }
            tail.extend_from_slice(line);
        }

        String::from_utf8_lossy(&tail).into_owned().into_boxed_str()
    }

    fn total_bytes(&self) -> usize {
        self.iter_ordered_lines().map(<[u8]>::len).sum()
    }

    fn iter_ordered_lines(&self) -> Box<dyn Iterator<Item = &[u8]> + '_> {
        if self.len < self.lines.len() {
            return Box::new(self.lines.iter().take(self.len).map(Vec::as_slice));
        }

        let (head, tail) = self.lines.split_at(self.next);
        Box::new(tail.iter().chain(head.iter()).map(Vec::as_slice))
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
    let mut tail = StderrTailBuffer::new(max_lines);
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

        buffer = tail.push(std::mem::take(&mut buffer));
    }

    Ok(tail.into_boxed_str())
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
    async fn read_stderr_tail_wraps_to_keep_only_latest_lines() {
        let (mut writer, reader) = tokio::io::duplex(128);
        tokio::spawn(async move {
            let _ = writer.write_all(b"one\ntwo\nthree\nfour\n").await;
        });

        let tail = read_stderr_tail(reader, 2)
            .await
            .expect("stderr tail read should succeed");

        assert_eq!(tail.as_ref(), "three\nfour");
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
