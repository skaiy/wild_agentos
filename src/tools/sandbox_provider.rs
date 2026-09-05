//! External compute-sandbox contract.
//!
//! The kernel never executes submitted work. Providers run it elsewhere and
//! return this small, JSON-serializable result envelope.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::event_bus::EventBus;

pub const SANDBOX_AUDIT_EVENT: &str = "SANDBOX_AUDIT";
const SANDBOX_AUDIT_SOURCE: &str = "external-sandbox-provider";

/// Verified scope that accompanies work sent to a sandbox provider.
///
/// Callers must construct this from their authenticated isolation claims, not
/// from request-body values. It is passed through only as structured metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxClaims {
    pub tenant_id: String,
    pub project_id: String,
    pub actor_id: String,
}

/// Structured work description accepted by an external provider.
///
/// `work` deliberately has no shell-command field: provider-specific execution
/// inputs must be represented as validated JSON, rather than making the kernel
/// a fourth in-process operating-system sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SandboxSubmitRequest {
    pub task_id: String,
    pub claims: SandboxClaims,
    pub work: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl SandboxTaskStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Result JSON schema returned to the kernel by every provider.
///
/// `output` is structured JSON. Logs are bounded textual diagnostics supplied
/// by the provider, never an executable continuation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SandboxResult {
    pub task_id: String,
    pub status: SandboxTaskStatus,
    pub exit_code: Option<i32>,
    pub output: Value,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

impl SandboxResult {
    pub fn validate(&self, task_id: &str) -> Result<()> {
        if self.task_id != task_id {
            return Err(anyhow!(
                "sandbox provider returned result for task {} while {} was requested",
                self.task_id,
                task_id
            ));
        }
        if !self.status.is_terminal() {
            return Err(anyhow!("sandbox result must have a terminal status"));
        }
        Ok(())
    }
}

/// Mount point for a remote compute sandbox.
///
/// Implementations own remote execution lifecycle; this contract exposes only
/// submit, status, and structured-result retrieval to the kernel.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    fn provider_name(&self) -> &str;

    async fn submit(&self, request: SandboxSubmitRequest) -> Result<String>;

    async fn status(&self, task_id: &str) -> Result<SandboxTaskStatus>;

    async fn fetch_result(&self, task_id: &str) -> Result<SandboxResult>;
}

/// Default provider for development and tests. It executes no code.
#[derive(Debug, Default)]
pub struct MockSandboxProvider {
    tasks: Mutex<HashMap<String, SandboxResult>>,
}

impl MockSandboxProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a terminal response for a known task without running arbitrary code.
    pub fn set_result(&self, result: SandboxResult) {
        self.tasks.lock().insert(result.task_id.clone(), result);
    }
}

#[async_trait]
impl SandboxProvider for MockSandboxProvider {
    fn provider_name(&self) -> &str {
        "mock"
    }

    async fn submit(&self, request: SandboxSubmitRequest) -> Result<String> {
        if request.task_id.trim().is_empty() {
            return Err(anyhow!("sandbox task_id must not be empty"));
        }

        let task_id = request.task_id;
        self.tasks
            .lock()
            .entry(task_id.clone())
            .or_insert_with(|| SandboxResult {
                task_id: task_id.clone(),
                status: SandboxTaskStatus::Succeeded,
                exit_code: Some(0),
                output: request.work,
                stdout: String::new(),
                stderr: String::new(),
            });
        Ok(task_id)
    }

    async fn status(&self, task_id: &str) -> Result<SandboxTaskStatus> {
        self.tasks
            .lock()
            .get(task_id)
            .map(|result| result.status)
            .ok_or_else(|| anyhow!("sandbox task {task_id} was not found"))
    }

    async fn fetch_result(&self, task_id: &str) -> Result<SandboxResult> {
        let result = self
            .tasks
            .lock()
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow!("sandbox task {task_id} was not found"))?;
        result.validate(task_id)?;
        Ok(result)
    }
}

/// Emits a best-effort audit event after a terminal provider result is read.
pub struct AuditedSandboxProvider<P> {
    provider: P,
    event_bus: Arc<EventBus>,
    claims_by_task: Mutex<HashMap<String, SandboxClaims>>,
}

