//! Temp file lifecycle management for pipeline stages.
//!
//! ## Concurrency assumptions
//! Cleanup runs on a background task, but drop handlers may also fall back to
//! synchronous deletion if the queue is saturated.
//!
//! ## Trade-off acknowledgment
//! We favor eventual cleanup over strict ordering; this avoids blocking hot
//! pipeline paths on filesystem operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, mpsc, mpsc::error::TrySendError, watch};

const CLEANUP_CHANNEL_CAPACITY: usize = 10_000;
const CLEANUP_DRAIN_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Default)]
pub struct StaleTempCleanup {
    pub scanned_entries: usize,
    pub removed_paths: Vec<PathBuf>,
}

impl StaleTempCleanup {
    pub fn removed_entries(&self) -> usize {
        self.removed_paths.len()
    }
}

/// Async temp file cleanup manager.
///
/// ## Invariants
/// - Cleanup queue capacity is bounded (`CLEANUP_CHANNEL_CAPACITY`).
/// - A shutdown signal drains pending work up to a deadline.
#[derive(Clone, Debug)]
pub struct TempFileCleanup {
    inner: Arc<TempFileCleanupInner>,
}

#[derive(Debug)]
struct TempFileCleanupInner {
    tx: mpsc::Sender<PathBuf>,
    cancel: watch::Sender<bool>,
    join_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TempFileCleanup {
    /// Start the background cleanup task and its control channels.
    ///
    /// ## Rationale
    /// Centralizes cleanup so pipeline code can rely on RAII guards.
    pub fn new() -> Self {
        let (tx, mut rx) = mpsc::channel::<PathBuf>(CLEANUP_CHANNEL_CAPACITY);
        let (cancel, mut cancel_rx) = watch::channel(false);

        let join_handle = match Handle::try_current() {
            Ok(handle) => Some(handle.spawn(async move {
                let mut draining = false;
                loop {
                    tokio::select! {
                        _ = cancel_rx.changed(), if !draining => {
                            if *cancel_rx.borrow() {
                                // Stop accepting new paths and switch to draining mode.
                                draining = true;
                                rx.close();
                                break;
                            }
                        }
                        maybe_path = rx.recv(), if !draining => {
                            match maybe_path {
                                Some(path) => remove_path_async(&path).await,
                                None => break,
                            }
                        }
                    }
                }

                if draining {
                    let deadline = Instant::now() + Duration::from_secs(CLEANUP_DRAIN_TIMEOUT_SECS);
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }

                        match tokio::time::timeout(remaining, rx.recv()).await {
                            // Drain queued paths until timeout or channel closes.
                            Ok(Some(path)) => remove_path_async(&path).await,
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }

                tracing::debug!("Cleanup task shutting down");
            })),
            Err(_) => {
                // No runtime available: drop receiver to force sync cleanup in guards.
                drop(rx);
                None
            }
        };

        Self {
            inner: Arc::new(TempFileCleanupInner {
                tx,
                cancel,
                join_handle: Mutex::new(join_handle),
            }),
        }
    }

    /// Return a guard that schedules the path for deletion on drop.
    ///
    /// ## Postconditions
    /// The returned guard is responsible for deleting `path` when dropped.
    pub fn guard(&self, path: PathBuf) -> TempFileGuard {
        TempFileGuard {
            path,
            tx: self.inner.tx.clone(),
        }
    }

    /// Signal the cleanup task to shut down and await its completion.
    ///
    /// ## Edge-case handling
    /// Shutdown drains queued paths up to a bounded timeout to avoid hang.
    pub async fn shutdown(&self) {
        let _ = self.inner.cancel.send(true);
        if let Some(handle) = self.inner.join_handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}

impl Default for TempFileCleanup {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that removes a temporary file when dropped.
///
/// ## Invariant-preserving notes
/// Dropping a guard is idempotent; missing paths are ignored.
#[derive(Debug)]
pub struct TempFileGuard {
    path: PathBuf,
    tx: mpsc::Sender<PathBuf>,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        if path.as_os_str().is_empty() {
            return;
        }

        match self.tx.try_send(path) {
            Ok(()) => {}
            Err(TrySendError::Full(path)) | Err(TrySendError::Closed(path)) => {
                // Dropping the owned path here guarantees cleanup even if the
                // async queue cannot accept more work.
                remove_path_sync(&path);
            }
        }
    }
}

/// Perform synchronous cleanup of temp directory on shutdown.
///
/// ## Explicit non-goals
/// This does not retry failures; it is a best-effort cleanup pass.
///
/// This is best-effort and intentionally ignores missing files.
pub fn cleanup_temp_dir_sync(temp_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(temp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to cleanup temp file");
                }
            } else if path.is_dir()
                && let Err(e) = std::fs::remove_dir_all(&path)
            {
                tracing::warn!(path = %path.display(), error = %e, "Failed to cleanup temp dir");
            }
        }
    }
}

