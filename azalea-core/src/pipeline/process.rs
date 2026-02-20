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
        configure_process_group(command);
        let child = command.spawn()?;
        let pid = child.id().map(|id| id as i32);
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
        status
    }
}

impl Drop for SubprocessGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

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
                if let Some(tx) = notify.take() {
                    // Best-effort signal to cancel the owning process.
                    let _ = tx.try_send(());
                }
            }
        }
    }

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
