use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tracing::debug;

use crate::core::agent_instance::AgentRole;
use crate::memory::consistency_engine::ConsistencyEngine;
use crate::memory::hyperspace_store::HyperspaceStore;
use crate::memory::l0_store::L0Store;
use crate::memory::l1_session::L1Session;
use crate::memory::l2_blackboard::Blackboard;
use crate::memory::l3_projection::ProjectionEngine;
use crate::memory::memory_bus::MemoryBus;
use crate::CoreError;

pub struct MemoryScheduler {
    l0_store: Arc<L0Store>,
    blackboard: Arc<Blackboard>,
    projection: Arc<ProjectionEngine>,
    consistency: Arc<ConsistencyEngine>,
    memory_bus: Arc<MemoryBus>,
    sessions: parking_lot::RwLock<HashMap<String, L1Session>>,
    /// Optional HyperspaceStore for time-decayed vector search
    hyperspace: Option<Arc<HyperspaceStore>>,
    recall_requests: AtomicU64,
    recall_hits: AtomicU64,
}

static SCHEDULER_WIRED: AtomicBool = AtomicBool::new(false);
static HYPERSPACE_ATTACHED: AtomicBool = AtomicBool::new(false);
static RECALL_REQUESTS: AtomicU64 = AtomicU64::new(0);
static RECALL_HITS: AtomicU64 = AtomicU64::new(0);

impl MemoryScheduler {
    pub fn new(
        l0_store: Arc<L0Store>,
        blackboard: Arc<Blackboard>,
        projection: Arc<ProjectionEngine>,
        consistency: Arc<ConsistencyEngine>,
        memory_bus: Arc<MemoryBus>,
    ) -> Self {
        Self::with_hyperspace(
            l0_store,
            blackboard,
            projection,
            consistency,
            memory_bus,
            None,
        )
    }

    pub fn with_hyperspace(
        l0_store: Arc<L0Store>,
        blackboard: Arc<Blackboard>,
        projection: Arc<ProjectionEngine>,
        consistency: Arc<ConsistencyEngine>,
        memory_bus: Arc<MemoryBus>,
        hyperspace: Option<Arc<HyperspaceStore>>,
    ) -> Self {
        let this = Self {
            l0_store,
            blackboard,
            projection,
            consistency,
            memory_bus,
            sessions: parking_lot::RwLock::new(HashMap::new()),
            hyperspace,
            recall_requests: AtomicU64::new(0),
            recall_hits: AtomicU64::new(0),
        };
        SCHEDULER_WIRED.store(true, Ordering::Relaxed);
        HYPERSPACE_ATTACHED.store(this.hyperspace.is_some(), Ordering::Relaxed);
        this
    }

    /// Cheap Admin snapshot of scheduler wiring and recall counters.
    pub fn runtime_snapshot() -> serde_json::Value {
        serde_json::json!({
            "wired": SCHEDULER_WIRED.load(Ordering::Relaxed),
            "hyperspace_attached": HYPERSPACE_ATTACHED.load(Ordering::Relaxed),
            "recall_requests": RECALL_REQUESTS.load(Ordering::Relaxed),
            "recall_hits": RECALL_HITS.load(Ordering::Relaxed),
        })
    }

    pub async fn on_context_request(
        &self,
        agent_role: AgentRole,
        task_iri: &str,
        claims: Option<&crate::isolation::IsolationClaims>,
    ) -> Result<String, CoreError> {
        // Context is only served within the caller's verified scope.
        let Some(claims) = claims else {
            tracing::warn!(task_iri = %task_iri, "Context request refused: no verified isolation claims");
            return Ok(String::new());
        };
        let frame_name = match agent_role {
            AgentRole::Plan => "pa_init",
            AgentRole::Do => "da_input",
            AgentRole::Check => "ca_review",
            AgentRole::Act => "aa_decision",
        };

        let params = HashMap::new();
        let projection_result = self
            .projection
            .project(task_iri, frame_name, params, claims)
            .await;

        if let Ok(result) = projection_result {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&result) {
                if let Some(artifacts) = parsed.get("artifacts").and_then(|a| a.as_array()) {
                    if !artifacts.is_empty() {
                        return Ok(result);
                    }
                }
            } else {
                return Ok(result);
            }
        }

