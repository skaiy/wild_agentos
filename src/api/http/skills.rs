//! 技能注册 / manifest / Git 导入 / 准入流水线。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；本模块承载持久化、处理器与技能相关测试。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::skill_pipeline::TenantPromotionReview;
use crate::tools::skill_registry::SkillMeta;

use super::iam::UserIdentity;
use super::{data_dir, AppState};

/// 用户态注册技能的持久化文件路径（仅 POST 注册的技能，不含启动播种的默认技能）。
fn skills_store_path() -> std::path::PathBuf {
    data_dir().join("skills.json")
}

/// 启动时从磁盘加载用户态注册的技能；文件不存在或解析失败时返回空列表。
pub(crate) fn load_user_skills() -> Vec<SkillMeta> {
    match std::fs::read_to_string(skills_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 以 skill_iri 为主键 upsert 一条用户态技能并持久化（pretty JSON）。
fn save_user_skill(skill: &SkillMeta) -> std::io::Result<()> {
    let mut skills = load_user_skills();
    match skills.iter_mut().find(|s| s.skill_iri == skill.skill_iri) {
        Some(existing) => *existing = skill.clone(),
        None => skills.push(skill.clone()),
    }
    let path = skills_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&skills).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 按 skill_iri 从用户态技能文件删除一条并持久化。返回是否原本存在。
fn delete_user_skill(skill_iri: &str) -> std::io::Result<bool> {
    let mut skills = load_user_skills();
    let before = skills.len();
    skills.retain(|s| s.skill_iri != skill_iri);
    let existed = skills.len() != before;
    if existed {
        let path = skills_store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&skills).unwrap_or_else(|_| "[]".to_string());
        std::fs::write(&path, content)?;
    }
    Ok(existed)
}

/// 判定是否为系统级内置技能（`iri://` 命名空间，由内核启动播种，只读）。
fn is_system_skill_iri(iri: &str) -> bool {
    iri.starts_with("iri://")
}

/// 技能准入流水线运行记录的持久化文件路径。
pub(crate) fn pipeline_runs_path() -> std::path::PathBuf {
    data_dir().join("pipeline_runs.json")
}

/// 保留的最近流水线运行记录条数上限（超出则裁剪最早记录）。
/// Owner 记录不在这个上限里，截断运行历史不会丢掉 skill IRI 的归属。
pub(crate) const PIPELINE_RUNS_CAP: usize = 200;

/// One market package cannot fill the whole admission history by itself.
pub(crate) const MARKET_PACKAGE_SKILL_CAP: usize = 32;

/// Serializes pipeline-run read-modify-write in this process.
static PIPELINE_RUNS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_pipeline_runs() -> std::sync::MutexGuard<'static, ()> {
    PIPELINE_RUNS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// 从磁盘加载流水线运行记录（最新在前）。文件不存在时为空；解析失败是错误，
/// 调用方不得把它写成空列表。
fn load_pipeline_runs_unlocked() -> std::io::Result<Vec<crate::tools::skill_pipeline::PipelineRun>>
{
    match std::fs::read_to_string(pipeline_runs_path()) {
        Ok(content) => serde_json::from_str(&content).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub(crate) enum PipelineWriteError {
    Io(std::io::Error),
    /// The skill IRI is already owned by another tenant. The run was not written.
    OwnedByAnotherTenant,
}

impl From<std::io::Error> for PipelineWriteError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn publisher_tenant(run: &crate::tools::skill_pipeline::PipelineRun) -> Option<&str> {
    run.publisher_tenant_id
        .as_deref()
        .map(str::trim)
        .filter(|tenant_id| !tenant_id.is_empty())
}

fn publisher_project(run: &crate::tools::skill_pipeline::PipelineRun) -> Option<&str> {
    run.publisher_project_id
        .as_deref()
        .map(str::trim)
        .filter(|project_id| !project_id.is_empty())
}

fn skill_iri_owners_path() -> std::path::PathBuf {
    data_dir().join("skill_iri_owners.json")
}

/// Written once, beside the owner file. Deleting the owner file does not
/// clear it, so a later load cannot rebuild owners from truncated runs.
pub(crate) const SKILL_IRI_OWNERS_MIGRATED_FILE: &str = "skill_iri_owners.migrated";

fn owners_migration_marker_path() -> std::path::PathBuf {
    data_dir().join(SKILL_IRI_OWNERS_MIGRATED_FILE)
}

/// Durable owner of one skill IRI. Run history is capped; this record is not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SkillIriOwnerRecord {
    pub skill_iri: String,
    pub tenant_id: String,
    pub project_id: String,
    /// Latest admission written by this owner. Survives run-history truncation.
    pub published: bool,
    pub gate_passed: bool,
    #[serde(default = "default_owner_visibility")]
    pub visibility: crate::tools::skill_pipeline::SkillVisibility,
}

fn default_owner_visibility() -> crate::tools::skill_pipeline::SkillVisibility {
    crate::tools::skill_pipeline::SkillVisibility::Session
}

pub(crate) struct AdmissionSnapshot {
    pub runs: Vec<crate::tools::skill_pipeline::PipelineRun>,
    pub owners: HashMap<String, SkillIriOwnerRecord>,
}

fn publisher_pair(run: &crate::tools::skill_pipeline::PipelineRun) -> Option<(&str, &str)> {
    Some((publisher_tenant(run)?, publisher_project(run)?))
}

/// Fold one run into the owner map. A conflicting tenant does not move the owner.
fn replay_owner_run(
    owners: &mut HashMap<String, SkillIriOwnerRecord>,
    run: &crate::tools::skill_pipeline::PipelineRun,
) -> Result<(), PipelineWriteError> {
    let Some((tenant_id, project_id)) = publisher_pair(run) else {
        if owners.contains_key(&run.skill_iri) {
            return Err(PipelineWriteError::OwnedByAnotherTenant);
        }
        return Ok(());
    };
    if let Some(owner) = owners.get_mut(&run.skill_iri) {
        if owner.tenant_id != tenant_id {
            return Err(PipelineWriteError::OwnedByAnotherTenant);
        }
        owner.published = run.published;
        owner.gate_passed = run.gate_passed;
        owner.visibility = run.visibility;
        return Ok(());
    }
    if run.published && run.gate_passed {
        owners.insert(
            run.skill_iri.clone(),
            SkillIriOwnerRecord {
                skill_iri: run.skill_iri.clone(),
                tenant_id: tenant_id.to_string(),
                project_id: project_id.to_string(),
                published: true,
                gate_passed: true,
                visibility: run.visibility,
            },
        );
    }
    Ok(())
}

fn owners_from_runs(
    runs: &[crate::tools::skill_pipeline::PipelineRun],
) -> HashMap<String, SkillIriOwnerRecord> {
    let mut owners = HashMap::new();
    // Newest-first storage: the oldest qualifying run is the owner.
    for run in runs.iter().rev() {
        let _ = replay_owner_run(&mut owners, run);
    }
    owners
}

fn save_owners_unlocked(owners: &HashMap<String, SkillIriOwnerRecord>) -> std::io::Result<()> {
    let mut records: Vec<_> = owners.values().cloned().collect();
    records.sort_by(|left, right| left.skill_iri.cmp(&right.skill_iri));
    let bytes = serde_json::to_vec_pretty(&records)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let path = skill_iri_owners_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::config::write_file_atomically(&path, &bytes)?;
    // The marker is written with the owner file. A marker without that file
    // means the owner file was removed and must fail closed.
    write_owners_migration_marker()
}

fn path_exists(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn write_owners_migration_marker() -> std::io::Result<()> {
    let path = owners_migration_marker_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::config::write_file_atomically(&path, br#"{"version":1}"#)
}

fn ensure_owners_migrated() -> std::io::Result<()> {
    if path_exists(&owners_migration_marker_path())? {
        return Ok(());
    }
    write_owners_migration_marker()
}

/// Copy admission history into the owner file a single time.
///
/// The marker records that this copy finished. After it exists, a missing
/// owner file is an error even when `pipeline_runs.json` is gone. An empty
/// map would drop ownership. Deleting the marker and the owner file together
/// runs this copy again from whatever runs remain, and those runs may already
/// have been truncated to the latest 200, so ownership can be reset.
fn migrate_owners_from_runs(
    runs: &[crate::tools::skill_pipeline::PipelineRun],
) -> std::io::Result<HashMap<String, SkillIriOwnerRecord>> {
    let owners = owners_from_runs(runs);
    // A rejected request on an empty data dir must not create either file.
    // Writing the marker alone would make the next load look like a deleted
    // owner file. A runs file with nothing to own still gets an empty owner
    // file and the marker, so a later deletion is fail-closed.
    if !owners.is_empty() || path_exists(&pipeline_runs_path())? {
        save_owners_unlocked(&owners)?;
    }
    Ok(owners)
}

fn load_missing_owner_file(
    runs: &[crate::tools::skill_pipeline::PipelineRun],
) -> std::io::Result<HashMap<String, SkillIriOwnerRecord>> {
    if path_exists(&owners_migration_marker_path())? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "skill owner file is missing after migration",
        ));
    }
    migrate_owners_from_runs(runs)
}

/// Load the owner file. The first load migrates whatever runs are still on
/// disk and records that migration. A file that will not parse is an error
/// and is not replaced. After the marker exists, a missing owner file is an
/// error and is not rebuilt, whether or not run history is still on disk.
fn load_owners_unlocked(
    runs: &[crate::tools::skill_pipeline::PipelineRun],
) -> std::io::Result<HashMap<String, SkillIriOwnerRecord>> {
    match std::fs::read_to_string(skill_iri_owners_path()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => load_missing_owner_file(runs),
        Err(error) => Err(error),
        Ok(content) => {
            let records: Vec<SkillIriOwnerRecord> =
                serde_json::from_str(&content).map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                })?;
            ensure_owners_migrated()?;
            let mut owners = HashMap::new();
            for record in records {
                owners.insert(record.skill_iri.clone(), record);
            }
            let known = owners.len();
            for run in runs.iter().rev() {
                if owners.contains_key(&run.skill_iri) {
                    continue;
                }
                let _ = replay_owner_run(&mut owners, run);
            }
            if owners.len() != known {
                save_owners_unlocked(&owners)?;
            }
            Ok(owners)
        }
    }
}

