//! Deploy-time corpus watcher scheduling.
//!
//! A watcher registration is trusted configuration, not an HTTP request. It
//! declares the claims scope it may enqueue for, and it can only create a
//! queued job through the same idempotent store path as the authenticated API.
//! It deliberately does not run jobs: `/run` needs explicit runner input and
//! retains all staging, quality-gate, ER-approval, and materialization gates.

use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    config::{OnlineCorpusWatcherRegistration, OnlineCorpusWatcherSettings},
    isolation::IsolationClaims,
};

use super::corpus_jobs::{
    enqueue_online_corpus_job, CorpusSource, CreateOnlineCorpusJobRequest, OnlineCorpusJobState,
    OnlineCorpusJobStore,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WatcherTickReport {
    pub considered: usize,
    pub enqueued: usize,
    pub reused: usize,
    pub saturated: usize,
    pub invalid: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct WatcherCursor {
    registration_id: String,
    tenant_id: String,
    project_id: String,
    source_id: String,
    source_version: String,
}

fn watcher_cursors_path() -> PathBuf {
    super::data_dir().join("online_corpus_watcher_cursors.json")
}

fn load_watcher_cursors() -> BTreeSet<WatcherCursor> {
    std::fs::read_to_string(watcher_cursors_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_watcher_cursors(cursors: &BTreeSet<WatcherCursor>) -> Result<(), String> {
    let path = watcher_cursors_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(cursors).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

fn registration_cursor(registration: &OnlineCorpusWatcherRegistration) -> WatcherCursor {
    WatcherCursor {
        registration_id: registration.id.clone(),
        tenant_id: registration.tenant_id.clone(),
        project_id: registration.project_id.clone(),
        source_id: registration.source_id.clone(),
        source_version: registration.source_version.clone(),
    }
}

fn watcher_idempotency_key(registration: &OnlineCorpusWatcherRegistration) -> String {
    let mut digest = Sha256::new();
    digest.update(registration.tenant_id.as_bytes());
    digest.update([0]);
    digest.update(registration.project_id.as_bytes());
    digest.update([0]);
    digest.update(registration.source_id.as_bytes());
    digest.update([0]);
    digest.update(registration.source_version.as_bytes());
    format!("watcher-v1:{:x}", digest.finalize())
}

fn registration_claims(
    registration: &OnlineCorpusWatcherRegistration,
) -> Result<IsolationClaims, String> {
    if registration.id.trim().is_empty()
        || registration.source_id.trim().is_empty()
        || registration.source_version.trim().is_empty()
    {
        return Err("watcher registration id, source_id, and source_version are required".into());
    }
    IsolationClaims::from_verified(
        &registration.tenant_id,
        &registration.project_id,
        &registration.actor_id,
    )
    .map_err(|error| error.to_string())
}

/// Evaluate each configured source version once. Cursors advance only after
/// the idempotent job store accepts (or reuses) the logical job, so a crash
/// before persistence is safe to retry and a restart cannot duplicate it.
pub(crate) async fn tick_online_corpus_watchers(
    store: &OnlineCorpusJobStore,
    settings: &OnlineCorpusWatcherSettings,
) -> WatcherTickReport {
    let mut report = WatcherTickReport::default();
    if !settings.enabled {
        return report;
    }

    let mut cursors = load_watcher_cursors();
    for registration in settings
        .registrations
        .iter()
        .filter(|registration| registration.enabled)
        .take(settings.max_concurrent_polls)
    {
        report.considered += 1;
        let cursor = registration_cursor(registration);
        if cursors.contains(&cursor) {
            continue;
        }
        let claims = match registration_claims(registration) {
            Ok(claims) => claims,
            Err(error) => {
                report.invalid += 1;
                tracing::warn!(watcher_id = %registration.id, error, "online corpus watcher registration rejected");
                continue;
            }
        };

        let queued = store
            .read()
            .await
            .iter()
            .filter(|job| {
                job.tenant_id == claims.tenant_id()
                    && job.project_id == claims.project_id()
                    && job.state == OnlineCorpusJobState::Queued
            })
            .count();
        if queued >= settings.queue_capacity {
            report.saturated += 1;
            tracing::warn!(
                watcher_id = %registration.id,
                queue_capacity = settings.queue_capacity,
                queued,
                "online corpus watcher queue is saturated"
            );
            continue;
        }

        let request = CreateOnlineCorpusJobRequest {
            source: CorpusSource {
                id: registration.source_id.clone(),
                version: registration.source_version.clone(),
                uri: registration.source_uri.clone(),
            },
            idempotency_key: Some(watcher_idempotency_key(registration)),
            watcher_id: Some(registration.id.clone()),
        };
        match enqueue_online_corpus_job(store, &claims, request).await {
            Ok(result) => {
                if result.reused {
                    report.reused += 1;
                } else {
                    report.enqueued += 1;
                }
                cursors.insert(cursor);
                if let Err(error) = save_watcher_cursors(&cursors) {
                    // The job's idempotency key makes this retry-safe after a
                    // cursor persistence failure.
                    tracing::error!(watcher_id = %registration.id, error, "online corpus watcher cursor was not persisted");
                }
            }
            Err(error) => {
                report.invalid += 1;
                tracing::warn!(watcher_id = %registration.id, error, "online corpus watcher enqueue rejected");
            }
        }
    }
    report
}

/// Start the bounded periodic scheduler only when there is work configured.
/// No task exists for an explicitly disabled watcher deployment.
pub(crate) fn start_online_corpus_watcher_scheduler(
    store: OnlineCorpusJobStore,
    settings: OnlineCorpusWatcherSettings,
) {
    if !settings.enabled || settings.registrations.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(settings.poll_interval_seconds));
        loop {
            interval.tick().await;
            let report = tick_online_corpus_watchers(&store, &settings).await;
            if report.saturated > 0 || report.invalid > 0 {
                tracing::warn!(
                    ?report,
                    "online corpus watcher tick completed with deferred registrations"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::http::TEST_ENV_LOCK;
    use std::sync::Arc;

    fn registration(version: &str) -> OnlineCorpusWatcherRegistration {
        OnlineCorpusWatcherRegistration {
            id: "watch-docs".into(),
            enabled: true,
            source_id: "docs".into(),
            source_version: version.into(),
            source_uri: Some("https://example.test/docs".into()),
            tenant_id: "tenant-a".into(),
            project_id: "project-a".into(),
            actor_id: "watcher-service".into(),
        }
    }

    fn registration_for_scope(
        id: &str,
        source_id: &str,
        version: &str,
        tenant_id: &str,
        project_id: &str,
    ) -> OnlineCorpusWatcherRegistration {
        OnlineCorpusWatcherRegistration {
            id: id.into(),
            source_id: source_id.into(),
            source_version: version.into(),
            tenant_id: tenant_id.into(),
            project_id: project_id.into(),
            ..registration(version)
        }
    }

    #[tokio::test]
    async fn watchers_default_on_enqueue_once_and_survive_restart() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        let settings = OnlineCorpusWatcherSettings {
            registrations: vec![registration("v1")],
            ..Default::default()
        };
        let store = Arc::new(tokio::sync::RwLock::new(vec![]));

        assert!(settings.enabled);
        assert_eq!(
            tick_online_corpus_watchers(&store, &settings)
                .await
                .enqueued,
            1
        );
        assert_eq!(
            tick_online_corpus_watchers(&store, &settings)
                .await
                .enqueued,
            0
        );
        assert_eq!(store.read().await.len(), 1);

        let restarted_store = Arc::new(tokio::sync::RwLock::new(
            super::super::corpus_jobs::load_online_corpus_jobs(),
        ));
        assert_eq!(
            tick_online_corpus_watchers(&restarted_store, &settings)
                .await
                .enqueued,
            0
        );
        assert_eq!(restarted_store.read().await.len(), 1);
        std::env::remove_var("AGENTOS_DATA_DIR");
    }

    #[tokio::test]
    async fn explicit_disable_does_not_enqueue_or_clear_existing_jobs() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        let store = Arc::new(tokio::sync::RwLock::new(vec![]));
        let enabled = OnlineCorpusWatcherSettings {
            registrations: vec![registration("v1")],
            ..Default::default()
        };
        tick_online_corpus_watchers(&store, &enabled).await;
        let disabled = OnlineCorpusWatcherSettings {
            enabled: false,
            registrations: vec![registration("v2")],
            ..Default::default()
        };
        assert_eq!(
            tick_online_corpus_watchers(&store, &disabled).await,
            WatcherTickReport::default()
        );
        assert_eq!(store.read().await.len(), 1);
        let reenabled = OnlineCorpusWatcherSettings {
            registrations: vec![registration("v2")],
            ..Default::default()
        };
        assert_eq!(
            tick_online_corpus_watchers(&store, &reenabled)
                .await
                .enqueued,
            1
        );
        assert_eq!(store.read().await.len(), 2);
        std::env::remove_var("AGENTOS_DATA_DIR");
    }

    #[tokio::test]
    async fn watchers_deduplicate_within_scope_but_not_across_scopes() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        let store = Arc::new(tokio::sync::RwLock::new(vec![]));
        let settings = OnlineCorpusWatcherSettings {
            registrations: vec![
                registration_for_scope("watch-a", "docs", "v1", "tenant-a", "project-a"),
                registration_for_scope("watch-b", "docs", "v1", "tenant-a", "project-a"),
                registration_for_scope("watch-c", "docs", "v1", "tenant-b", "project-a"),
            ],
            ..Default::default()
        };

        let report = tick_online_corpus_watchers(&store, &settings).await;
        assert_eq!(report.enqueued, 2);
        assert_eq!(report.reused, 1);
        let jobs = store.read().await;
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|job| job.tenant_id == "tenant-a"));
        assert!(jobs.iter().any(|job| job.tenant_id == "tenant-b"));
        std::env::remove_var("AGENTOS_DATA_DIR");
    }

    #[tokio::test]
    async fn queue_capacity_defers_new_watcher_jobs_without_advancing_cursor() {
        let _lock = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", temp.path());
        let store = Arc::new(tokio::sync::RwLock::new(vec![]));
        let settings = OnlineCorpusWatcherSettings {
            queue_capacity: 1,
            registrations: vec![
                registration_for_scope("watch-a", "docs", "v1", "tenant-a", "project-a"),
                registration_for_scope("watch-b", "handbook", "v1", "tenant-a", "project-a"),
            ],
            ..Default::default()
        };

        let report = tick_online_corpus_watchers(&store, &settings).await;
        assert_eq!(report.enqueued, 1);
        assert_eq!(report.saturated, 1);
        assert_eq!(store.read().await.len(), 1);
        std::env::remove_var("AGENTOS_DATA_DIR");
    }
}
