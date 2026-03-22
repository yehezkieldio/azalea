//! Process management helpers for pipeline subprocesses.
//!
//! ## Algorithm overview
//! - Bounded reads prevent untrusted subprocess output from exploding memory.
//! - Process groups allow clean termination of ffmpeg/yt-dlp trees.
//!
//! ## Hot-path markers
//! `read_bounded` is on the output processing path for ffmpeg and yt-dlp.

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use tokio::io::AsyncWriteExt;

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
}