/// Exposure follows the stored owner, not the newest run still inside the cap.
///
/// A later run can revoke publication only when that same tenant wrote it.
/// Another project in the owner tenant may still create its own exposure.
/// A run with no publisher tenant does not grant ownership.
pub(crate) fn skill_published_for_tenant(
    owner: Option<&SkillIriOwnerRecord>,
    tenant_id: &str,
    project_id: &str,
) -> bool {
    use crate::tools::skill_pipeline::SkillVisibility;
    let tenant_id = tenant_id.trim();
    if tenant_id.is_empty() || project_id.trim().is_empty() {
        return false;
    }
    owner.is_some_and(|owner| {
        owner.tenant_id == tenant_id
            && owner.published
            && owner.gate_passed
            && owner.visibility == SkillVisibility::Tenant
            && !owner.project_id.trim().is_empty()
    })
}

/// Count of the most recently loaded admission file. Updated on every read.
static RUNS_WITHOUT_PUBLISHER_TENANT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn runs_without_publisher_tenant_count() -> usize {
    RUNS_WITHOUT_PUBLISHER_TENANT.load(std::sync::atomic::Ordering::Relaxed)
}

fn note_runs_without_publisher_tenant(runs: &[crate::tools::skill_pipeline::PipelineRun]) {
    let count = runs
        .iter()
        .filter(|run| publisher_tenant(run).is_none())
        .count();
    RUNS_WITHOUT_PUBLISHER_TENANT.store(count, std::sync::atomic::Ordering::Relaxed);
    if count == 0 {
        return;
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            runs_without_publisher_tenant = count,
            "pipeline runs without publisher_tenant_id do not grant skill ownership and stay on disk"
        );
    });
}

pub(crate) fn admission_snapshot() -> std::io::Result<AdmissionSnapshot> {
    let _guard = lock_pipeline_runs();
    let runs = load_pipeline_runs_unlocked()?;
    note_runs_without_publisher_tenant(&runs);
    let owners = load_owners_unlocked(&runs)?;
    Ok(AdmissionSnapshot { runs, owners })
}

pub(crate) fn pipeline_runs_snapshot(
) -> std::io::Result<Vec<crate::tools::skill_pipeline::PipelineRun>> {
    Ok(admission_snapshot()?.runs)
}

pub(crate) fn is_tenant_published_skill(
    skill_iri: &str,
    tenant_id: &str,
    project_id: &str,
) -> bool {
    match admission_snapshot() {
        Ok(view) => skill_published_for_tenant(view.owners.get(skill_iri), tenant_id, project_id),
        Err(error) => {
            tracing::error!(error = %error, "pipeline run store is unreadable");
            false
        }
    }
}

pub(crate) async fn is_tenant_published_skill_async(
    skill_iri: String,
    tenant_id: String,
    project_id: String,
) -> bool {
    tokio::task::spawn_blocking(move || {
        is_tenant_published_skill(&skill_iri, &tenant_id, &project_id)
    })
    .await
    .unwrap_or(false)
}

pub(crate) fn skill_iri_owned_by_other_tenant(
    skill_iri: &str,
    tenant_id: &str,
) -> std::io::Result<bool> {
    let view = admission_snapshot()?;
    Ok(view
        .owners
        .get(skill_iri)
        .is_some_and(|owner| owner.tenant_id != tenant_id.trim()))
}

/// `skill://{tenant}/...` must name the publisher's verified tenant.
pub(crate) fn skill_iri_matches_publisher(skill_iri: &str, tenant_id: &str) -> bool {
    let Some(rest) = skill_iri.strip_prefix("skill://") else {
        return false;
    };
    let Some((segment, name)) = rest.split_once('/') else {
        return false;
    };
    !segment.is_empty() && !name.is_empty() && segment == tenant_id.trim()
}

pub(crate) fn skill_iri_mismatch_response() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error": "skill_iri_tenant_mismatch"})),
    )
        .into_response()
}

/// Ownership wins over the namespace check, so a foreign republish of an
/// owned IRI is 409. An unowned IRI outside the caller's namespace is 403.
///
/// Platform-admin register, import, and rerun pass `require_tenant_segment:
/// false`. Those routes already require the platform tenant, and they register
/// product namespaces such as `skill://battery/...`. The run is still owned by
/// the platform tenant, so a customer tenant cannot republish or expose it.
/// Market publish passes `true`: a customer tenant cannot pre-claim
/// `skill://other-tenant/...`.
pub(crate) async fn reject_skill_iri_write(
    skill_iri: &str,
    tenant_id: &str,
    require_tenant_segment: bool,
) -> Result<(), axum::response::Response> {
    match skill_iri_blocked_for_tenant(skill_iri, tenant_id).await? {
        true => Err(skill_iri_conflict_response()),
        false if require_tenant_segment && !skill_iri_matches_publisher(skill_iri, tenant_id) => {
            Err(skill_iri_mismatch_response())
        }
        false => Ok(()),
    }
}

pub(crate) fn skill_iri_conflict_response() -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": "skill_iri_owned_by_another_tenant"})),
    )
        .into_response()
}

fn pipeline_store_failure(error: impl std::fmt::Display) -> axum::response::Response {
    tracing::error!(error = %error, "pipeline run store failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "pipeline_run_store_failed"})),
    )
        .into_response()
}

/// `Err` is an HTTP response. `Ok(false)` means this tenant may write a run.
pub(crate) async fn skill_iri_blocked_for_tenant(
    skill_iri: &str,
    tenant_id: &str,
) -> Result<bool, axum::response::Response> {
    let skill_iri = skill_iri.to_string();
    let tenant_id = tenant_id.to_string();
    match tokio::task::spawn_blocking(move || {
        skill_iri_owned_by_other_tenant(&skill_iri, &tenant_id)
    })
    .await
    {
        Ok(Ok(blocked)) => Ok(blocked),
        Ok(Err(error)) => Err(pipeline_store_failure(error)),
        Err(error) => Err(pipeline_store_failure(error)),
    }
}

pub(crate) fn verified_publisher_tenant(identity: &UserIdentity) -> String {
    use crate::isolation::IsolationScopeProvenance;
    identity
        .isolation_claims()
        .filter(|claims| claims.provenance() == IsolationScopeProvenance::VerifiedExplicit)
        .map(|claims| claims.tenant_id().trim().to_string())
        .filter(|tenant_id| !tenant_id.is_empty())
        .unwrap_or_default()
}

/// Copy an explicit verified publisher onto a pipeline context. A defaulted
/// or missing project is left unset so the run cannot authorize exposure.
fn record_publisher_from_identity(
    ctx: &mut crate::tools::skill_pipeline::PipelineContext,
    identity: &UserIdentity,
) {
    use crate::isolation::IsolationScopeProvenance;

    let Some(claims) = identity.isolation_claims() else {
        return;
    };
    if claims.provenance() != IsolationScopeProvenance::VerifiedExplicit {
        return;
    }
    ctx.record_publisher(claims.tenant_id(), claims.project_id());
}

fn save_pipeline_runs_unlocked(
    runs: &[crate::tools::skill_pipeline::PipelineRun],
) -> std::io::Result<()> {
    let path = pipeline_runs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(runs)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    super::config::write_file_atomically(&path, &bytes)
}

/// Append every run or none. Owner records are updated under the same lock
/// and are not truncated with the run history.
pub(crate) fn append_pipeline_runs(
    new_runs: &[crate::tools::skill_pipeline::PipelineRun],
) -> Result<(), PipelineWriteError> {
    let _guard = lock_pipeline_runs();
    let mut runs = load_pipeline_runs_unlocked()?;
    let mut owners = load_owners_unlocked(&runs)?;
    for run in new_runs {
        replay_owner_run(&mut owners, run)?;
    }
    for run in new_runs.iter().rev() {
        runs.insert(0, run.clone());
    }
    if runs.len() > PIPELINE_RUNS_CAP {
        runs.truncate(PIPELINE_RUNS_CAP);
    }
    note_runs_without_publisher_tenant(&runs);
    save_owners_unlocked(&owners)?;
    save_pipeline_runs_unlocked(&runs)?;
    Ok(())
}

/// 追加一条运行记录并持久化（最新在前，超上限裁剪最早）。
/// 读-改-写持有进程锁，落盘走临时文件 + fsync + rename。解析失败时不写。
/// 其他租户不能改写已有 owner 的 skill IRI。
pub(crate) fn append_pipeline_run(
    run: &crate::tools::skill_pipeline::PipelineRun,
) -> Result<(), PipelineWriteError> {
    append_pipeline_runs(std::slice::from_ref(run))
}

