use anyhow::{Context, anyhow};
use redb::Database;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify, watch};

pub mod dedup;
pub mod metrics;

pub use dedup::Cache as DedupCache;
pub use metrics::{Snapshot as MetricsSnapshot, Stage, Tracker as Metrics};

/// Open a redb store, rotating the file aside when the on-disk contents are
/// corrupt so startup can continue with a fresh database.
pub(crate) fn open_or_rotate_corrupt_store<F>(
    kind: &'static str,
    path: &Path,
    init: F,
) -> anyhow::Result<Database>
where
    F: Fn(&Database) -> Result<(), redb::Error>,
{
    match open_store_once(path, &init) {
        Ok(db) => Ok(db),
        Err(error) if is_corruption_error(&error) => {
            let rotated_path = rotate_corrupt_store(path)?;
            let db = open_store_once(path, &init).with_context(|| {
                format!(
                    "failed to recreate {kind} redb store after rotating {}",
                    path.display()
                )
            })?;

            tracing::error!(
                store = kind,
                path = %path.display(),
                rotated_path = %rotated_path.display(),
                error = %error,
                "Detected redb corruption; rotated store and created a fresh one"
            );

            Ok(db)
        }
        Err(error) => Err(error.into()),
    }
}

fn open_store_once<F>(path: &Path, init: &F) -> Result<Database, redb::Error>
where
    F: Fn(&Database) -> Result<(), redb::Error>,
{
    let db = Database::create(path).map_err(redb::Error::from)?;
    init(&db)?;
    Ok(db)
}

fn is_corruption_error(error: &redb::Error) -> bool {
    matches!(error, redb::Error::Corrupted(_))
        || matches!(
            error,
            redb::Error::Io(io_error) if io_error.kind() == std::io::ErrorKind::InvalidData
        )
}

fn rotate_corrupt_store(path: &Path) -> anyhow::Result<PathBuf> {
    let rotated_path = next_corrupt_store_path(path)?;
    std::fs::rename(path, &rotated_path).with_context(|| {
        format!(
            "failed to rotate corrupt redb store {} to {}",
            path.display(),
            rotated_path.display()
        )
    })?;
    Ok(rotated_path)
}

fn next_corrupt_store_path(path: &Path) -> anyhow::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        anyhow!(
            "redb store path {} has no file name to rotate",
            path.display()
        )
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    for attempt in 0..=u16::MAX {
        let mut rotated_name = OsString::from(file_name);
        if attempt == 0 {
            rotated_name.push(format!(".corrupt.{timestamp}"));
        } else {
            rotated_name.push(format!(".corrupt.{timestamp}.{attempt}"));
        }

        let rotated_path = path.with_file_name(rotated_name);
        if !rotated_path.exists() {
            return Ok(rotated_path);
        }
    }

    Err(anyhow!(
        "failed to allocate unique corrupt-store path for {}",
        path.display()
    ))
}

/// Shared flush worker for storage-backed subsystems.
///
/// This stays concrete and local to storage so dedup and metrics can share
/// queue wakeups, periodic flushing, and shutdown without extra indirection.
#[derive(Debug)]
pub(crate) struct FlushWorker {
    running: AtomicBool,
    wake: Arc<Notify>,
    cancel: watch::Sender<bool>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl FlushWorker {
    pub(crate) fn new() -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            running: AtomicBool::new(false),
            wake: Arc::new(Notify::new()),
            cancel,
            handle: Mutex::new(None),
        }
    }

    pub(crate) fn schedule(&self) -> bool {
        if !self.running.load(Ordering::Relaxed) {
            return false;
        }
        self.wake.notify_one();
        true
    }

    pub(crate) async fn start<F, Fut>(&self, interval: Duration, flush: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if interval.is_zero() || self.running.swap(true, Ordering::Relaxed) {
            return;
        }

        let mut handle_guard = self.handle.lock().await;
        if handle_guard.is_some() {
            return;
        }

        let _ = self.cancel.send(false);
        let wake = Arc::clone(&self.wake);
        let mut cancel_rx = self.cancel.subscribe();
        *handle_guard = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate tick so startup does not flush empty or partial state.
            ticker.tick().await;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                    _ = wake.notified() => {
                        flush().await;
                    }
                    _ = ticker.tick() => {
                        flush().await;
                    }
                }
            }
        }));
    }

    pub(crate) async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.cancel.send(true);
        if let Some(handle) = self.handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}