        let nodes = self.blackboard.query_nodes(task_iri)?;
        if !nodes.is_empty() {
            for n in &nodes {
                let _ = self.consistency.on_l2_read(&n.iri);
            }
            let contents: Vec<String> = nodes.iter().map(|n| n.json_ld.clone()).collect();
            return Ok(contents.join("\n"));
        }

        let results = self.l0_store.search(task_iri, 10)?;
        if !results.is_empty() {
            let contents: Vec<String> = results.iter().map(|r| r.content.clone()).collect();
            return Ok(contents.join("\n"));
        }

        Ok(String::new())
    }

    pub fn on_l1_overflow(&self, session_id: &str) -> Result<usize, CoreError> {
        let mut sessions = self.sessions.write();
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| CoreError::Internal {
                message: format!("Session not found: {}", session_id),
            })?;
        Ok(session.evict_by_policy())
    }

    /// Finish a task: persist its dirty L2 nodes, then release its subtree.
    ///
    /// `tenant_l0` must be the claims-verified L0 handle of the run that owns
    /// `task_iri`. The scheduler's own startup handle is the shared legacy
    /// store, which is read-only and has no tenant, so completion never writes
    /// there. Only the task's own subtree is flushed, so dirty nodes of
    /// concurrent runs (possibly other tenants) are never written into this
    /// tenant's store. A read-only `tenant_l0` still rejects the write.
    pub async fn on_task_complete(
        &self,
        task_iri: &str,
        tenant_l0: &L0Store,
    ) -> Result<(), CoreError> {
        self.blackboard.flush_dirty_subtree(task_iri, tenant_l0)?;
        if let Err(_error) = self
            .consistency
            .on_l2_write_for(task_iri, task_iri, &[], Some(tenant_l0))
            .await
        {
            let count = crate::memory::l0_store::note_l0_write_rejected("consistency");
            self.memory_bus
                .publish(
                    "L0_WRITE_REJECTED",
                    task_iri,
                    &serde_json::json!({"kind": "consistency", "count": count}).to_string(),
                )
                .await;
        }
        self.blackboard.release_subtree(task_iri)?;
        self.memory_bus
            .publish("TASK_COMPLETED", task_iri, "{}")
            .await;
        Ok(())
    }

    /// Settle a run that did not reach `on_task_complete` (failure, timeout,
    /// or cancellation).
    ///
    /// The task's dirty subtree is flushed into `tenant_l0` and then released.
    /// If the flush fails, the subtree is still released so L2 does not grow;
    /// the error is returned after that release. Only that task's subtree is
    /// released. The shared blackboard is not prefix-filtered: production task
    /// IRIs are `iri://task_<uuid>`, and dropping every other IRI would delete
    /// in-flight runs of other tenants. Nodes this run recorded outside the
    /// subtree can be removed with [`Blackboard::discard_recorded_nodes`];
    /// nothing else is.
    pub fn on_run_end(&self, task_iri: &str, tenant_l0: &L0Store) -> Result<(), CoreError> {
        let flushed = self.blackboard.flush_dirty_subtree(task_iri, tenant_l0);
        let released = self.blackboard.release_subtree(task_iri);
        flushed?;
        released?;
        Ok(())
    }

    /// Context request with time-decayed search at the vector-store level.
    ///
    /// Falls back to `on_context_request` when HyperspaceStore is not available.
    pub async fn context_request_with_decay(
        &self,
        agent_role: AgentRole,
        task_iri: &str,
        decay_lambda: f64,
        claims: Option<&crate::isolation::IsolationClaims>,
    ) -> Result<String, CoreError> {
        self.recall_requests.fetch_add(1, Ordering::Relaxed);
        RECALL_REQUESTS.fetch_add(1, Ordering::Relaxed);
        if let Some(ref hs) = self.hyperspace {
            let filter = crate::memory::hyperspace_store::HybridSearchFilter::new();
            let results = hs
                .search_with_time_decay(task_iri, &filter, decay_lambda, 10)
                .await?;
            if !results.is_empty() {
                self.recall_hits.fetch_add(1, Ordering::Relaxed);
                RECALL_HITS.fetch_add(1, Ordering::Relaxed);
                let contents: Vec<String> = results.iter().map(|r| r.text.clone()).collect();
                return Ok(contents.join("\n"));
            }
        }
        let fallback = self
            .on_context_request(agent_role, task_iri, claims)
            .await?;
        if !fallback.trim().is_empty() {
            self.recall_hits.fetch_add(1, Ordering::Relaxed);
            RECALL_HITS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(fallback)
    }

    /// Attach a HyperspaceStore at runtime (for delayed injection).
    pub fn with_hyperspace_store(&mut self, hs: Arc<HyperspaceStore>) {
        self.hyperspace = Some(hs);
        HYPERSPACE_ATTACHED.store(true, Ordering::Relaxed);
    }

    pub fn on_session_close(&self, session_id: &str) -> Result<(), CoreError> {
        let session = {
            let mut sessions = self.sessions.write();
            sessions
                .remove(session_id)
                .ok_or_else(|| CoreError::Internal {
                    message: format!("Session not found: {}", session_id),
                })?
        };

        let summary = session.summarize();
        let config = crate::CoreConfig::default();

        let json_ld = serde_json::json!({
            "@context": "https://wildagentos.org/context/memory",
            "@id": format!("iri://memory/{}", uuid::Uuid::new_v4().hyphenated()),
            "@type": "SessionSummary",
            "session_id": summary.session_id,
            "agent_id": summary.agent_id,
            "agent_role": summary.agent_role,
            "task_iri": summary.task_iri,
            "turn_count": summary.turn_count,
            "summary_text": summary.summary_text,
        })
        .to_string();

        self.blackboard.write_node(
            &format!("iri://session/{}", summary.session_id),
            &json_ld,
            &config,
        )?;

        let l0_iri = format!("iri://archive/session/{}", summary.session_id);
        let content = serde_json::json!({
            "session_id": summary.session_id,
            "agent_id": summary.agent_id,
            "agent_role": summary.agent_role,
            "task_iri": summary.task_iri,
            "turn_count": summary.turn_count,
            "summary_text": summary.summary_text,
        })
        .to_string();
        self.l0_store.store(&l0_iri, &content)?;

        debug!(session_id = %session_id, "Session closed and archived to L2+L0");
        Ok(())
    }

    pub fn create_session(
        &self,
        agent_id: &str,
        agent_role: &str,
        task_iri: &str,
        token_budget: usize,
    ) -> String {
        let session = L1Session::with_budget(agent_id, agent_role, task_iri, token_budget);
        let session_id = session.session_id().to_string();
        self.sessions.write().insert(session_id.clone(), session);
        session_id
    }

    pub fn get_session(&self, session_id: &str) -> Option<L1Session> {
        self.sessions.read().get(session_id).cloned()
    }

    pub fn add_summary_to_session(
        &self,
        session_id: &str,
        role: &str,
        summary: &str,
        l0_archive_iri: Option<String>,
    ) {
        let mut sessions = self.sessions.write();
        if let Some(session) = sessions.get_mut(session_id) {
            session.add_summary(role, summary, l0_archive_iri);
        }
    }

    pub async fn archive_to_l0(
        &self,
        session_id: &str,
        role: &str,
        thought: &str,
        content: &str,
    ) -> Result<String, CoreError> {
        let iri = {
            let sessions = self.sessions.read();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| CoreError::Internal {
                    message: format!("Session not found: {}", session_id),
                })?;
            session.archive_full_to_l0(&self.l0_store, role, thought, content)?
        };
        if let Err(e) = self.consistency.on_l0_update(&iri).await {
            tracing::warn!("Consistency on_l0_update failed: {}", e);
        }
        Ok(iri)
    }

    /// Insert an existing session (called by MemoryManager for synchronization)
    pub fn insert_session(&self, session: L1Session) {
        let id = session.session_id().to_string();
        self.sessions.write().insert(id, session);
    }

    /// Remove and return the specified session (called by MemoryManager for synchronous shutdown)
    pub fn remove_session(&self, session_id: &str) -> Option<L1Session> {
        self.sessions.write().remove(session_id)
    }

    /// Return the current session count
    pub fn session_count(&self) -> usize {
        self.sessions.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event_bus::EventBus;
    use crate::memory::consistency_engine::ConsistencyEngine;
    use crate::memory::l0_store::L0Store;
    use crate::memory::l2_blackboard::Blackboard;
    use crate::memory::l3_projection::ProjectionEngine;
    use crate::memory::memory_bus::MemoryBus;
    use tempfile::tempdir;

    fn setup_scheduler() -> (Arc<MemoryScheduler>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("l0_sched");
        let l0_store = Arc::new(L0Store::new(path.to_str().unwrap()).unwrap());
        let blackboard = Arc::new(Blackboard::new().unwrap());
        let projection = Arc::new(ProjectionEngine::new(blackboard.clone(), 1024));
        let event_bus = Arc::new(EventBus::new(100));
        let memory_bus = Arc::new(MemoryBus::new(event_bus));
        let consistency = Arc::new(ConsistencyEngine::new(
            memory_bus.clone(),
            l0_store.clone(),
            blackboard.clone(),
            projection.clone(),
        ));
        let scheduler = Arc::new(MemoryScheduler::new(
            l0_store,
            blackboard,
            projection,
            consistency,
            memory_bus,
        ));
        (scheduler, dir)
    }

    #[test]
    fn test_create_and_get_session() {
        let (scheduler, _dir) = setup_scheduler();
        let id = scheduler.create_session("agent1", "PA", "iri://task1", 1000);
        let session = scheduler.get_session(&id);
        assert!(session.is_some());
        assert_eq!(session.unwrap().agent_id(), "agent1");
    }

    #[test]
    fn test_session_count() {
        let (scheduler, _dir) = setup_scheduler();
        assert_eq!(scheduler.session_count(), 0);
        scheduler.create_session("a1", "PA", "iri://t1", 500);
        assert_eq!(scheduler.session_count(), 1);
        scheduler.create_session("a2", "DO", "iri://t2", 500);
        assert_eq!(scheduler.session_count(), 2);
    }

    #[test]
    fn test_insert_and_remove_session() {
        let (scheduler, _dir) = setup_scheduler();
        let session = L1Session::with_budget("ext", "PA", "iri://ext", 500);
        let id = session.session_id().to_string();

        scheduler.insert_session(session);
        assert_eq!(scheduler.session_count(), 1);

        let removed = scheduler.remove_session(&id);
        assert!(removed.is_some());
        assert_eq!(scheduler.session_count(), 0);
    }

    #[test]
    fn test_on_l1_overflow() {
        let (scheduler, _dir) = setup_scheduler();
        let id = scheduler.create_session("a1", "PA", "iri://t1", 10);

        let evicted = scheduler.on_l1_overflow(&id);
        assert!(evicted.is_ok());
    }

    #[test]
    fn test_add_summary_to_session() {
        let (scheduler, _dir) = setup_scheduler();
        let id = scheduler.create_session("a1", "PA", "iri://t1", 1000);

        scheduler.add_summary_to_session(&id, "PA", "Summary text", None);
        let session = scheduler.get_session(&id).unwrap();
        assert_eq!(session.turn_count(), 1);
    }

    #[test]
    fn test_on_session_close() {
        let (scheduler, _dir) = setup_scheduler();
        let id = scheduler.create_session("a1", "PA", "iri://t1", 1000);

        let result = scheduler.on_session_close(&id);
        assert!(result.is_ok());
        assert!(scheduler.get_session(&id).is_none());
    }

    #[test]
    fn test_on_session_close_nonexistent() {
        let (scheduler, _dir) = setup_scheduler();
        let result = scheduler.on_session_close("iri://nonexistent");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_on_task_complete() {
        let (scheduler, dir) = setup_scheduler();
        let tenant_l0 = L0Store::new(dir.path().join("tenant").to_str().unwrap()).unwrap();
        let result = scheduler
            .on_task_complete("iri://task_complete", &tenant_l0)
            .await;
        assert!(result.is_ok());
    }

    /// Production wiring: the scheduler holds the shared legacy L0, which is
    /// read-only. Completion must flush the task's dirty nodes into the run's
    /// own tenant L0 instead, and must leave other tasks' dirty nodes alone.
    #[tokio::test]
    async fn completion_flushes_only_the_task_subtree_into_the_run_tenant_l0() {
        let dir = tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy");
        let legacy = Arc::new(L0Store::open_legacy_readonly(legacy_dir.to_str().unwrap()).unwrap());
        let blackboard = Arc::new(Blackboard::new().unwrap());
        let projection = Arc::new(ProjectionEngine::new(blackboard.clone(), 1024));
        let memory_bus = Arc::new(MemoryBus::new(Arc::new(EventBus::new(100))));
        let consistency = Arc::new(ConsistencyEngine::new(
            memory_bus.clone(),
            legacy.clone(),
            blackboard.clone(),
            projection.clone(),
        ));
        let scheduler = MemoryScheduler::new(
            legacy.clone(),
            blackboard.clone(),
            projection,
            consistency,
            memory_bus,
        );
        let tenant_a =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "p", "u").unwrap();
        let tenant_a_l0 = L0Store::open_for_claims(dir.path(), &tenant_a).unwrap();

        let config = crate::CoreConfig::default();
        let own = "iri://task/run-a/result";
        let other = "iri://task/run-b/result";
        for iri in [own, other] {
            // A second write marks the node dirty (Modified).
            blackboard.write_node(iri, r#"{"v":1}"#, &config).unwrap();
            blackboard.write_node(iri, r#"{"v":2}"#, &config).unwrap();
            assert!(blackboard.read_node(iri).unwrap().unwrap().dirty);
        }

        scheduler
            .on_task_complete("iri://task/run-a", &tenant_a_l0)
            .await
            .expect("completion must not write the read-only legacy L0");

        assert!(tenant_a_l0.retrieve(own).unwrap().is_some());
        assert!(
            tenant_a_l0.retrieve(other).unwrap().is_none(),
            "another task's dirty node must not land in this tenant's L0"
        );
        assert!(blackboard.read_node(other).unwrap().unwrap().dirty);
        assert_eq!(legacy.count().unwrap(), 0);
    }

    /// A run without a writable tenant handle still fails closed rather than
    /// silently dropping its dirty nodes.
    #[tokio::test]
    async fn completion_with_a_read_only_handle_still_rejects_the_write() {
        let (scheduler, dir) = setup_scheduler();
        let read_only =
            L0Store::open_legacy_readonly(dir.path().join("ro").to_str().unwrap()).unwrap();
        let blackboard = scheduler.blackboard.clone();
        let config = crate::CoreConfig::default();
        let iri = "iri://task/ro-run/result";
        blackboard.write_node(iri, r#"{"v":1}"#, &config).unwrap();
        blackboard.write_node(iri, r#"{"v":2}"#, &config).unwrap();

        let err = scheduler
            .on_task_complete("iri://task/ro-run", &read_only)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::PermissionDenied { .. }), "{err}");
        let text = err.to_string();
        assert!(
            !text.contains(dir.path().to_str().unwrap_or("")),
            "permission error must not include the L0 root: {text}"
        );
    }

    /// Timeout, cancel, and failure never reach `on_task_complete`. `on_run_end`
    /// still flushes that task and releases only its subtree. Nodes this run
    /// did not record (session, memory, and other tasks) stay in L2 and are
    /// not written into this tenant. A node written once (`dirty == false`) is
    /// released without being persisted.
    #[test]
    fn stopped_run_flushes_its_subtree_and_discards_taskless_nodes() {
        let dir = tempdir().unwrap();
        let legacy = Arc::new(
            L0Store::open_legacy_readonly(dir.path().join("legacy").to_str().unwrap()).unwrap(),
        );
        let blackboard = Arc::new(Blackboard::new().unwrap());
        let projection = Arc::new(ProjectionEngine::new(blackboard.clone(), 1024));
        let memory_bus = Arc::new(MemoryBus::new(Arc::new(EventBus::new(100))));
        let consistency = Arc::new(ConsistencyEngine::new(
            memory_bus.clone(),
            legacy.clone(),
            blackboard.clone(),
            projection.clone(),
        ));
        let scheduler = MemoryScheduler::new(
            legacy.clone(),
            blackboard.clone(),
            projection,
            consistency,
            memory_bus,
        );
        let tenant =
            crate::isolation::IsolationClaims::from_verified("tenant-a", "p", "u").unwrap();
        let tenant_l0 = L0Store::open_for_claims(dir.path(), &tenant).unwrap();
        let config = crate::CoreConfig::default();

        let dirty = "iri://task/stopped/result";
        blackboard.write_node(dirty, r#"{"v":1}"#, &config).unwrap();
        blackboard.write_node(dirty, r#"{"v":2}"#, &config).unwrap();
        let clean = "iri://task/stopped/once";
        blackboard.write_node(clean, r#"{"v":1}"#, &config).unwrap();
        assert!(!blackboard.read_node(clean).unwrap().unwrap().dirty);
        let session = "iri://session/s1";
        blackboard
            .write_node(session, r#"{"v":1}"#, &config)
            .unwrap();
        blackboard
            .write_node(session, r#"{"v":2}"#, &config)
            .unwrap();
        let memory = "iri://memory/m1";
        blackboard
            .write_node(memory, r#"{"v":1}"#, &config)
            .unwrap();
        let other = "iri://task/other/result";
        blackboard.write_node(other, r#"{"v":1}"#, &config).unwrap();
        blackboard.write_node(other, r#"{"v":2}"#, &config).unwrap();

        scheduler
            .on_run_end("iri://task/stopped", &tenant_l0)
            .unwrap();

        assert!(tenant_l0.retrieve(dirty).unwrap().is_some());
        assert!(
            tenant_l0.retrieve(clean).unwrap().is_none(),
            "a node written once is not persisted"
        );
        assert!(blackboard.read_node(clean).unwrap().is_none());
        assert!(blackboard.read_node(dirty).unwrap().is_none());
        for iri in [session, memory] {
            assert!(
                tenant_l0.retrieve(iri).unwrap().is_none(),
                "{iri} must not be written into the tenant L0"
            );
            assert!(
                blackboard.read_node(iri).unwrap().is_some(),
                "{iri} was not recorded by this run and must stay in L2"
            );
        }
        assert!(blackboard.read_node(other).unwrap().unwrap().dirty);
        assert_eq!(legacy.count().unwrap(), 0);
    }

    /// PoC (review #435): production task IRIs are `iri://task_<uuid>`
    /// (core_types.rs init_task). Another tenant's run ending must not drop
    /// this tenant's in-flight dirty L2 nodes.
    #[test]
    fn poc_other_tenant_run_end_must_not_discard_inflight_production_task_nodes() {
        let dir = tempdir().unwrap();
        let legacy = Arc::new(
            L0Store::open_legacy_readonly(dir.path().join("legacy").to_str().unwrap()).unwrap(),
        );
        let blackboard = Arc::new(Blackboard::new().unwrap());
        let projection = Arc::new(ProjectionEngine::new(blackboard.clone(), 1024));
        let memory_bus = Arc::new(MemoryBus::new(Arc::new(EventBus::new(100))));
        let consistency = Arc::new(ConsistencyEngine::new(
            memory_bus.clone(),
            legacy.clone(),
            blackboard.clone(),
            projection.clone(),
        ));
        let scheduler = MemoryScheduler::new(
            legacy.clone(),
            blackboard.clone(),
            projection,
            consistency,
            memory_bus,
        );
        let a = crate::isolation::IsolationClaims::from_verified("tenant-a", "p", "u").unwrap();
        let b = crate::isolation::IsolationClaims::from_verified("tenant-b", "p", "u").unwrap();
        let a_l0 = L0Store::open_for_claims(dir.path(), &a).unwrap();
        let b_l0 = L0Store::open_for_claims(dir.path(), &b).unwrap();
        let config = crate::CoreConfig::default();
        let b_task = "iri://task_bbbbbbbb-0000-0000-0000-000000000000";
        let b_node = format!("{b_task}/result");
        blackboard
            .write_node(&b_node, r#"{"v":1}"#, &config)
            .unwrap();
        blackboard
            .write_node(&b_node, r#"{"v":2}"#, &config)
            .unwrap();
        assert!(blackboard.read_node(&b_node).unwrap().unwrap().dirty);
        // tenant A's run ends (timeout/cancel/success all call on_run_end)
        scheduler
            .on_run_end("iri://task_aaaaaaaa-0000-0000-0000-000000000000", &a_l0)
            .unwrap();
        assert!(
            blackboard.read_node(&b_node).unwrap().is_some(),
            "tenant A's run end deleted tenant B's in-flight dirty node"
        );
        let _ = b_l0;
    }

    /// Orphan cleanup may delete only IRIs this caller recorded. It must not
    /// prefix-filter the shared cache.
    #[test]
    fn discard_recorded_nodes_leaves_other_production_task_nodes() {
        let blackboard = Blackboard::new().unwrap();
        let config = crate::CoreConfig::default();
        let kept = "iri://task_bbbbbbbb-0000-0000-0000-000000000000/result";
        let recorded = "iri://session/this-run";
        blackboard.write_node(kept, r#"{"v":1}"#, &config).unwrap();
        blackboard.write_node(kept, r#"{"v":2}"#, &config).unwrap();
        blackboard
            .write_node(recorded, r#"{"v":1}"#, &config)
            .unwrap();

        blackboard
            .discard_recorded_nodes(&[recorded.to_string()])
            .unwrap();

        assert!(blackboard.read_node(kept).unwrap().unwrap().dirty);
        assert!(blackboard.read_node(recorded).unwrap().is_none());
    }
}