pub(crate) async fn append_pipeline_run_async(
    run: crate::tools::skill_pipeline::PipelineRun,
) -> Result<(), PipelineWriteError> {
    tokio::task::spawn_blocking(move || append_pipeline_run(&run))
        .await
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "pipeline run writer task failed");
            Err(PipelineWriteError::Io(std::io::Error::other(
                "pipeline run writer failed",
            )))
        })
}

/// Two-party rendezvous. A timeout releases every waiter so a missing peer
/// cannot leave a worker blocked for the rest of the process.
#[cfg(test)]
pub(crate) struct AdmissionBarrier {
    parties: usize,
    state: std::sync::Mutex<AdmissionBarrierState>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
struct AdmissionBarrierState {
    arrived: usize,
    released: bool,
}

#[cfg(test)]
impl AdmissionBarrier {
    pub(crate) fn new(parties: usize) -> Self {
        Self {
            parties,
            state: std::sync::Mutex::new(AdmissionBarrierState {
                arrived: 0,
                released: false,
            }),
            cv: std::sync::Condvar::new(),
        }
    }

    pub(crate) fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if guard.released {
            return true;
        }
        guard.arrived += 1;
        if guard.arrived >= self.parties {
            guard.released = true;
            self.cv.notify_all();
            return true;
        }
        let start = std::time::Instant::now();
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                guard.released = true;
                self.cv.notify_all();
                return false;
            }
            let (next, wait) = self
                .cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|error| error.into_inner());
            guard = next;
            if guard.released {
                return true;
            }
            if wait.timed_out() {
                guard.released = true;
                self.cv.notify_all();
                return false;
            }
        }
    }
}

/// Lets a test release two admission writers into the run/registry section
/// together. Unset in production builds.
#[cfg(test)]
pub(crate) async fn await_admission_write_barrier() {
    let barrier = ADMISSION_WRITE_BARRIER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(barrier) = barrier {
        let _ = tokio::task::spawn_blocking(move || {
            barrier.wait_timeout(std::time::Duration::from_secs(5));
        })
        .await;
    }
}

#[cfg(test)]
static ADMISSION_WRITE_BARRIER: std::sync::Mutex<Option<std::sync::Arc<AdmissionBarrier>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_admission_write_barrier(barrier: Option<std::sync::Arc<AdmissionBarrier>>) {
    *ADMISSION_WRITE_BARRIER
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = barrier;
}

pub(crate) async fn append_pipeline_runs_async(
    runs: Vec<crate::tools::skill_pipeline::PipelineRun>,
) -> Result<(), PipelineWriteError> {
    tokio::task::spawn_blocking(move || append_pipeline_runs(&runs))
        .await
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "pipeline run writer task failed");
            Err(PipelineWriteError::Io(std::io::Error::other(
                "pipeline run writer failed",
            )))
        })
}

pub(crate) fn persist_admitted_skill(
    registry: &crate::tools::skill_registry::SkillRegistry,
    skill: &SkillMeta,
) -> Result<(), String> {
    save_user_skill(skill).map_err(|error| error.to_string())?;
    registry.register_skill(skill.clone());
    Ok(())
}

pub(crate) fn pipeline_write_response(error: PipelineWriteError) -> axum::response::Response {
    match error {
        PipelineWriteError::OwnedByAnotherTenant => skill_iri_conflict_response(),
        PipelineWriteError::Io(error) => pipeline_store_failure(error),
    }
}

pub(crate) async fn list_skills_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let skills = state.core.skills.list_all_skills();
    let trusted_key_count = state.core.skills.trusted_key_count();
    let enriched: Vec<Value> = skills
        .iter()
        .map(|s| {
            let status = state.core.skills.verify_skill_signature(s);
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("signature_status".into(), json!(status.as_str()));
            }
            v
        })
        .collect();
    Json(json!({
        "count": enriched.len(),
        "trusted_key_count": trusted_key_count,
        "skills": enriched,
    }))
}

/// skill.yaml 下载端点的查询参数。
#[derive(Deserialize)]
pub(crate) struct SkillManifestQuery {
    iri: String,
}

/// 将字符串转义为合法的 YAML 双引号标量。
pub(crate) fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 依据已注册的技能元数据生成标准化 skill.yaml 文本。
/// input_schema / output_schema 直接内联为 JSON（YAML 是 JSON 的超集，合法）。
pub(crate) fn build_skill_yaml(skill: &SkillMeta, signature_status: &str) -> String {
    let roles_json = serde_json::to_string(&skill.allowed_roles).unwrap_or_else(|_| "[]".into());
    let perms_json = serde_json::to_string(&skill.skill_types).unwrap_or_else(|_| "[]".into());
    let input_json = serde_json::to_string(&skill.input_schema).unwrap_or_else(|_| "{}".into());
    let output_json = serde_json::to_string(&skill.output_schema).unwrap_or_else(|_| "{}".into());
    format!(
        "# skill.yaml — 由 Wild AgentOS 依据已注册技能元数据生成\n\
apiVersion: agentos.dev/v1\n\
kind: Skill\n\
metadata:\n\
\x20 iri: {iri}\n\
\x20 name: {name}\n\
\x20 version: {version}\n\
\x20 category: {category}\n\
spec:\n\
\x20 description: {desc}\n\
\x20 security_level: {sec}\n\
\x20 signature_status: {sig}\n\
\x20 allowed_roles: {roles}\n\
\x20 permissions: {perms}\n\
\x20 input_schema: {input}\n\
\x20 output_schema: {output}\n",
        iri = yaml_quote(&skill.skill_iri),
        name = yaml_quote(&skill.name),
        version = yaml_quote(&skill.version),
        category = yaml_quote(&skill.category),
        desc = yaml_quote(&skill.description),
        sec = yaml_quote(&skill.security_level),
        sig = yaml_quote(signature_status),
        roles = roles_json,
        perms = perms_json,
        input = input_json,
        output = output_json,
    )
}

/// GET /api/v1/skills/manifest?iri=... — 生成并下载指定技能的 skill.yaml。
pub(crate) async fn skill_manifest_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SkillManifestQuery>,
) -> impl IntoResponse {
    match state.core.skills.get_skill(&q.iri) {
        Some(skill) => {
            let sig = state.core.skills.verify_skill_signature(&skill);
            let yaml = build_skill_yaml(&skill, sig.as_str());
            // 文件名以技能名为基，去除路径分隔符等不安全字符。
            let safe: String = skill
                .name
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let filename = if safe.is_empty() {
                "skill".to_string()
            } else {
                safe
            };
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        "application/x-yaml; charset=utf-8".to_string(),
                    ),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{}.skill.yaml\"", filename),
                    ),
                ],
                yaml,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "status": "error", "error": "技能不存在", "iri": q.iri })),
        )
            .into_response(),
    }
}

/// POST /api/v1/skills — 注册新技能（#302：仅平台管理员；技能注册表全进程共享）
pub(crate) async fn register_skill_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(skill): Json<SkillMeta>,
) -> impl IntoResponse {
    // #302: the SkillRegistry and the persisted user skills are process-global
    // (no tenant/project scope), so writes need a platform administrator.
    if let Err(e) = identity.require_platform_admin("skill registry writes") {
        return e.into_response();
    }
    if skill.skill_iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error", "error": "skill_iri 不能为空"
            })),
        )
            .into_response();
    }
    // 系统级命名空间（iri://）保留给内核内置技能，只读——不可经 API 注册或覆盖。
    if is_system_skill_iri(&skill.skill_iri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error",
                "error": "系统级内置技能（iri://）为只读，不可注册或修改；请使用 skill:// 命名空间",
            })),
        )
            .into_response();
    }
    // 走技能准入流水线（Lint→Security→Test→Publish）：签名/Schema 等门禁在流水线内统一裁决，
    // 仅当门禁放行时 publish 回调才会真正持久化并注册技能。
    use crate::tools::skill_pipeline::{run_pipeline, PipelineContext, PipelineSource};
    let iri = skill.skill_iri.clone();
    let mut ctx = PipelineContext::local(PipelineSource::Manual, identity.user_id.clone());
    record_publisher_from_identity(&mut ctx, &identity);
    let publisher = verified_publisher_tenant(&identity);
    if let Err(response) = reject_skill_iri_write(&iri, &publisher, false).await {
        return response;
    }
    // Claim the owner record before touching the registry. A losing racer
    // gets 409 and leaves the registered skill unchanged.
    let run = run_pipeline(
        &state.core.skills,
        &skill,
        &ctx,
        Box::new(|_| Ok("admission accepted".into())),
    );
    #[cfg(test)]
    await_admission_write_barrier().await;
    if let Err(error) = append_pipeline_run_async(run.clone()).await {
        return pipeline_write_response(error);
    }
    if run.published {
        if let Err(error) = persist_admitted_skill(&state.core.skills, &skill) {
            return pipeline_store_failure(error);
        }
    }

    let sig_status = state.core.skills.verify_skill_signature(&skill);
    let code = if run.published {
        StatusCode::CREATED
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (
        code,
        Json(json!({
            "status": if run.published { "ok" } else { "error" },
            "error": if run.published { Value::Null } else { json!(run.summary) },
            "skill_iri": iri,
            "signature_status": sig_status.as_str(),
            "gate_passed": run.gate_passed,
            "published": run.published,
            "registered_by": identity.user_id,
            "tenant_id": identity.tenant_id,
            "pipeline_run": run,
        })),
    )
        .into_response()
}

