//! # Module overview
//! Event dispatching and shard management.
//!
//! ## Algorithm overview
//! Runs a shard event loop, dispatching handler tasks via a task tracker, and
//! emits session resume data on shutdown.
//!
//! ## Concurrency assumptions
//! The dispatcher only spawns tasks; it does not await them until shutdown.

use crate::gateway::resume::SessionInfo;
use std::{error::Error, future::Future, pin::pin};
use tokio_util::task::TaskTracker;
use twilight_gateway::{CloseFrame, Event, Shard, StreamExt as _};

/// Lightweight handle for spawning per-event tasks tied to shard shutdown.
///
/// ## Invariants
/// Tasks are registered with the tracker to enable graceful shutdown.
pub struct Dispatcher<'a> {
    tracker: &'a TaskTracker,
}

impl<'a> Dispatcher<'a> {
    fn new(tracker: &'a TaskTracker) -> Self {
        Self { tracker }
    }

    /// Spawn a handler task that will be awaited during graceful shutdown.
    ///
    /// ## Rationale
    /// Keeps the shard loop responsive by offloading heavy work.
    pub fn dispatch(self, future: impl Future<Output = ()> + Send + 'static) {
        self.tracker.spawn(future);
    }
}

async fn shutdown_dispatch_tasks(tracker: TaskTracker) {
    tracker.close();
    if !tracker.is_empty() {
        tracing::info!(pending = tracker.len(), "waiting for in-flight tasks");
        tracker.wait().await;
    }
}

/// Drive a shard event loop, dispatching handlers and collecting resume info.
///
/// ## Postconditions
/// Returns a snapshot of session state to allow resume on next startup.
#[tracing::instrument(name = "dispatcher", fields(shard.id = shard.id().number()), skip_all)]
pub async fn run(
    mut shard: Shard,
    mut event_handler: impl FnMut(Dispatcher, Event),
) -> SessionInfo {
    let mut shutdown = pin!(crate::shutdown::wait_for_shutdown_signal());
    let tracker = TaskTracker::new();

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                match signal {
                    Ok(signal) => {
                        tracing::info!(signal = signal.name(), "shutdown signal received; closing shard");
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "shutdown signal listener failed; closing shard");
                    }
                }
                shard.close(CloseFrame::RESUME);
                break;
            }
            event = shard.next_event(super::event::EVENT_TYPES) => {
                match event {
                    Some(Ok(Event::GatewayClose(_))) => {
                        tracing::debug!("gateway closed; waiting for reconnect");
                        continue;
                    }
                    Some(Ok(event)) => event_handler(Dispatcher::new(&tracker), event),
                    Some(Err(error)) => {
                        tracing::warn!(error = &error as &dyn Error, "shard failed to receive event");
                        continue;
                    }
                    None => break,
                }
            }
        }
    }

    // Snapshot resume state before we close outstanding tasks.
    let resume_info = SessionInfo::from(&shard);

    shutdown_dispatch_tasks(tracker).await;

    resume_info
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn shutdown_awaits_all_dispatched_tasks() {
        let tracker = TaskTracker::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();

        Dispatcher::new(&tracker).dispatch(async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            let _ = finished_tx.send(());
        });

        assert!(started_rx.await.is_ok(), "dispatched task should start");

        let mut shutdown = tokio::spawn(shutdown_dispatch_tasks(tracker));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
                .await
                .is_err(),
            "shutdown should wait for tracked tasks"
        );

        assert!(release_tx.send(()).is_ok(), "task should still be waiting");
        assert!(
            shutdown.await.is_ok(),
            "shutdown task should complete cleanly"
        );
        assert!(
            finished_rx.await.is_ok(),
            "tracked task should finish before shutdown returns"
        );
    }
}