/// Remove stale temp entries older than the supplied age threshold.
///
/// ## Trade-off acknowledgment
/// Best-effort cleanup; failures are logged and ignored.
pub fn cleanup_stale_temp_entries(temp_dir: &Path, max_age: Duration) -> StaleTempCleanup {
    let now = std::time::SystemTime::now();
    let mut cleanup = StaleTempCleanup::default();

    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return cleanup;
    };

    for entry in entries.flatten() {
        cleanup.scanned_entries += 1;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "Failed to stat temp entry");
                continue;
            }
        };

        let modified = match metadata.modified() {
            Ok(time) => time,
            Err(_) => continue,
        };

        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < max_age {
            continue;
        }

        let result = if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };

        match result {
            Ok(()) => {
                cleanup.removed_paths.push(path);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "Failed to remove stale temp entry");
            }
        }
    }

    cleanup
}

/// Remove temp entries older than the supplied age threshold.
///
/// ## Trade-off acknowledgment
/// Best-effort cleanup; failures are logged and ignored.
pub fn cleanup_temp_dir_older_than(temp_dir: &Path, max_age: Duration) -> usize {
    cleanup_stale_temp_entries(temp_dir, max_age).removed_entries()
}

async fn remove_path_async(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {
            if let Err(e) = tokio::fs::remove_dir_all(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::trace!(path = %path.display(), error = %e, "TempFile cleanup failed");
            }
        }
        Err(e) => {
            tracing::trace!(path = %path.display(), error = %e, "TempFile cleanup failed");
        }
    }
}

fn remove_path_sync(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {
            let _ = std::fs::remove_dir_all(path);
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("azalea-{name}-{nanos}"))
    }

    #[test]
    fn cleanup_temp_dir_sync_removes_files_and_dirs() {
        let dir = unique_temp_dir("sync-cleanup");
        std::fs::create_dir_all(dir.join("nested")).expect("create nested dir");
        std::fs::write(dir.join("a.tmp"), b"x").expect("write file");
        std::fs::write(dir.join("nested/b.tmp"), b"y").expect("write nested file");

        cleanup_temp_dir_sync(&dir);

        let remaining = std::fs::read_dir(&dir).expect("read cleanup dir").count();
        assert_eq!(remaining, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_temp_dir_older_than_only_removes_stale_entries() {
        let dir = unique_temp_dir("stale-cleanup");
        std::fs::create_dir_all(&dir).expect("create dir");
        let old_file = dir.join("old.tmp");
        let new_file = dir.join("new.tmp");

        std::fs::write(&old_file, b"old").expect("write old file");
        std::thread::sleep(Duration::from_millis(40));
        std::fs::write(&new_file, b"new").expect("write new file");

        let removed = cleanup_temp_dir_older_than(&dir, Duration::from_millis(20));
        assert_eq!(removed, 1);
        assert!(!old_file.exists());
        assert!(new_file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_stale_temp_entries_returns_removed_paths() {
        let dir = unique_temp_dir("stale-cleanup-paths");
        std::fs::create_dir_all(&dir).expect("create dir");
        let old_file = dir.join("old.tmp");
        let new_file = dir.join("new.tmp");

        std::fs::write(&old_file, b"old").expect("write old file");
        std::thread::sleep(Duration::from_millis(40));
        std::fs::write(&new_file, b"new").expect("write new file");

        let cleanup = cleanup_stale_temp_entries(&dir, Duration::from_millis(20));
        assert_eq!(cleanup.scanned_entries, 2);
        assert_eq!(cleanup.removed_paths, vec![old_file.clone()]);
        assert!(!old_file.exists());
        assert!(new_file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn guard_drop_removes_file() {
        let dir = unique_temp_dir("guard-file");
        std::fs::create_dir_all(&dir).expect("create dir");
        let file = dir.join("temp.bin");
        std::fs::write(&file, b"payload").expect("write file");

        let cleanup = TempFileCleanup::new();
        let guard = cleanup.guard(file.clone());
        drop(guard);
        cleanup.shutdown().await;

        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_guard_drops_cleanup_all_files() {
        let dir = unique_temp_dir("concurrent-guards");
        std::fs::create_dir_all(&dir).expect("create dir");
        let cleanup = TempFileCleanup::new();

        let guards: Vec<_> = (0..32u32)
            .map(|idx| {
                let file = dir.join(format!("{idx}.tmp"));
                std::fs::write(&file, b"x").expect("write file");
                cleanup.guard(file)
            })
            .collect();

        drop(guards);
        cleanup.shutdown().await;

        let remaining = std::fs::read_dir(&dir).expect("read dir").count();
        assert_eq!(remaining, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_drop_removes_file_when_queue_is_full() {
        let dir = unique_temp_dir("guard-file-full");
        std::fs::create_dir_all(&dir).expect("create dir");
        let file = dir.join("temp.bin");
        std::fs::write(&file, b"payload").expect("write file");

        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(dir.join("queued.tmp")).expect("fill queue");

        let guard = TempFileGuard {
            path: file.clone(),
            tx,
        };
        drop(guard);

        assert!(!file.exists());
        assert!(rx.try_recv().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_drop_removes_file_when_queue_is_closed() {
        let dir = unique_temp_dir("guard-file-closed");
        std::fs::create_dir_all(&dir).expect("create dir");
        let file = dir.join("temp.bin");
        std::fs::write(&file, b"payload").expect("write file");

        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let guard = TempFileGuard {
            path: file.clone(),
            tx,
        };
        drop(guard);

        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