impl<P> AuditedSandboxProvider<P> {
    #[must_use]
    pub fn new(provider: P, event_bus: Arc<EventBus>) -> Self {
        Self {
            provider,
            event_bus,
            claims_by_task: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl<P: SandboxProvider> SandboxProvider for AuditedSandboxProvider<P> {
    fn provider_name(&self) -> &str {
        self.provider.provider_name()
    }

    async fn submit(&self, request: SandboxSubmitRequest) -> Result<String> {
        let claims = request.claims.clone();
        let task_id = self.provider.submit(request).await?;
        self.claims_by_task.lock().insert(task_id.clone(), claims);
        Ok(task_id)
    }

    async fn status(&self, task_id: &str) -> Result<SandboxTaskStatus> {
        self.provider.status(task_id).await
    }

    async fn fetch_result(&self, task_id: &str) -> Result<SandboxResult> {
        let result = self.provider.fetch_result(task_id).await?;
        result.validate(task_id)?;

        // Keep the non-Send parking_lot guard confined to this synchronous
        // expression; EventBus::emit below awaits.
        let claims = { self.claims_by_task.lock().remove(task_id) };
        if let Some(claims) = claims {
            let payload = serde_json::json!({
                "task_id": task_id,
                "provider": self.provider.provider_name(),
                "tenant_id": claims.tenant_id,
                "project_id": claims.project_id,
                "actor_id": claims.actor_id,
                "status": result.status,
                "exit_code": result.exit_code,
            });
            // EventBus is intentionally best-effort: a finished sandbox result
            // must remain available even when no consumer is subscribed.
            self.event_bus
                .emit(
                    task_id,
                    SANDBOX_AUDIT_EVENT,
                    SANDBOX_AUDIT_SOURCE,
                    &serde_json::to_string(&payload)?,
                )
                .await;
        }
        Ok(result)
    }
}

#[cfg(feature = "external-sandbox")]
pub mod http {
    use super::{SandboxProvider, SandboxResult, SandboxSubmitRequest, SandboxTaskStatus};
    use anyhow::Result;
    use async_trait::async_trait;
    use reqwest::{Client, Url};

    /// Generic HTTP adapter for OpenHands/E2B-style gateway services.
    ///
    /// The gateway is expected to expose `POST /tasks`, `GET /tasks/{id}`, and
    /// `GET /tasks/{id}/result` with the contract types in this module.
    pub struct HttpSandboxProvider {
        base_url: Url,
        client: Client,
    }

    impl HttpSandboxProvider {
        pub fn new(base_url: Url) -> Self {
            Self {
                base_url,
                client: Client::new(),
            }
        }

        fn endpoint(&self, path: &str) -> Result<Url> {
            Ok(self.base_url.join(path)?)
        }
    }

    #[async_trait]
    impl SandboxProvider for HttpSandboxProvider {
        fn provider_name(&self) -> &str {
            "http"
        }

        async fn submit(&self, request: SandboxSubmitRequest) -> Result<String> {
            #[derive(serde::Deserialize)]
            struct SubmitResponse {
                task_id: String,
            }
            let response = self
                .client
                .post(self.endpoint("tasks")?)
                .json(&request)
                .send()
                .await?
                .error_for_status()?
                .json::<SubmitResponse>()
                .await?;
            Ok(response.task_id)
        }

        async fn status(&self, task_id: &str) -> Result<SandboxTaskStatus> {
            #[derive(serde::Deserialize)]
            struct StatusResponse {
                status: SandboxTaskStatus,
            }
            Ok(self
                .client
                .get(self.endpoint(&format!("tasks/{task_id}"))?)
                .send()
                .await?
                .error_for_status()?
                .json::<StatusResponse>()
                .await?
                .status)
        }

        async fn fetch_result(&self, task_id: &str) -> Result<SandboxResult> {
            let result = self
                .client
                .get(self.endpoint(&format!("tasks/{task_id}/result"))?)
                .send()
                .await?
                .error_for_status()?
                .json::<SandboxResult>()
                .await?;
            result.validate(task_id)?;
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(task_id: &str) -> SandboxSubmitRequest {
        SandboxSubmitRequest {
            task_id: task_id.to_string(),
            claims: SandboxClaims {
                tenant_id: "tenant-a".to_string(),
                project_id: "project-a".to_string(),
                actor_id: "actor-a".to_string(),
            },
            work: serde_json::json!({"operation": "compile", "input": {"language": "rust"}}),
        }
    }

    #[tokio::test]
    async fn mock_provider_returns_structured_result_without_executing_work() {
        let provider = MockSandboxProvider::new();
        assert_eq!(provider.submit(request("task-1")).await.unwrap(), "task-1");
        assert_eq!(
            provider.status("task-1").await.unwrap(),
            SandboxTaskStatus::Succeeded
        );

        let result = provider.fetch_result("task-1").await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.output["operation"], "compile");
    }

    #[tokio::test]
    async fn audited_provider_emits_claims_task_and_exit_code() {
        let bus = Arc::new(EventBus::new(4));
        let mut receiver = bus.subscribe();
        let provider = AuditedSandboxProvider::new(MockSandboxProvider::new(), bus);
        provider.submit(request("task-2")).await.unwrap();
        provider.fetch_result("task-2").await.unwrap();

        let event = receiver.recv().await.unwrap();
        let payload: Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(event.event_type, SANDBOX_AUDIT_EVENT);
        assert_eq!(payload["task_id"], "task-2");
        assert_eq!(payload["tenant_id"], "tenant-a");
        assert_eq!(payload["exit_code"], 0);
    }

    #[test]
    fn rejects_non_terminal_or_wrong_task_results() {
        let result = SandboxResult {
            task_id: "other".to_string(),
            status: SandboxTaskStatus::Running,
            exit_code: None,
            output: Value::Null,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(result.validate("task-3").is_err());
    }
}
