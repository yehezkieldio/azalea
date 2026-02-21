//! # Module overview
//! Session resumption handling.
//!
//! ## Algorithm overview
//! Serializes shard session metadata to disk on shutdown and restores it on the
//! next startup to avoid cold reconnect penalties.
//!
//! ## Trade-off acknowledgment
//! Resume info is best-effort; corruption triggers a backup rename and a full
//! reconnect.

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use twilight_gateway::{Config, ConfigBuilder, Session, Shard, ShardId};

const INFO_FILE: &str = "azalea-resume-info.json";
const BACKUP_FILE: &str = "azalea-resume-info.json.bak";

fn temp_file_name() -> String {
    // Include PID to avoid collisions when multiple instances overlap.
    format!("{}.{}.tmp", INFO_FILE, std::process::id())
}

pub trait ConfigBuilderExt {
    /// Apply stored resume information to a gateway config.
    fn apply_resume(self, resume_info: SessionInfo) -> Self;
}

impl ConfigBuilderExt for ConfigBuilder {
    fn apply_resume(mut self, resume_info: SessionInfo) -> Self {
        if let Some(resume_url) = resume_info.resume_url {
            self = self.resume_url(resume_url);
        }
        if let Some(session) = resume_info.session {
            self = self.session(session);
        }

        self
    }
}

/// [`Shard`] session resumption information.
///
/// ## Invariants
/// `shard_total` matches the gateway-reported shard count.
#[derive(Debug, Deserialize, Serialize)]
pub struct SessionInfo {
    pub shard_id: u32,
    pub shard_total: u32,
    resume_url: Option<String>,
    session: Option<Session>,
}

impl SessionInfo {
    fn is_none(&self) -> bool {
        self.resume_url.is_none() && self.session.is_none()
    }

    fn matches(&self, shard_id: ShardId) -> bool {
        self.shard_id == shard_id.number() && self.shard_total == shard_id.total()
    }
}

impl From<&Shard> for SessionInfo {
    fn from(value: &Shard) -> Self {
        Self {
            shard_id: value.id().number(),
            shard_total: value.id().total(),
            resume_url: value.resume_url().map(ToOwned::to_owned),
            session: value.session().cloned(),
        }
    }
}

/// Persist resume metadata for fast reconnects on the next startup.
///
/// ## Postconditions
/// Writes are atomic via a temp file + rename sequence.
pub async fn save(info: &[SessionInfo]) -> anyhow::Result<()> {
    // Avoid creating empty files when resumption isn't possible.
    if info.iter().any(|resume| !resume.is_none()) {
        let contents = serde_json::to_vec(&info)?;
        let temp_file = temp_file_name();
        let mut file = fs::File::create(&temp_file).await?;
        file.write_all(&contents).await?;
        file.sync_all().await?;
        fs::rename(&temp_file, INFO_FILE).await?;
        let parent = std::path::Path::new(INFO_FILE)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        // fsync the parent directory to make the rename durable on crash.
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let dir = std::fs::File::open(parent)?;
            dir.sync_all()
        })
        .await??;
        tracing::debug!(count = info.len(), "Saved resume info");
    }

    Ok(())
}

/// Restore shards with previous session information when available.
///
/// ## Edge-case handling
/// Corrupt resume data is moved to a backup file and ignored.
pub async fn restore(config: Config, shards: u32) -> Vec<Shard> {
    let shard_ids: Vec<_> = (0..shards)
        .map(|shard| ShardId::new(shard, shards))
        .collect();

    let info_result = async {
        let contents = fs::read(INFO_FILE).await?;
        Ok::<_, anyhow::Error>(serde_json::from_slice::<Vec<SessionInfo>>(&contents)?)
    }
    .await;

    match info_result {
        Ok(info) => {
            let mut resumed = false;
            let shards = shard_ids
                .iter()
                .map(|&shard_id| {
                    if let Some(resume_info) = info.iter().find(|i| i.matches(shard_id)) {
                        resumed = true;
                        let builder =
                            ConfigBuilder::from(config.clone()).apply_resume(SessionInfo {
                                shard_id: resume_info.shard_id,
                                shard_total: resume_info.shard_total,
                                resume_url: resume_info.resume_url.clone(),
                                session: resume_info.session.clone(),
                            });
                        Shard::with_config(shard_id, builder.build())
                    } else {
                        Shard::with_config(shard_id, config.clone())
                    }
                })
                .collect();

            if resumed {
                tracing::info!("Resuming previous gateway sessions");
                if let Err(e) = fs::remove_file(INFO_FILE).await {
                    tracing::debug!(error = %e, "Could not remove resume file");
                }
            }

            shards
        }
        Err(e) => {
            // If the file exists but is unreadable, keep a backup for inspection.
            tracing::debug!(error = %e, "Could not restore resume info");
            if fs::metadata(INFO_FILE).await.is_ok() {
                let _ = fs::rename(INFO_FILE, BACKUP_FILE).await;
            }
            shard_ids
                .into_iter()
                .map(|shard_id| Shard::with_config(shard_id, config.clone()))
                .collect()
        }
    }
}
