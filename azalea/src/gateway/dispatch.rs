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
use tokio::signal;
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

/// Drive a shard event loop, dispatching handlers and collecting resume info.
///
/// ## Postconditions
/// Returns a snapshot of session state to allow resume on next startup.
#[tracing::instrument(name = "dispatcher", fields(shard.id = shard.id().number()), skip_all)]
pub async fn run(
    mut shard: Shard,
    mut event_handler: impl FnMut(Dispatcher, Event),
) -> SessionInfo {
    let mut shutdown = pin!(signal::ctrl_c());
    let tracker = TaskTracker::new();

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("ctrl-c received; closing shard");
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

    tracker.close();
    if !tracker.is_empty() {
        tracing::info!(pending = tracker.len(), "waiting for in-flight tasks");
        tracker.wait().await;
    }

    resume_info
}