/// DELETE /api/v1/skills?iri=... — 删除应用级技能（#302：仅平台管理员）。
/// 系统级内置技能（iri://）只读，拒绝删除。
pub(crate) async fn delete_skill_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(q): Query<SkillManifestQuery>,
) -> impl IntoResponse {
    // #302: process-global skill registry; platform administrator only.
    if let Err(e) = identity.require_platform_admin("skill registry writes") {
        return e.into_response();
    }
    if q.iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error", "error": "iri 不能为空"
            })),
        )
            .into_response();
    }
    if is_system_skill_iri(&q.iri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error",
                "error": "系统级内置技能（iri://）为只读，不可删除",
            })),
        )
            .into_response();
    }
    let removed_mem = state.core.skills.remove_skill(&q.iri);
    let removed_disk = delete_user_skill(&q.iri).unwrap_or(false);
    if !removed_mem && !removed_disk {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "status": "error", "error": "技能未找到（检查 iri）"
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "skill_iri": q.iri,
            "deleted_by": identity.user_id,
        })),
    )
        .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────
// Git 技能导入
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct GitImportRequest {
    /// Git 仓库 URL（https:// 或 git@）。
    repo_url: String,
    /// 分支/Tag/Commit，缺省 "main"。
    #[serde(default = "default_ref")]
    r#ref: String,
    /// 仓库内 skill.yaml 所在子目录，缺省根目录 "."。
    #[serde(default = "default_path")]
    path: String,
    // ── 下列字段为可选覆盖（优先于 skill.yaml 中同名字段） ──
    skill_iri: Option<String>,
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    category: Option<String>,
    security_level: Option<String>,
    allowed_roles: Option<Vec<String>>,
    skill_types: Option<Vec<String>>,
}

fn default_ref() -> String {
    "main".into()
}

/// Values that may be passed to `git clone`.
///
/// Git is executed without a shell, but option-looking or control-character
/// values can still change the behavior of the git process unless they are
/// validated and options are terminated explicitly.
struct ValidatedGitCloneSource {
    repo_url: String,
    git_ref: String,
}

fn validate_git_clone_source(
    repo_url: &str,
    git_ref: &str,
) -> Result<ValidatedGitCloneSource, &'static str> {
    let repo_url = repo_url.trim();
    let git_ref = git_ref.trim();

    if repo_url.is_empty() {
        return Err("repo_url 不能为空");
    }
    if repo_url.len() > 2048
        || repo_url
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        || !(repo_url.starts_with("https://") || repo_url.starts_with("git@"))
    {
        return Err("repo_url 必须是无空白字符的 https:// 或 git@ 仓库地址");
    }

    // `git clone --branch <ref>` must not receive option-like or malformed
    // refnames. This matches Git's refname restrictions for the branch/tag
    // names supported by this endpoint.
    if git_ref.is_empty()
        || git_ref.len() > 255
        || git_ref.starts_with('-')
        || git_ref.starts_with('/')
        || git_ref.ends_with('/')
        || git_ref.ends_with('.')
        || git_ref.ends_with(".lock")
        || git_ref.contains("..")
        || git_ref.contains("//")
        || git_ref.contains("@{")
        || git_ref
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || "~^:?*[\\".contains(ch))
    {
        return Err("ref 必须是有效且不以 - 开头的 Git 分支或标签名");
    }

    Ok(ValidatedGitCloneSource {
        repo_url: repo_url.to_owned(),
        git_ref: git_ref.to_owned(),
    })
}
pub(crate) fn normalize_git_skill_subpath(path: &str) -> Result<std::path::PathBuf, &'static str> {
    let requested_path = path.trim();
    if requested_path.is_empty() || requested_path == "." || requested_path == "/" {
        return Ok(std::path::PathBuf::new());
    }
    let path = std::path::Path::new(requested_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err("技能目录路径必须是仓库内的相对路径，且不能包含 ..");
    }
    Ok(path.to_path_buf())
}

fn default_path() -> String {
    ".".into()
}

/// 从 skill.yaml 文本中解析扁平化 key→value 映射（支持 metadata/spec 两级）。
/// 不依赖任何外部 YAML 库，直接按行分析。
pub(crate) fn parse_skill_yaml_text(yaml: &str) -> HashMap<String, String> {
    let mut flat: HashMap<String, String> = HashMap::new();
    let mut section = String::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some(colon_pos) = trimmed.find(':') {
            let key = trimmed[..colon_pos].trim().to_string();
            let value_raw = trimmed[colon_pos + 1..].trim().to_string();
            if indent == 0 {
                if value_raw.is_empty() {
                    section = key;
                } else {
                    flat.insert(key, yaml_unquote(&value_raw));
                }
            } else {
                let full_key = if section.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", section, key)
                };
                if !value_raw.is_empty() {
                    flat.insert(full_key, yaml_unquote(&value_raw));
                }
            }
        }
    }
    flat
}

fn yaml_unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].replace("\\\"", "\"").replace("\\'", "'")
    } else {
        s.to_string()
    }
}

/// 从 Git 仓库 URL 派生默认 skill IRI。
/// 例：https://github.com/org/repo.git → skill://org/repo
pub(crate) fn iri_from_git_url(url: &str) -> String {
    let base = url.trim_end_matches(".git");
    let without_proto: String = if let Some(rest) = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
    {
        rest.to_string()
    } else if let Some(rest) = base.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else {
        base.to_string()
    };
    let parts: Vec<&str> = without_proto.trim_matches('/').split('/').collect();
    if parts.len() >= 2 {
        format!(
            "skill://{}/{}",
            parts[parts.len() - 2],
            parts[parts.len() - 1]
        )
    } else {
        format!("skill://repo/{}", without_proto.replace('/', "-"))
    }
}

