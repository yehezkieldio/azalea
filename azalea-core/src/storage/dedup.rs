//! Deduplication cache with optional persistent backing store.
//!
//! ## Algorithm overview
//! - In-flight cache prevents concurrent duplicate work.
//! - LRU cache with TTL captures recently processed tweets.
//! - Optional Redb store provides persistence across restarts.
//!
//! ## Concurrency assumptions
//! This cache is shared across tasks; all state is guarded by async locks or
//! atomic primitives.
//!
//! ## Trade-off acknowledgment
//! Persistence failures degrade to in-memory mode to keep the pipeline alive.

use crate::config::{PipelineSettings, StorageSettings, TranscodeSettings};
use crate::media::TweetId;
use moka::future::Cache as MokaCache;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, watch};

/// Compact dedup key.
///
/// ## Invariants
/// - `scope_id` + `tweet_id` uniquely identify a job.
/// - Byte encoding is fixed at 16 bytes to allow stable storage keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub scope_id: u64,
    pub tweet_id: TweetId,
}

impl Key {
    /// Build a compact key from scope and tweet identifiers.
    pub fn new(scope_id: u64, tweet_id: TweetId) -> Self {
        Self { scope_id, tweet_id }
    }

    fn to_bytes(self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        let (scope_slice, tweet_slice) = buf.split_at_mut(8);
        scope_slice.copy_from_slice(&self.scope_id.to_le_bytes());
        tweet_slice.copy_from_slice(&self.tweet_id.0.to_le_bytes());
        buf
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 16 {
            return None;
        }
        let mut scope = [0u8; 8];
        let mut tweet = [0u8; 8];
        let (scope_bytes, tweet_bytes) = bytes.split_at(8);
        scope.copy_from_slice(scope_bytes);
        tweet.copy_from_slice(tweet_bytes);
        Some(Self {
            scope_id: u64::from_le_bytes(scope),
            tweet_id: TweetId(u64::from_le_bytes(tweet)),
        })
    }
}

const CLOCK_SKEW_THRESHOLD_SECS: u64 = 60;
static LAST_KNOWN_SECS: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_KNOWN_SECS.load(Ordering::Relaxed);

    if now + CLOCK_SKEW_THRESHOLD_SECS < last {
        tracing::warn!(
            now,
            last,
            "System clock moved backwards; using last known time"
        );
        return last;
    }

    if now >= last {
        LAST_KNOWN_SECS.store(now, Ordering::Relaxed);
        now
    } else {
        last
    }
}

/// Table definition for persistent dedup storage.
const DEDUP_TABLE: TableDefinition<&[u8], u64> = TableDefinition::new("dedup");

const DEFAULT_BATCH_SIZE: usize = 50;
const PENDING_WRITE_CAP_MULTIPLIER: usize = 10;
const FLUSH_FAILURE_THRESHOLD: usize = 5;
const FLUSH_BACKOFF_BASE_SECS: u64 = 1;
const FLUSH_BACKOFF_MAX_SECS: u64 = 300;

#[derive(Debug, Clone)]
struct PendingWrite {
    key: [u8; 16],
    timestamp: u64,
}

/// Deduplication cache with optional persistence and in-flight tracking.
///
/// ## Usage footguns
/// Call [`Cache::load_from_db`] once at startup to hydrate caches.
#[derive(Debug, Clone)]
pub struct Cache {
    cache: MokaCache<Key, ()>,
    inflight: MokaCache<Key, Arc<AtomicBool>>,
    db: Option<Arc<Database>>,
    ttl_secs: u64,
    pending_writes: Arc<RwLock<VecDeque<PendingWrite>>>,
    batch_size: usize,
    pending_cap: usize,
    flush_failures: Arc<AtomicUsize>,
    flush_backoff_until: Arc<AtomicU64>,
    persistence_disabled: Arc<AtomicBool>,
    flush_cancel: watch::Sender<bool>,
    flush_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    maintenance_cancel: watch::Sender<bool>,
    maintenance_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Cache {
    /// Initialize the cache and backing database based on configuration.
    ///
    /// ## Preconditions
    /// - Storage paths are writable if persistence is enabled.
    pub fn new(
        config: &StorageSettings,
        pipeline: &PipelineSettings,
        transcode: &TranscodeSettings,
    ) -> anyhow::Result<Self> {
        let inflight_ttl_secs = inflight_ttl_secs(pipeline, transcode);
        let cache = MokaCache::builder()
            .max_capacity(config.dedup_cache_size as u64)
            .time_to_live(Duration::from_secs(config.dedup_ttl_hours * 3600))
            .build();

        let inflight = MokaCache::builder()
            .max_capacity(config.dedup_cache_size as u64)
            .time_to_live(Duration::from_secs(inflight_ttl_secs))
            .build();

        let (db, pending_writes) = if config.dedup_persistent {
            // Opens existing DBs or initializes new ones.
            let db = Database::create(&config.dedup_db_path)?;
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(DEDUP_TABLE)?;
            }
            write_txn.commit()?;
            (
                Some(Arc::new(db)),
                Arc::new(RwLock::new(VecDeque::with_capacity(DEFAULT_BATCH_SIZE))),
            )
        } else {
            (None, Arc::new(RwLock::new(VecDeque::new())))
        };

        let (flush_cancel, _) = watch::channel(false);
        let (maintenance_cancel, _) = watch::channel(false);
        let pending_cap = DEFAULT_BATCH_SIZE
            .saturating_mul(PENDING_WRITE_CAP_MULTIPLIER)
            .max(1);

        Ok(Self {
            cache,
            inflight,
            db,
            ttl_secs: config.dedup_ttl_hours * 3600,
            pending_writes,
            batch_size: DEFAULT_BATCH_SIZE,
            pending_cap,
            flush_failures: Arc::new(AtomicUsize::new(0)),
            flush_backoff_until: Arc::new(AtomicU64::new(0)),
            persistence_disabled: Arc::new(AtomicBool::new(false)),
            flush_cancel,
            flush_handle: Arc::new(Mutex::new(None)),
            maintenance_cancel,
            maintenance_handle: Arc::new(Mutex::new(None)),
        })
    }

