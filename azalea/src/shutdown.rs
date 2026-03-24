//! Shutdown signal handling shared across the process.
//!
//! ## Invariants
//! The first matching OS signal starts graceful shutdown. Callers may await the
//! helper again to implement a forced-abort path on a subsequent signal.

use tokio::signal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShutdownSignal {
    CtrlC,
    Sigterm,
}

impl ShutdownSignal {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::CtrlC => "ctrl_c",
            Self::Sigterm => "sigterm",
        }
    }
}

pub(crate) async fn wait_for_shutdown_signal() -> std::io::Result<ShutdownSignal> {
    #[cfg(unix)]
    {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = signal::ctrl_c() => {
                result?;
                Ok(ShutdownSignal::CtrlC)
            }
            _ = sigterm.recv() => Ok(ShutdownSignal::Sigterm),
        }
    }

    #[cfg(not(unix))]
    {
        signal::ctrl_c().await?;
        Ok(ShutdownSignal::CtrlC)
    }
}