/// POST /api/v1/skills/import-git — 从 Git 仓库导入技能。
/// #302：需要平台管理员（技能注册表全进程共享）。
pub(crate) async fn import_git_skill_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<GitImportRequest>,
) -> impl IntoResponse {
    // #302: process-global skill registry; platform administrator only.
    if let Err(e) = identity.require_platform_admin("skill registry writes") {
        return e.into_response();
    }
    // When the caller names the IRI, reject a foreign owner before cloning.
    // A test that supplies an owned IRI and an unusable repository URL stays
    // 409; deleting this check lets the clone fail instead.
    if let Some(skill_iri) = req
        .skill_iri
        .as_deref()
        .filter(|skill_iri| !skill_iri.is_empty())
    {
        if let Err(response) =
            reject_skill_iri_write(skill_iri, &verified_publisher_tenant(&identity), false).await
        {
            return response;
        }
    }
    let git_source = match validate_git_clone_source(&req.repo_url, &req.r#ref) {
        Ok(source) => source,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "error": error })),
            )
                .into_response();
        }
    };
    let relative_path = match normalize_git_skill_subpath(&req.path) {
        Ok(path) => path,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": error,
                })),
            )
                .into_response();
        }
    };

    // 1. All options precede `--`, which terminates option parsing before the
    // validated repository URL. The ref is passed as a distinct argv value.
    let clone_dir = std::env::temp_dir().join(format!("waos-skill-{}", uuid::Uuid::new_v4()));
    let mut output = match tokio::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            git_source.git_ref.as_str(),
            "--single-branch",
            "--",
            git_source.repo_url.as_str(),
            clone_dir.to_str().unwrap_or("/tmp/waos-skill-clone"),
        ])
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error": format!("无法执行 git clone：{error}"),
                })),
            )
                .into_response();
        }
    };

    // cleanup helper (best-effort; ignore errors)
    let cleanup = |dir: &std::path::Path| {
        let _ = std::fs::remove_dir_all(dir);
    };

    if !output.status.success() && git_source.git_ref == "main" {
        cleanup(&clone_dir);
        output = match tokio::process::Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--",
                git_source.repo_url.as_str(),
                clone_dir.to_string_lossy().as_ref(),
            ])
            .output()
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "status": "error",
                        "error": format!("无法执行 git clone：{error}"),
                    })),
                )
                    .into_response();
            }
        };
    }
    if !output.status.success() {
        cleanup(&clone_dir);
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "status": "error",
                "error": "Git 仓库克隆失败，请检查仓库地址、分支和访问权限",
            })),
        )
            .into_response();
    }

    // 2. 读取并校验仓库内 skill.yaml，不允许路径越界或静默回退为占位技能。
    let clone_root = match std::fs::canonicalize(&clone_dir) {
        Ok(path) => path,
        Err(error) => {
            cleanup(&clone_dir);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error": format!("无法读取克隆目录：{error}"),
                })),
            )
                .into_response();
        }
    };
    let skill_yaml_path = clone_dir.join(relative_path).join("skill.yaml");
    let canonical_yaml = match std::fs::canonicalize(&skill_yaml_path) {
        Ok(path) if path.starts_with(&clone_root) => path,
        _ => {
            cleanup(&clone_dir);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": "指定目录中未找到 skill.yaml",
                })),
            )
                .into_response();
        }
    };
    let yaml_text = match std::fs::read_to_string(&canonical_yaml) {
        Ok(text) => text,
        Err(error) => {
            cleanup(&clone_dir);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": format!("skill.yaml 读取失败：{error}"),
                })),
            )
                .into_response();
        }
    };
    let yaml_fields = parse_skill_yaml_text(&yaml_text);

    // 3. 合并字段（请求体优先）。
    let skill_iri = req
        .skill_iri
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.iri").cloned())
        .unwrap_or_else(|| iri_from_git_url(&req.repo_url));

    let name = req
        .name
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.name").cloned())
        .unwrap_or_else(|| {
            skill_iri
                .split('/')
                .next_back()
                .unwrap_or("unnamed")
                .to_string()
        });

    let description = req
        .description
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("spec.description").cloned())
        .unwrap_or_default();

    let version = req
        .version
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.version").cloned())
        .unwrap_or_else(|| "1.0.0".into());

    let category = req
        .category
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.category").cloned())
        .unwrap_or_else(|| "application".into());

    let security_level = req
        .security_level
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("spec.security_level").cloned())
        .unwrap_or_else(|| "normal".into());

    let allowed_roles = req.allowed_roles.unwrap_or_else(|| {
        // 尝试从 yaml 字段解析 JSON 数组
        yaml_fields
            .get("spec.allowed_roles")
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .unwrap_or_else(|| vec!["DA".into()])
    });

    let skill_types = req.skill_types.unwrap_or_else(|| {
        yaml_fields
            .get("spec.permissions")
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .unwrap_or_default()
    });

    if skill_iri.is_empty() {
        cleanup(&clone_dir);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "无法确定 skill_iri，请手动填写" })),
        )
            .into_response();
    }

    let skill = SkillMeta {
        skill_iri: skill_iri.clone(),
        name: name.clone(),
        description,
        version,
        category,
        security_level,
        allowed_roles,
        input_schema: serde_json::Value::Object(Default::default()),
        output_schema: serde_json::Value::Object(Default::default()),
        compiled_template: String::new(),
        signature: None,
        signature_algorithm: None,
        input_mapping: HashMap::new(),
        output_mapping: HashMap::new(),
        skill_types,
    };

    // 走技能准入流水线（Git 来源）：Security/Test 阶段会对克隆目录做敏感扫描与示例夹具校验，
    // 因此流水线必须在 cleanup 清理克隆目录之前执行。
    use crate::tools::skill_pipeline::{run_pipeline, PipelineContext, PipelineSource};
    let mut ctx = PipelineContext {
        source: PipelineSource::Git,
        triggered_by: identity.user_id.clone(),
        repo_url: Some(req.repo_url.trim().to_string()),
        clone_dir: Some(clone_dir.clone()),
        sub_path: req.path.clone(),
        require_package: true,
        // Git imports are the explicit tenant publication channel. The
        // pipeline refuses system visibility and persists only after all gates.
        visibility: crate::tools::skill_pipeline::SkillVisibility::Tenant,
        tenant_promotion_review: Some(TenantPromotionReview::completed(identity.user_id.clone())),
        publisher_tenant_id: None,
        publisher_project_id: None,
    };
    record_publisher_from_identity(&mut ctx, &identity);
    if let Err(response) =
        reject_skill_iri_write(&skill_iri, &verified_publisher_tenant(&identity), false).await
    {
        cleanup(&clone_dir);
        return response;
    }
    let run = run_pipeline(
        &state.core.skills,
        &skill,
        &ctx,
        Box::new(|_| Ok("admission accepted".into())),
    );
    #[cfg(test)]
    await_admission_write_barrier().await;
    if let Err(error) = append_pipeline_run_async(run.clone()).await {
        cleanup(&clone_dir);
        return pipeline_write_response(error);
    }
    if run.published {
        if let Err(error) = persist_admitted_skill(&state.core.skills, &skill) {
            cleanup(&clone_dir);
            return pipeline_store_failure(error);
        }
    }

    let sig_status = state.core.skills.verify_skill_signature(&skill);
    cleanup(&clone_dir);

    let code = if run.published {
        StatusCode::CREATED
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (
        code,
        Json(json!({
            "status": if run.published { "ok" } else { "error" },
            "error": if run.published { Value::Null } else { json!(run.summary) },
            "skill_iri": skill_iri,
            "name": name,
            "git_cloned": true,
            "git_stderr": "",
            "yaml_fields_found": yaml_fields.len(),
            "signature_status": sig_status.as_str(),
            "gate_passed": run.gate_passed,
            "published": run.published,
            "registered_by": identity.user_id,
            "pipeline_run": run,
        })),
    )
        .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────
// 技能准入流水线：运行记录查询 + 重跑
// ──────────────────────────────────────────────────────────────────────────────

/// GET /api/v1/skills/pipeline-runs 的查询参数。
#[derive(Debug, Deserialize)]
pub(crate) struct PipelineRunsQuery {
    /// 可选：按 skill_iri 过滤（详情弹窗按单个技能拉取其历史）。
    iri: Option<String>,
    /// 可选：仅返回最近 N 条（默认全部，受服务端上限约束）。
    limit: Option<usize>,
}

/// GET /api/v1/skills/pipeline-runs — 查询技能准入流水线运行记录（只读，无需鉴权）。
/// 匿名响应去掉发布者 tenant/project；磁盘上的记录仍保留这两项。
fn redact_pipeline_run(run: &crate::tools::skill_pipeline::PipelineRun) -> Value {
    let mut value = serde_json::to_value(run).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("publisher_tenant_id");
        object.remove("publisher_project_id");
    }
    value
}

pub(crate) async fn list_pipeline_runs_handler(
    Query(q): Query<PipelineRunsQuery>,
) -> impl IntoResponse {
    let mut runs = match tokio::task::spawn_blocking(pipeline_runs_snapshot).await {
        Ok(Ok(runs)) => runs,
        Ok(Err(error)) => return pipeline_store_failure(error),
        Err(error) => return pipeline_store_failure(error),
    };
    if let Some(iri) = q.iri.filter(|s| !s.is_empty()) {
        runs.retain(|r| r.skill_iri == iri);
    }
    if let Some(limit) = q.limit {
        runs.truncate(limit);
    }
    let runs: Vec<Value> = runs.iter().map(redact_pipeline_run).collect();
    Json(json!({
        "count": runs.len(),
        "runs": runs,
    }))
    .into_response()
}

/// POST /api/v1/skills/pipeline-rerun 的请求体。
#[derive(Debug, Deserialize)]
pub(crate) struct PipelineRerunRequest {
    skill_iri: String,
}

/// POST /api/v1/skills/pipeline-rerun — 对已注册的应用级技能重跑准入流水线（#302：仅平台管理员）。
pub(crate) async fn pipeline_rerun_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<PipelineRerunRequest>,
) -> impl IntoResponse {
    // #302: process-global skill registry; platform administrator only.
    if let Err(e) = identity.require_platform_admin("skill registry writes") {
        return e.into_response();
    }
    if req.skill_iri.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error", "error": "skill_iri 不能为空"
            })),
        )
            .into_response();
    }
    if is_system_skill_iri(&req.skill_iri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error", "error": "系统级内置技能（iri://）为只读，不支持重跑流水线",
            })),
        )
            .into_response();
    }
    if let Err(response) =
        reject_skill_iri_write(&req.skill_iri, &verified_publisher_tenant(&identity), false).await
    {
        return response;
    }
    // 以持久化的用户态技能为准（含完整 schema/template/签名）。
    let skill = match load_user_skills()
        .into_iter()
        .find(|s| s.skill_iri == req.skill_iri)
    {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "status": "error", "error": "技能未找到（检查 skill_iri）"
                })),
            )
                .into_response();
        }
    };

    use crate::tools::skill_pipeline::{run_pipeline, PipelineContext, PipelineSource};
    let mut ctx = PipelineContext::local(PipelineSource::Rerun, identity.user_id.clone());
    record_publisher_from_identity(&mut ctx, &identity);
    let run = run_pipeline(
        &state.core.skills,
        &skill,
        &ctx,
        Box::new(|_| Ok("admission accepted".into())),
    );
    #[cfg(test)]
    await_admission_write_barrier().await;
    if let Err(error) = append_pipeline_run_async(run.clone()).await {
        return pipeline_write_response(error);
    }
    if run.published {
        if let Err(error) = persist_admitted_skill(&state.core.skills, &skill) {
            return pipeline_store_failure(error);
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "status": if run.published { "ok" } else { "error" },
            "error": if run.published { Value::Null } else { json!(run.summary) },
            "skill_iri": req.skill_iri,
            "gate_passed": run.gate_passed,
            "published": run.published,
            "pipeline_run": run,
        })),
    )
        .into_response()
}