    /// Load persisted keys into the in-memory cache and prune expired entries.
    ///
    /// ## Postconditions
    /// - In-memory cache contains only non-expired entries.
    pub async fn load_from_db(&self) -> anyhow::Result<usize> {
        let Some(db) = &self.db else {
            return Ok(0);
        };

        let db = Arc::clone(db);
        let ttl_secs = self.ttl_secs;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Key>(DEFAULT_BATCH_SIZE);
        let _expired_count = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let now = now_secs();

            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(DEDUP_TABLE)?;
            let mut expired = Vec::new();

            for result in table.iter()? {
                let (key, value): (redb::AccessGuard<&[u8]>, redb::AccessGuard<u64>) = result?;
                let timestamp = value.value();
                // Skip expired entries, and clean them up opportunistically.
                if now.saturating_sub(timestamp) > ttl_secs {
                    expired.push(key.value().to_vec());
                    continue;
                }

                if let Some(parsed) = Key::from_bytes(key.value())
                    && tx.blocking_send(parsed).is_err()
                {
                    break;
                }
            }
            drop(read_txn);

            if !expired.is_empty()
                && let Ok(write_txn) = db.begin_write()
            {
                if let Ok(mut table) = write_txn.open_table(DEDUP_TABLE) {
                    for key in &expired {
                        let _: Option<redb::AccessGuard<u64>> = table.remove(key.as_slice())?;
                    }
                }
                let _ = write_txn.commit();
            }

            Ok(expired.len())
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))??;

        let mut inserted = 0usize;
        while let Some(key) = rx.recv().await {
            self.cache.insert(key, ()).await;
            inserted += 1;
        }

        Ok(inserted)
    }

    /// Check whether a tweet has already been processed or is in-flight.
    ///
    /// ## Edge-case handling
    /// If persistence is enabled, expired keys are removed on lookup.
    pub async fn is_duplicate(&self, scope_id: u64, tweet_id: TweetId) -> bool {
        let key = Key::new(scope_id, tweet_id);

        if self.inflight.get(&key).await.is_some() {
            // Any in-flight marker short-circuits to prevent duplicate work.
            return true;
        }

        if self.cache.get(&key).await.is_some() {
            // Positive cache hit: already processed recently.
            return true;
        }

        if self.db_duplicate_check(key).await {
            // Warm the in-memory cache after a persistent hit.
            self.cache.insert(key, ()).await;
            return true;
        }

        false
    }

    /// Mark a request as in-flight, returning `false` if already claimed.
    ///
    /// ## Concurrency assumptions
    /// This method is safe under concurrent calls; it uses atomic compare-exchange.
    pub async fn reserve_inflight(&self, scope_id: u64, tweet_id: TweetId) -> bool {
        let key = Key::new(scope_id, tweet_id);

        // Ordering invariant: claim the in-flight marker first, then check cache/DB.
        // This prevents concurrent duplicate work while still clearing the marker
        // if we discover the key already exists. Do not reorder without reasoning.
        let marker = self
            .inflight
            .get_with(key, async { Arc::new(AtomicBool::new(false)) })
            .await;

        if marker
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }

        if self.cache.get(&key).await.is_some() {
            self.inflight.invalidate(&key).await;
            return false;
        }

        if self.db_duplicate_check(key).await {
            self.cache.insert(key, ()).await;
            self.inflight.invalidate(&key).await;
            return false;
        }

        true
    }

    /// Clear in-flight tracking when a job fails or is canceled.
    pub async fn clear_inflight(&self, scope_id: u64, tweet_id: TweetId) {
        let key = Key::new(scope_id, tweet_id);
        self.inflight.invalidate(&key).await;
    }

    /// Mark a tweet as processed and queue a persistence write.
    pub async fn mark_processed(&self, scope_id: u64, tweet_id: TweetId) {
        let key = Key::new(scope_id, tweet_id);
        self.cache.insert(key, ()).await;
        self.inflight.invalidate(&key).await;

        let Some(_) = &self.db else {
            // In-memory-only mode: nothing to persist.
            return;
        };

        if self.persistence_disabled.load(Ordering::Relaxed) {
            // Persisting is disabled after repeated failures.
            return;
        }

        let db_key = key.to_bytes();
        let timestamp = now_secs();

        let should_flush = {
            let mut pending = self.pending_writes.write().await;
            pending.push_back(PendingWrite {
                key: db_key,
                timestamp,
            });
            self.enforce_pending_cap(&mut pending);
            pending.len() >= self.batch_size
        };

        if should_flush {
            // Best-effort flush when the batch size threshold is reached.
            self.flush().await;
        }
    }

    /// Flush pending writes to the backing database if enabled.
    ///
    /// ## Performance hints
    /// Called in a background task; avoids holding write locks across I/O.
    pub async fn flush(&self) {
        let Some(db) = &self.db else {
            return;
        };

        if self.persistence_disabled.load(Ordering::Relaxed) {
            return;
        }

        let now = now_secs();
        let backoff_until = self.flush_backoff_until.load(Ordering::Relaxed);
        if backoff_until > now {
            // Respect backoff window after repeated failures.
            return;
        }

        let writes_to_flush: Vec<PendingWrite> = {
            let mut pending = self.pending_writes.write().await;
            pending.drain(..).collect()
        };

        if writes_to_flush.is_empty() {
            return;
        }

        let db = Arc::clone(db);
        let writes_for_db = writes_to_flush.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(DEDUP_TABLE)?;
                for write in &writes_for_db {
                    let _ = table.insert(write.key.as_slice(), write.timestamp);
                }
            }
            write_txn.commit()?;
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => {
                self.flush_failures.store(0, Ordering::Relaxed);
                self.flush_backoff_until.store(0, Ordering::Relaxed);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Failed to flush dedup batch");
                // Requeue writes and apply exponential backoff on failure.
                self.handle_flush_failure(writes_to_flush).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to flush dedup batch");
                // Requeue writes and apply exponential backoff on failure.
                self.handle_flush_failure(writes_to_flush).await;
            }
        }
    }

    /// Current in-memory cache size, used for status reporting.
    pub fn cache_entries(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Pending write batch size for observability.
    pub async fn pending_writes_len(&self) -> usize {
        self.pending_writes.read().await.len()
    }

    /// Start a periodic maintenance task that prunes expired keys.
    pub async fn start_maintenance_task(&self, interval: Duration, batch_size: usize) {
        if self.db.is_none() || interval.is_zero() {
            return;
        }

        let mut handle_guard = self.maintenance_handle.lock().await;
        if handle_guard.is_some() {
            return;
        }

        let _ = self.maintenance_cancel.send(false);
        let dedup = self.clone();
        let mut cancel_rx = self.maintenance_cancel.subscribe();
        *handle_guard = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        let removed = dedup.prune_expired(batch_size).await;
                        if removed > 0 {
                            tracing::debug!(removed, "Pruned expired dedup entries");
                        }
                    }
                }
            }
        }));
    }

    /// Stop the periodic maintenance task.
    pub async fn stop_maintenance_task(&self) {
        let _ = self.maintenance_cancel.send(true);
        if let Some(handle) = self.maintenance_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Start a periodic flush task to persist pending writes.
    pub async fn start_flush_task(&self, interval: Duration) {
        if self.db.is_none() || interval.is_zero() {
            return;
        }

        let mut handle_guard = self.flush_handle.lock().await;
        if handle_guard.is_some() {
            return;
        }

        let _ = self.flush_cancel.send(false);
        let dedup = self.clone();
        let mut cancel_rx = self.flush_cancel.subscribe();
        *handle_guard = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        dedup.flush().await;
                    }
                }
            }
        }));
    }

    /// Stop the periodic flush task.
    pub async fn stop_flush_task(&self) {
        let _ = self.flush_cancel.send(true);
        if let Some(handle) = self.flush_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    async fn prune_expired(&self, batch_size: usize) -> usize {
        let Some(db) = &self.db else {
            return 0;
        };

        let db = Arc::clone(db);
        let ttl_secs = self.ttl_secs;
        let batch_size = batch_size.max(1);

        let result = tokio::task::spawn_blocking(move || -> Result<usize, redb::Error> {
            let now = now_secs();

            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(DEDUP_TABLE)?;
            let mut expired = Vec::new();

            for result in table.iter()? {
                let (key, value): (redb::AccessGuard<&[u8]>, redb::AccessGuard<u64>) = result?;
                let timestamp = value.value();
                if now.saturating_sub(timestamp) > ttl_secs {
                    expired.push(key.value().to_vec());
                    if expired.len() >= batch_size {
                        break;
                    }
                }
            }
            drop(read_txn);

            if expired.is_empty() {
                return Ok(0);
            }

            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(DEDUP_TABLE)?;
                for key in &expired {
                    let _: Option<redb::AccessGuard<u64>> = table.remove(key.as_slice())?;
                }
            }
            write_txn.commit()?;
            Ok(expired.len())
        })
        .await;

        match result {
            Ok(Ok(count)) => count,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Failed to prune dedup entries");
                0
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to prune dedup entries");
                0
            }
        }
    }

    async fn handle_flush_failure(&self, writes: Vec<PendingWrite>) {
        let failures = self.flush_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let backoff_secs = self.compute_backoff_secs(failures);
        let backoff_until = now_secs().saturating_add(backoff_secs);
        self.flush_backoff_until
            .store(backoff_until, Ordering::Relaxed);

        if failures >= FLUSH_FAILURE_THRESHOLD {
            // Hard-disable persistence after repeated failures to keep pipeline healthy.
            self.disable_persistence(writes.len()).await;
            return;
        }

        let mut pending = self.pending_writes.write().await;
        for write in writes {
            pending.push_back(write);
        }
        self.enforce_pending_cap(&mut pending);
    }

    async fn disable_persistence(&self, dropped: usize) {
        if self.persistence_disabled.swap(true, Ordering::Relaxed) {
            return;
        }

        let mut pending = self.pending_writes.write().await;
        let pending_len = pending.len();
        pending.clear();

        tracing::error!(
            dropped_pending = pending_len.saturating_add(dropped),
            "Disabling dedup persistence after repeated flush failures"
        );
    }

    fn compute_backoff_secs(&self, failures: usize) -> u64 {
        let exponent = failures.saturating_sub(1).min(8);
        let backoff = FLUSH_BACKOFF_BASE_SECS.saturating_mul(1u64 << exponent);
        backoff.min(FLUSH_BACKOFF_MAX_SECS)
    }

    fn enforce_pending_cap(&self, pending: &mut VecDeque<PendingWrite>) {
        if pending.len() <= self.pending_cap {
            return;
        }

        // Drop oldest writes when the queue grows beyond the cap.
        let overflow = pending.len().saturating_sub(self.pending_cap);
        for _ in 0..overflow {
            pending.pop_front();
        }

        tracing::error!(overflow, "Dropping dedup pending writes due to cap");
    }

    async fn db_duplicate_check(&self, key: Key) -> bool {
        let Some(db) = &self.db else {
            return false;
        };

        let db = Arc::clone(db);
        let ttl_secs = self.ttl_secs;
        let key_bytes = key.to_bytes();

        let result = tokio::task::spawn_blocking(move || -> Result<bool, redb::Error> {
            let now = now_secs();

            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(DEDUP_TABLE)?;
            let mut expired = false;

            if let Some(entry) = table.get(key_bytes.as_slice())? {
                let timestamp = entry.value();
                if now.saturating_sub(timestamp) <= ttl_secs {
                    return Ok(true);
                }
                expired = true;
            }
            drop(read_txn);

            if expired {
                // Clean up expired entries on lookup to keep the db lean.
                let write_txn = db.begin_write()?;
                {
                    let mut table = write_txn.open_table(DEDUP_TABLE)?;
                    let _: Option<redb::AccessGuard<u64>> = table.remove(key_bytes.as_slice())?;
                }
                let _ = write_txn.commit();
            }

            Ok(false)
        })
        .await;

        match result {
            Ok(Ok(hit)) => hit,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Dedup db lookup failed");
                false
            }
            Err(e) => {
                tracing::warn!(error = %e, "Dedup db lookup failed");
                false
            }
        }
    }
}

fn inflight_ttl_secs(pipeline: &PipelineSettings, transcode: &TranscodeSettings) -> u64 {
    let ffmpeg_secs = transcode.ffmpeg_timeout_secs;
    let pipeline_total = pipeline
        .download_timeout_secs
        .saturating_add(ffmpeg_secs)
        .saturating_add(pipeline.upload_timeout_secs);
    let doubled_ffmpeg = ffmpeg_secs.saturating_mul(2);
    let margin = 60;
    pipeline_total.max(doubled_ffmpeg).saturating_add(margin)
}
