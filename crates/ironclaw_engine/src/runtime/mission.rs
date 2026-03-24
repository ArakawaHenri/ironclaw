//! Mission manager — orchestrates long-running goals that spawn threads over time.
//!
//! Missions track ongoing objectives and periodically spawn threads to make
//! progress. The manager handles lifecycle (create, pause, resume, complete)
//! and delegates thread spawning to [`ThreadManager`].

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::runtime::manager::ThreadManager;
use crate::traits::store::Store;
use crate::types::error::EngineError;
use crate::types::mission::{Mission, MissionCadence, MissionId, MissionStatus};
use crate::types::project::ProjectId;
use crate::types::thread::{ThreadConfig, ThreadId, ThreadType};

/// Manages mission lifecycle and thread spawning.
pub struct MissionManager {
    store: Arc<dyn Store>,
    thread_manager: Arc<ThreadManager>,
    /// Active missions indexed by ID for quick lookup.
    active: RwLock<Vec<MissionId>>,
}

impl MissionManager {
    pub fn new(store: Arc<dyn Store>, thread_manager: Arc<ThreadManager>) -> Self {
        Self {
            store,
            thread_manager,
            active: RwLock::new(Vec::new()),
        }
    }

    /// Create and persist a new mission. Returns the mission ID.
    pub async fn create_mission(
        &self,
        project_id: ProjectId,
        name: impl Into<String>,
        goal: impl Into<String>,
        cadence: MissionCadence,
    ) -> Result<MissionId, EngineError> {
        let mission = Mission::new(project_id, name, goal, cadence);
        let id = mission.id;
        self.store.save_mission(&mission).await?;
        self.active.write().await.push(id);
        debug!(mission_id = %id, "mission created");
        Ok(id)
    }

    /// Pause an active mission. No new threads will be spawned.
    pub async fn pause_mission(&self, id: MissionId) -> Result<(), EngineError> {
        self.store
            .update_mission_status(id, MissionStatus::Paused)
            .await?;
        debug!(mission_id = %id, "mission paused");
        Ok(())
    }

    /// Resume a paused mission.
    pub async fn resume_mission(&self, id: MissionId) -> Result<(), EngineError> {
        self.store
            .update_mission_status(id, MissionStatus::Active)
            .await?;
        debug!(mission_id = %id, "mission resumed");
        Ok(())
    }

    /// Mark a mission as completed.
    pub async fn complete_mission(&self, id: MissionId) -> Result<(), EngineError> {
        self.store
            .update_mission_status(id, MissionStatus::Completed)
            .await?;
        self.active.write().await.retain(|mid| *mid != id);
        debug!(mission_id = %id, "mission completed");
        Ok(())
    }

    /// Manually fire a mission — spawn a thread for it right now.
    pub async fn fire_mission(
        &self,
        id: MissionId,
        user_id: &str,
    ) -> Result<Option<ThreadId>, EngineError> {
        let mission = self.store.load_mission(id).await?;
        let mission = match mission {
            Some(m) => m,
            None => {
                return Err(EngineError::Store {
                    reason: format!("mission {id} not found"),
                });
            }
        };

        if mission.is_terminal() {
            warn!(mission_id = %id, status = ?mission.status, "cannot fire terminal mission");
            return Ok(None);
        }

        let thread_id = self
            .thread_manager
            .spawn_thread(
                &mission.goal,
                ThreadType::Mission,
                mission.project_id,
                ThreadConfig {
                    enable_reflection: true,
                    ..ThreadConfig::default()
                },
                None,
                user_id,
            )
            .await?;

        // Record the thread in mission history
        let mut updated = mission;
        updated.record_thread(thread_id);
        self.store.save_mission(&updated).await?;

        debug!(mission_id = %id, thread_id = %thread_id, "mission fired");
        Ok(Some(thread_id))
    }

    /// List all missions in a project.
    pub async fn list_missions(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Mission>, EngineError> {
        self.store.list_missions(project_id).await
    }

    /// Get a mission by ID.
    pub async fn get_mission(&self, id: MissionId) -> Result<Option<Mission>, EngineError> {
        self.store.load_mission(id).await
    }

    /// Tick — check all active missions and fire any that are due.
    ///
    /// For `Cron` cadence missions, checks `next_fire_at` against current time.
    /// For `Manual` missions, this is a no-op.
    /// Returns the IDs of threads spawned.
    pub async fn tick(&self, user_id: &str) -> Result<Vec<ThreadId>, EngineError> {
        let active_ids = self.active.read().await.clone();
        let mut spawned = Vec::new();
        let now = chrono::Utc::now();

        for mid in active_ids {
            let mission = match self.store.load_mission(mid).await? {
                Some(m) if m.status == MissionStatus::Active => m,
                _ => continue,
            };

            let should_fire = match &mission.cadence {
                MissionCadence::Cron { .. } => {
                    // Fire if next_fire_at has passed
                    mission
                        .next_fire_at
                        .is_some_and(|next| next <= now)
                }
                MissionCadence::Manual => false,
                MissionCadence::OnEvent { .. } | MissionCadence::OnPush => false,
            };

            if should_fire
                && let Some(tid) = self.fire_mission(mid, user_id).await?
            {
                spawned.push(tid);
            }
        }

        Ok(spawned)
    }
}