#[cfg(test)]
// Test-only lock held for the whole test by design (serializes process-global env/state);
// code under test never takes it, so holding it across `.await` cannot deadlock.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use crate::tools::prompt_registry::PromptRegistry;
    use axum::{
        http::StatusCode,
        routing::{get, post},
        Router,
    };
    use serde_json::Value;
    use tower::ServiceExt;

    use super::super::{api_gov::ApiUsageState, TEST_ENV_LOCK};

    // ── 辅助：最小 AppState ───────────────────────────────────────────────────
    fn make_state(tmp: &std::path::Path) -> Arc<AppState> {
        let l0 = tmp.join("l0");
        std::fs::create_dir_all(&l0).unwrap();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: l0.to_str().unwrap().to_string(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            crate::gateway::UnifiedGateway::new(&crate::config::GatewaySettings {
                base_url: "http://localhost".into(),
                api_key: String::new(),
                default_model: "test-model".into(),
                timeout_seconds: 30,
                max_retries: 1,
                retry_base_ms: 500,
                use_responses_api: false,
                model_mapping: std::collections::HashMap::new(),
            })
            .unwrap(),
        );
        let kg_store = Arc::new(oxigraph::store::Store::new().unwrap());
        Arc::new(AppState {
            core,
            gateway,
            kg_store,
            config_info: Arc::new(tokio::sync::RwLock::new(serde_json::json!({}))),
            agents_info: serde_json::json!({ "count": 0, "agents": [] }),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
            prompts: Arc::new(PromptRegistry::new()),
            kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![])),
            vector_store: Arc::new(arc_swap::ArcSwapOption::empty()),
            blob_store: None,
            task_executor: None,
            batch_manager: None,
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
            online_corpus_jobs: Arc::new(tokio::sync::RwLock::new(vec![])),
            online_corpus_queue_capacity: 10,
            invocations: crate::api::http::invocations::InvocationsRuntime::unavailable(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    fn sample_skill() -> SkillMeta {
        SkillMeta {
            skill_iri: "skill://test/hello".into(),
            name: "Hello World".into(),
            description: "测试技能".into(),
            version: "1.0.0".into(),
            category: "test".into(),
            security_level: "standard".into(),
            allowed_roles: vec!["DA".into()],
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            compiled_template: "{{x}}".into(),
            signature: None,
            signature_algorithm: None,
            input_mapping: Default::default(),
            output_mapping: Default::default(),
            skill_types: vec![],
        }
    }

    #[test]
    fn tenant_publish_status_requires_latest_passing_tenant_run() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("tenant_publish_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let registry = crate::tools::skill_registry::SkillRegistry::new();
        let skill = sample_skill();
        let mut ctx = crate::tools::skill_pipeline::PipelineContext {
            source: crate::tools::skill_pipeline::PipelineSource::Git,
            triggered_by: "tester".into(),
            repo_url: None,
            clone_dir: Some(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/skills/package-green"),
            ),
            sub_path: ".".into(),
            require_package: true,
            visibility: crate::tools::skill_pipeline::SkillVisibility::Tenant,
            tenant_promotion_review: Some(TenantPromotionReview::completed("reviewer:tester")),
            publisher_tenant_id: Some("tenant-a".into()),
            publisher_project_id: Some("project-a".into()),
        };
        let published = crate::tools::skill_pipeline::run_pipeline(
            &registry,
            &skill,
            &ctx,
            Box::new(|_| Ok("published".into())),
        );
        append_pipeline_run(&published).unwrap();
        assert!(is_tenant_published_skill(
            &skill.skill_iri,
            "tenant-a",
            "project-b"
        ));
        assert!(
            !is_tenant_published_skill(&skill.skill_iri, "tenant-b", "project-b"),
            "another tenant must not inherit this publication"
        );

        ctx.visibility = crate::tools::skill_pipeline::SkillVisibility::Session;
        ctx.require_package = false;
        ctx.clone_dir = None;
        ctx.tenant_promotion_review = None;
        let session_update = crate::tools::skill_pipeline::run_pipeline(
            &registry,
            &skill,
            &ctx,
            Box::new(|_| Ok("session update".into())),
        );
        append_pipeline_run(&session_update).unwrap();
        assert!(
            !is_tenant_published_skill(&skill.skill_iri, "tenant-a", "project-a"),
            "a later session update must revoke external publication"
        );

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    fn sample_pipeline_run(index: usize) -> crate::tools::skill_pipeline::PipelineRun {
        use crate::tools::skill_pipeline::{PipelineRun, PipelineSource, SkillVisibility};
        PipelineRun {
            run_id: format!("run-{index}"),
            skill_iri: format!("skill://test/parallel-{index}"),
            skill_name: "parallel".into(),
            version: "1.0.0".into(),
            source: PipelineSource::Manual,
            visibility: SkillVisibility::Tenant,
            tenant_promotion_review: None,
            triggered_by: "tester".into(),
            repo_url: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            duration_ms: 1,
            stages: vec![],
            gate_passed: true,
            published: true,
            summary: "published".into(),
            publisher_tenant_id: Some("tenant-a".into()),
            publisher_project_id: Some("project-a".into()),
        }
    }

    /// #430: concurrent appends keep every row, and a corrupt file is not rewritten.
    #[test]
    fn pipeline_runs_keep_concurrent_appends_and_do_not_rewrite_a_corrupt_file() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_DATA_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
                    None => std::env::remove_var("AGENTOS_DATA_DIR"),
                }
            }
        }
        let _restore = Restore(previous);

        const N: usize = 40;
        std::thread::scope(|scope| {
            for index in 0..N {
                scope.spawn(move || {
                    append_pipeline_run(&sample_pipeline_run(index)).unwrap();
                });
            }
        });
        let stored: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("pipeline_runs.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.len(), N);
        let ids: std::collections::HashSet<_> = stored
            .iter()
            .map(|run| run["run_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), N);

        let path = dir.path().join("pipeline_runs.json");
        let garbage = b"[{";
        std::fs::write(&path, garbage).unwrap();
        assert!(append_pipeline_run(&sample_pipeline_run(99)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), garbage);
        assert!(!is_tenant_published_skill(
            "skill://test/parallel-0",
            "tenant-a",
            "project-a"
        ));
    }

    #[test]
    fn skill_published_for_tenant_uses_the_stored_owner() {
        use crate::tools::skill_pipeline::SkillVisibility;
        let owner = SkillIriOwnerRecord {
            skill_iri: "skill://tenant-a/owned".into(),
            tenant_id: "tenant-a".into(),
            project_id: "project-a".into(),
            published: true,
            gate_passed: true,
            visibility: SkillVisibility::Tenant,
        };
        assert!(skill_published_for_tenant(
            Some(&owner),
            "tenant-a",
            "project-b"
        ));
        assert!(!skill_published_for_tenant(
            Some(&owner),
            "tenant-b",
            "project-b"
        ));
    }

    /// #431: flooding past the run cap must not drop the owner or exposure.
    #[test]
    fn owner_record_survives_run_history_truncation() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_DATA_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
                    None => std::env::remove_var("AGENTOS_DATA_DIR"),
                }
            }
        }
        let _restore = Restore(previous);

        let mut owned = sample_pipeline_run(0);
        owned.skill_iri = "skill://tenant-a/secret-skill".into();
        append_pipeline_run(&owned).unwrap();
        for index in 0..PIPELINE_RUNS_CAP {
            let mut junk = sample_pipeline_run(index + 1);
            junk.skill_iri = format!("skill://tenant-b/junk-{index}");
            junk.publisher_tenant_id = Some("tenant-b".into());
            junk.publisher_project_id = Some("project-b".into());
            append_pipeline_run(&junk).unwrap();
        }
        let stored: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("pipeline_runs.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.len(), PIPELINE_RUNS_CAP);
        assert!(
            stored
                .iter()
                .all(|run| run["skill_iri"] != "skill://tenant-a/secret-skill"),
            "the owner run was not evicted, so this does not test the owner file"
        );
        assert!(is_tenant_published_skill(
            "skill://tenant-a/secret-skill",
            "tenant-a",
            "project-a"
        ));
        let mut hostile = owned.clone();
        hostile.run_id = "hostile".into();
        hostile.publisher_tenant_id = Some("tenant-b".into());
        hostile.publisher_project_id = Some("project-b".into());
        let error = append_pipeline_run(&hostile).unwrap_err();
        assert!(matches!(error, PipelineWriteError::OwnedByAnotherTenant));
        let owners: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("skill_iri_owners.json")).unwrap(),
        )
        .unwrap();
        assert!(owners.iter().any(|owner| {
            owner["skill_iri"] == "skill://tenant-a/secret-skill"
                && owner["tenant_id"] == "tenant-a"
        }));
    }

    /// Migration from admission history happens once. Deleting the owner file
    /// afterwards must not rebuild it from the remaining runs.
    #[test]
    fn owner_file_is_not_rebuilt_after_migration() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_DATA_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
                    None => std::env::remove_var("AGENTOS_DATA_DIR"),
                }
            }
        }
        let _restore = Restore(previous);

        let mut owned = sample_pipeline_run(0);
        owned.skill_iri = "skill://tenant-a/secret-skill".into();
        let path = pipeline_runs_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&[owned]).unwrap()).unwrap();
        assert!(!skill_iri_owners_path().exists());
        assert!(!owners_migration_marker_path().exists());

        let migrated = admission_snapshot().unwrap();
        assert_eq!(
            migrated.owners["skill://tenant-a/secret-skill"].tenant_id,
            "tenant-a"
        );
        assert!(owners_migration_marker_path().exists());

        std::fs::remove_file(skill_iri_owners_path()).unwrap();
        std::fs::write(&path, b"[]").unwrap();
        assert!(admission_snapshot().is_err());
        let mut claim = sample_pipeline_run(1);
        claim.skill_iri = "skill://tenant-a/secret-skill".into();
        claim.publisher_tenant_id = Some("platform".into());
        claim.publisher_project_id = Some("ops".into());
        assert!(append_pipeline_run(&claim).is_err());
        assert!(!skill_iri_owners_path().exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"[]");
    }

    #[test]
    fn foreign_tenant_append_is_rejected_and_leaves_the_file_unchanged() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_DATA_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
                    None => std::env::remove_var("AGENTOS_DATA_DIR"),
                }
            }
        }
        let _restore = Restore(previous);

        let mut owned = sample_pipeline_run(1);
        owned.skill_iri = "skill://tenant-a/owned".into();
        append_pipeline_run(&owned).unwrap();
        let before = std::fs::read(dir.path().join("pipeline_runs.json")).unwrap();

        let mut hostile = owned.clone();
        hostile.run_id = "hostile".into();
        hostile.publisher_tenant_id = Some("tenant-b".into());
        hostile.publisher_project_id = Some("project-b".into());
        let error = append_pipeline_run(&hostile).unwrap_err();
        assert!(matches!(error, PipelineWriteError::OwnedByAnotherTenant));
        assert_eq!(
            std::fs::read(dir.path().join("pipeline_runs.json")).unwrap(),
            before
        );
        assert!(is_tenant_published_skill(
            &owned.skill_iri,
            "tenant-a",
            "project-a"
        ));
        assert!(!is_tenant_published_skill(
            &owned.skill_iri,
            "tenant-b",
            "project-b"
        ));
    }

    #[test]
    fn runs_without_publisher_tenant_are_counted() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_DATA_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
                    None => std::env::remove_var("AGENTOS_DATA_DIR"),
                }
            }
        }
        let _restore = Restore(previous);

        let mut unscoped = sample_pipeline_run(7);
        unscoped.publisher_tenant_id = None;
        unscoped.publisher_project_id = None;
        append_pipeline_run(&unscoped).unwrap();
        let loaded = pipeline_runs_snapshot().unwrap();
        let missing = loaded
            .iter()
            .filter(|run| publisher_tenant(run).is_none())
            .count();
        assert_eq!(missing, 1);
        assert_eq!(runs_without_publisher_tenant_count(), missing);
        assert!(!is_tenant_published_skill(
            &unscoped.skill_iri,
            "tenant-a",
            "project-a"
        ));
    }

    /// A non-atomic overwrite of pipeline_runs.json is visible to a reader as
    /// invalid JSON. Atomic rename keeps every read parseable.
    #[test]
    fn pipeline_runs_concurrent_readers_never_see_a_partial_file() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_DATA_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", dir.path());
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("AGENTOS_DATA_DIR", value),
                    None => std::env::remove_var("AGENTOS_DATA_DIR"),
                }
            }
        }
        let _restore = Restore(previous);

        let torn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|scope| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    let torn = torn.clone();
                    let stop = stop.clone();
                    scope.spawn(move || {
                        let path = pipeline_runs_path();
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                            match std::fs::read(&path) {
                                Ok(bytes) => {
                                    if serde_json::from_slice::<Vec<Value>>(&bytes).is_err() {
                                        torn.store(true, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(_) => {
                                    torn.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                    })
                })
                .collect();
            let writers: Vec<_> = (0..8)
                .map(|writer| {
                    scope.spawn(move || {
                        for index in 0..8 {
                            append_pipeline_run(&sample_pipeline_run(writer * 8 + index)).unwrap();
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            for reader in readers {
                reader.join().unwrap();
            }
        });
        assert!(
            !torn.load(std::sync::atomic::Ordering::Relaxed),
            "a reader observed a partial pipeline_runs.json"
        );
        let stored: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("pipeline_runs.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.len(), 64);
    }

    // ── 纯函数单元测试 ────────────────────────────────────────────────────────

    #[test]
    fn test_yaml_quote_plain() {
        assert_eq!(yaml_quote("hello"), "\"hello\"");
    }

    #[test]
    fn test_yaml_quote_with_quotes() {
        // 双引号与反斜杠应被转义
        assert_eq!(yaml_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(yaml_quote(r"back\slash"), r#""back\\slash""#);
    }

    #[test]
    fn test_build_skill_yaml_contains_fields() {
        let skill = sample_skill();
        let yaml = build_skill_yaml(&skill, "unsigned");
        assert!(yaml.contains("skill://test/hello"), "should contain IRI");
        assert!(yaml.contains("Hello World"), "should contain name");
        assert!(yaml.contains("1.0.0"), "should contain version");
        assert!(yaml.contains("unsigned"), "should contain signature_status");
        assert!(
            yaml.contains("allowed_roles:"),
            "should contain allowed_roles key"
        );
        assert!(yaml.contains("DA"), "should contain DA role");
    }

    #[test]
    fn test_iri_from_git_url_https() {
        assert_eq!(
            iri_from_git_url("https://github.com/myorg/myrepo.git"),
            "skill://myorg/myrepo"
        );
    }

    #[test]
    fn test_iri_from_git_url_https_no_git_suffix() {
        assert_eq!(
            iri_from_git_url("https://gitee.com/acme/demo-skill"),
            "skill://acme/demo-skill"
        );
    }

    #[test]
    fn test_iri_from_git_url_ssh() {
        assert_eq!(
            iri_from_git_url("git@github.com:myorg/myrepo.git"),
            "skill://myorg/myrepo"
        );
    }

    #[test]
    fn test_parse_skill_yaml_text_flat() {
        let yaml = "\
skill_iri: \"skill://test/demo\"\n\
name: \"演示技能\"\n\
version: \"2.0.0\"\n\
";
        let map = parse_skill_yaml_text(yaml);
        assert_eq!(
            map.get("skill_iri").map(|s| s.as_str()),
            Some("skill://test/demo")
        );
        assert_eq!(map.get("name").map(|s| s.as_str()), Some("演示技能"));
        assert_eq!(map.get("version").map(|s| s.as_str()), Some("2.0.0"));
    }

    #[test]
    fn test_parse_skill_yaml_text_nested() {
        // 两级嵌套（metadata / spec），键应被扁平化为 "section.key"。
        // 注意：用 concat! 保留缩进——字符串行尾 `\` 会连同下一行前导空格一并吞掉。
        let yaml = concat!(
            "metadata:\n",
            "  iri: \"skill://test/nested\"\n",
            "  name: \"嵌套技能\"\n",
            "  version: \"3.1.0\"\n",
            "  category: \"application\"\n",
            "spec:\n",
            "  description: \"支持嵌套解析\"\n",
            "  security_level: \"normal\"\n",
        );
        let map = parse_skill_yaml_text(yaml);
        assert_eq!(
            map.get("metadata.iri").map(|s| s.as_str()),
            Some("skill://test/nested")
        );
        assert_eq!(
            map.get("metadata.name").map(|s| s.as_str()),
            Some("嵌套技能")
        );
        assert_eq!(
            map.get("metadata.version").map(|s| s.as_str()),
            Some("3.1.0")
        );
        assert_eq!(
            map.get("metadata.category").map(|s| s.as_str()),
            Some("application")
        );
        assert_eq!(
            map.get("spec.description").map(|s| s.as_str()),
            Some("支持嵌套解析")
        );
        assert_eq!(
            map.get("spec.security_level").map(|s| s.as_str()),
            Some("normal")
        );
    }

    #[test]
    fn test_normalize_git_skill_subpath_accepts_repository_paths() {
        assert_eq!(
            normalize_git_skill_subpath("/").unwrap(),
            std::path::PathBuf::new()
        );
        assert_eq!(
            normalize_git_skill_subpath("skills/pdf-parser").unwrap(),
            std::path::PathBuf::from("skills/pdf-parser")
        );
    }

    #[test]
    fn test_normalize_git_skill_subpath_rejects_escape_paths() {
        assert!(normalize_git_skill_subpath("../outside").is_err());
        assert!(normalize_git_skill_subpath("skills/../../outside").is_err());
        assert!(normalize_git_skill_subpath("/tmp/outside").is_err());
    }

    #[test]
    fn test_validate_git_clone_source_accepts_supported_safe_values() {
        let source =
            validate_git_clone_source("https://github.com/skaiy/wild_agentos.git", "release/v0.6")
                .unwrap();
        assert_eq!(source.repo_url, "https://github.com/skaiy/wild_agentos.git");
        assert_eq!(source.git_ref, "release/v0.6");

        assert!(validate_git_clone_source("git@github.com:skaiy/wild_agentos.git", "main").is_ok());
    }

    #[test]
    fn test_validate_git_clone_source_rejects_command_and_ref_injection_inputs() {
        assert!(validate_git_clone_source("--upload-pack=evil", "main").is_err());
        assert!(validate_git_clone_source("file:///tmp/skill", "main").is_err());
        assert!(validate_git_clone_source(
            "https://github.com/org/repo\n--upload-pack=evil",
            "main"
        )
        .is_err());
        assert!(
            validate_git_clone_source("https://github.com/org/repo", "--upload-pack=evil").is_err()
        );
        assert!(
            validate_git_clone_source("https://github.com/org/repo", "topic\n--config=x").is_err()
        );
    }

    // ── HTTP 集成测试 ─────────────────────────────────────────────────────────

    /// GET /api/v1/skills/manifest?iri=skill://test/hello → 200 + application/x-yaml
    #[tokio::test]
    async fn test_manifest_200_known_skill() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("manifest_200_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        state.core.skills.register_skill(sample_skill());

        let router = Router::new()
            .route("/api/v1/skills/manifest", get(skill_manifest_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/skills/manifest?iri=skill://test/hello")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("yaml"),
            "content-type should be yaml, got: {ct}"
        );
        let cd = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cd.contains("attachment"),
            "should be an attachment download"
        );

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// GET /api/v1/skills/manifest?iri=skill://notfound/x → 404
    #[tokio::test]
    async fn test_manifest_404_unknown_skill() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("manifest_404_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);

        let router = Router::new()
            .route("/api/v1/skills/manifest", get(skill_manifest_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/skills/manifest?iri=skill://notfound/x")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// POST /api/v1/skills/import-git 无 JWT → 401（严格模式；#302 起需平台管理员，
    /// 无 verified claims 时先返回 401，原为 403）
    #[tokio::test]
    async fn test_import_git_401_no_jwt() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("importgit_403_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");

        let state = make_state(&tmp);

        let router = Router::new()
            .route("/api/v1/skills/import-git", post(import_git_skill_handler))
            .with_state(state);

        let body =
            serde_json::json!({ "repo_url": "https://github.com/test/repo.git" }).to_string();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/skills/import-git")
            .header("content-type", "application/json")
            // 故意不带 JWT
            .body(axum::body::Body::from(body))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        std::env::remove_var("AGENTOS_DATA_DIR");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// POST /api/v1/skills/import-git 的 dev-only X-Identity 模拟带 DA 角色但 repo_url 为空 → 400
    #[tokio::test]
    async fn test_import_git_dev_only_400_empty_url() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("importgit_400_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        let strict_mode = std::env::var_os("AGENTOS_AUTH_STRICT");
        std::env::remove_var("AGENTOS_AUTH_STRICT");

        let state = make_state(&tmp);

        let router = Router::new()
            .route("/api/v1/skills/import-git", post(import_git_skill_handler))
            .with_state(state);

        let identity = STANDARD.encode(
            serde_json::json!({"user_id": "admin", "tenant_id": "t-test", "roles": ["DA"]})
                .to_string(),
        );
        // repo_url 为空字符串
        let body = serde_json::json!({ "repo_url": "" }).to_string();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/skills/import-git")
            .header("content-type", "application/json")
            .header("x-identity", identity)
            .body(axum::body::Body::from(body.clone()))
            .unwrap();

        // #302: the unverified dev X-Identity DA no longer reaches the handler.
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // A verified platform administrator reaches input validation → 400.
        let _auth = platform_admin_env();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/skills/import-git")
            .header("content-type", "application/json")
            .header("authorization", platform_admin_bearer())
            .body(axum::body::Body::from(body))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        std::env::remove_var("AGENTOS_DATA_DIR");
        if let Some(value) = strict_mode {
            std::env::set_var("AGENTOS_AUTH_STRICT", value);
        }
        let _ = std::fs::remove_dir_all(tmp);
    }

    // ── 技能准入流水线集成测试 ─────────────────────────────────────────────────

    const TEST_JWT_SECRET: &str = "test-hs256-secret-at-least-32-bytes-long";

    /// #302: skill registry writes need a platform administrator.
    fn platform_admin_env() -> super::super::control_plane_route_auth_tests::EnvGuard {
        super::super::control_plane_route_auth_tests::EnvGuard::set(&[
            ("AGENTOS_AUTH_MODE", "hs256".into()),
            ("AGENTOS_JWT_SECRET", TEST_JWT_SECRET.into()),
            (
                super::super::iam::PLATFORM_ADMIN_TENANT_ENV,
                "platform".into(),
            ),
        ])
    }

    /// `Bearer` value for a verified platform-administrator JWT.
    fn platform_admin_bearer() -> String {
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &super::super::iam::JwtClaims {
                sub: "platform-admin".into(),
                tenant_id: "platform".into(),
                project_id: Some("ops".into()),
                roles: vec![super::super::iam::PLATFORM_ADMIN_ROLE.into()],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &jsonwebtoken::EncodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        )
        .unwrap();
        format!("Bearer {token}")
    }

    /// POST 一个 JSON body 到 router，返回 (状态码, 解析后的 body)。
    async fn post_json(
        router: &Router,
        uri: &str,
        body: Value,
        ident: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(id) = ident {
            b = if id.starts_with("Bearer ") {
                b.header("authorization", id)
            } else {
                b.header("x-identity", id)
            };
        }
        let req = b.body(axum::body::Body::from(body.to_string())).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    /// GET 一个 URI，返回 (状态码, 解析后的 body)。
    async fn get_json(router: &Router, uri: &str) -> (StatusCode, Value) {
        let req = axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    /// 合法技能注册 → 201 CREATED，门禁放行且已发布，运行记录持久化并可经查询接口检索。
    #[tokio::test]
    async fn test_pipeline_manual_register_ok_persists_run() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_ok_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/skills", post(register_skill_handler))
            .route(
                "/api/v1/skills/pipeline-runs",
                get(list_pipeline_runs_handler),
            )
            .with_state(state);

        let _auth = platform_admin_env();
        let ident = platform_admin_bearer();
        let skill = serde_json::json!({
            "skill_iri": "skill://platform/ok", "name": "合法技能", "description": "有效",
            "version": "1.0.0", "category": "test", "security_level": "standard",
            "allowed_roles": ["DA"], "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
        });
        let (st, body) = post_json(&router, "/api/v1/skills", skill, Some(&ident)).await;
        assert_eq!(st, StatusCode::CREATED, "合法技能应 201，body={body}");
        assert_eq!(body["gate_passed"], true);
        assert_eq!(body["published"], true);
        assert_eq!(body["pipeline_run"]["source"], "manual");
        assert_eq!(body["pipeline_run"]["skill_iri"], "skill://platform/ok");

        // 运行记录已持久化落盘。
        let disk = std::fs::read_to_string(pipeline_runs_path()).unwrap();
        assert!(disk.contains("skill://platform/ok"), "运行记录应落盘");

        // 查询接口（按 iri 过滤）应可检索到该运行。
        let (st, listed) = get_json(
            &router,
            "/api/v1/skills/pipeline-runs?iri=skill://platform/ok",
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["runs"][0]["published"], true);
        assert!(
            disk.contains("publisher_tenant_id"),
            "the admission file keeps the publisher tenant"
        );
        assert!(listed["runs"][0].get("publisher_tenant_id").is_none());
        assert!(listed["runs"][0].get("publisher_project_id").is_none());
        assert_eq!(body["pipeline_run"]["publisher_tenant_id"], "platform");

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 非法 input_schema（无法编译为 JSON Schema）→ 422，门禁拦截、未发布，
    /// 但失败运行记录仍持久化且可查询。
    #[tokio::test]
    async fn test_pipeline_manual_register_invalid_schema_422() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_422_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/skills", post(register_skill_handler))
            .route(
                "/api/v1/skills/pipeline-runs",
                get(list_pipeline_runs_handler),
            )
            .with_state(state.clone());

        let _auth = platform_admin_env();
        let ident = platform_admin_bearer();
        // type 必须是字符串/数组；此处为数字 → JSON Schema 编译失败 → Lint 阶段 Failed。
        let skill = serde_json::json!({
            "skill_iri": "skill://platform/bad", "name": "非法技能", "description": "无效",
            "version": "1.0.0", "category": "test", "security_level": "standard",
            "allowed_roles": ["DA"], "input_schema": {"type": 123},
            "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
        });
        let (st, body) = post_json(&router, "/api/v1/skills", skill, Some(&ident)).await;
        assert_eq!(
            st,
            StatusCode::UNPROCESSABLE_ENTITY,
            "非法 schema 应 422，body={body}"
        );
        assert_eq!(body["gate_passed"], false);
        assert_eq!(body["published"], false);

        // 技能不得被真正注册。
        assert!(
            state
                .core
                .skills
                .get_skill("skill://platform/bad")
                .is_none(),
            "门禁拦截后不应注册"
        );

        // 失败运行记录仍持久化，且门禁字段为 false。
        let (st, listed) = get_json(
            &router,
            "/api/v1/skills/pipeline-runs?iri=skill://platform/bad",
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["runs"][0]["gate_passed"], false);
        assert_eq!(listed["runs"][0]["published"], false);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 对已注册技能重跑流水线 → 200，来源为 rerun，且新增一条运行记录。
    #[tokio::test]
    async fn test_pipeline_rerun_ok() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_rerun_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/skills", post(register_skill_handler))
            .route(
                "/api/v1/skills/pipeline-rerun",
                post(pipeline_rerun_handler),
            )
            .route(
                "/api/v1/skills/pipeline-runs",
                get(list_pipeline_runs_handler),
            )
            .with_state(state);

        let _auth = platform_admin_env();
        let ident = platform_admin_bearer();
        let skill = serde_json::json!({
            "skill_iri": "skill://platform/rerun", "name": "可重跑技能", "description": "有效",
            "version": "1.0.0", "category": "test", "security_level": "standard",
            "allowed_roles": ["DA"], "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
        });
        let (st, _) = post_json(&router, "/api/v1/skills", skill, Some(&ident)).await;
        assert_eq!(st, StatusCode::CREATED);

        // 重跑。
        let (st, body) = post_json(
            &router,
            "/api/v1/skills/pipeline-rerun",
            serde_json::json!({"skill_iri": "skill://platform/rerun"}),
            Some(&ident),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "重跑应 200，body={body}");
        assert_eq!(body["published"], true);
        assert_eq!(body["pipeline_run"]["source"], "rerun");

        // 两条运行记录（注册 + 重跑）。
        let (st, listed) = get_json(
            &router,
            "/api/v1/skills/pipeline-runs?iri=skill://platform/rerun",
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["count"], 2);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 重跑不存在的技能 → 404。
    #[tokio::test]
    async fn test_pipeline_rerun_not_found_404() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_rerun404_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route(
                "/api/v1/skills/pipeline-rerun",
                post(pipeline_rerun_handler),
            )
            .with_state(state);

        let _auth = platform_admin_env();
        let ident = platform_admin_bearer();
        let (st, _) = post_json(
            &router,
            "/api/v1/skills/pipeline-rerun",
            serde_json::json!({"skill_iri": "skill://platform/nope"}),
            Some(&ident),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
