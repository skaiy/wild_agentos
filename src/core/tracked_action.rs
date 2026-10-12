use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::tool_controller::{classify_recorded_tool_call, UNREGISTERED_TOOL_NAME};
use crate::gateway::usage_meter::RunUsageMeter;

/// How a tool attempt should be classified into the run's usage meter.
pub struct ToolUsageClass {
    pub registered: bool,
    pub policy_denied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub size_bytes: Option<u64>,
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ActionStatus {
    Success,
    Failed,
    Retried,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedAction {
    pub action_id: String,
    pub tool_name: String,
    pub agent_role: String,
    pub duration_secs: f64,
    pub status: ActionStatus,
    pub files_created: Vec<FileChange>,
    pub files_modified: Vec<FileChange>,
    pub files_read: Vec<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub tool_args: HashMap<String, Value>,
}

pub struct ActionTracker {
    pub actions: Vec<TrackedAction>,
    pub task_iri: String,
    pub agent_role: String,
    pub started_at: DateTime<Utc>,
    usage_meter: Option<Arc<RunUsageMeter>>,
}

impl ActionTracker {
    pub fn new(task_iri: &str, agent_role: &str) -> Self {
        Self {
            actions: Vec::new(),
            task_iri: task_iri.to_string(),
            agent_role: agent_role.to_string(),
            started_at: Utc::now(),
            usage_meter: None,
        }
    }

    pub fn with_usage_meter(mut self, meter: Option<Arc<RunUsageMeter>>) -> Self {
        self.usage_meter = meter;
        self
    }

    /// Records a policy refusal that never reached [`Self::record`]. The raw
    /// model name is not stored.
    pub fn note_unregistered_attempt(&self) {
        if let Some(meter) = &self.usage_meter {
            meter.record_tool_call(UNREGISTERED_TOOL_NAME, "unknown");
        }
    }

    pub fn record(&mut self, tool_name: &str, args: &Value, result: &Value, duration_secs: f64) {
        self.record_classified(
            tool_name,
            args,
            result,
            duration_secs,
            ToolUsageClass {
                registered: true,
                policy_denied: false,
            },
        );
    }

    /// Records the action and, when this tracker is bound to a run meter,
    /// the classified name and transport. Arguments stay on the in-memory
    /// action only.
    pub fn record_classified(
        &mut self,
        tool_name: &str,
        args: &Value,
        result: &Value,
        duration_secs: f64,
        class: ToolUsageClass,
    ) {
        self.meter_tool_call(tool_name, &class, result);
        self.record_action(tool_name, args, result, duration_secs);
    }

    fn meter_tool_call(&self, tool_name: &str, class: &ToolUsageClass, result: &Value) {
        let Some(meter) = &self.usage_meter else {
            return;
        };
        let reported_missing = result
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|msg| {
                msg.contains("Tool not found") || msg.contains("no registered executable skill")
            });
        let (name, transport) = classify_recorded_tool_call(
            tool_name,
            class.registered && !reported_missing,
            class.policy_denied || reported_missing,
        );
        meter.record_tool_call(name, transport);
    }

    fn record_action(&mut self, tool_name: &str, args: &Value, result: &Value, duration_secs: f64) {
        let mut action = TrackedAction {
            action_id: format!("act_{}", uuid::Uuid::new_v4().hyphenated()),
            tool_name: tool_name.to_string(),
            agent_role: self.agent_role.clone(),
            duration_secs,
            status: if result.get("error").is_some() {
                ActionStatus::Failed
            } else {
                ActionStatus::Success
            },
            files_created: vec![],
            files_modified: vec![],
            files_read: vec![],
            error: result
                .get("error")
                .and_then(|e| e.as_str())
                .map(String::from),
            tool_args: HashMap::new(),
        };

        match tool_name {
            "file_write" => {
                if result.get("effect_applied") == Some(&Value::Bool(true)) {
                    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                        self.actions.push(action);
                        return;
                    };
                    action
                        .tool_args
                        .insert("path".to_string(), Value::String(path.to_string()));
                    action.files_created.push(FileChange {
                        path: path.to_string(),
                        size_bytes: args
                            .get("content")
                            .and_then(|v| v.as_str())
                            .map(|c| c.len() as u64),
                        hash: None,
                    });
                }
            }
            "file_edit" => {
                if let Some(path) = args.get("filePath").and_then(|v| v.as_str()) {
                    action.files_modified.push(FileChange {
                        path: path.to_string(),
                        size_bytes: None,
                        hash: None,
                    });
                }
            }
            "file_read" => {
                if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                    action.files_read.push(path.to_string());
                }
            }
            "bash" | "powershell" if result.get("error").is_none() => {
                action.tool_args.insert(
                    "command".to_string(),
                    args.get("command").cloned().unwrap_or_default(),
                );
            }
            _ => {}
        }

        self.actions.push(action);
    }

    pub fn success_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| a.status == ActionStatus::Success)
            .count()
    }

    pub fn failure_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| a.status == ActionStatus::Failed)
            .count()
    }

    pub fn files_created_all(&self) -> Vec<&FileChange> {
        self.actions.iter().flat_map(|a| &a.files_created).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Whether a declared workspace write was skipped or failed without any
    /// later verified effect. The Do phase must pass this outcome to Check
    /// instead of claiming successful completion.
    pub fn requires_post_write_verification(&self) -> bool {
        let attempted_file_write = self
            .actions
            .iter()
            .any(|action| action.tool_name == "file_write");
        let applied_file_write = self.actions.iter().any(|action| {
            action.tool_name == "file_write"
                && action.status == ActionStatus::Success
                && (!action.files_created.is_empty() || !action.files_modified.is_empty())
        });
        attempted_file_write && !applied_file_write
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skipped_file_write_requires_post_write_verification() {
        let mut tracker = ActionTracker::new("iri://task/test", "Do");
        tracker.record(
            "file_write",
            &json!({"path": "output.txt", "content": "unchanged"}),
            &json!({"error": "Write skipped: file already contains the requested content"}),
            0.01,
        );

        assert!(tracker.requires_post_write_verification());
    }

    #[test]
    fn effectful_file_write_does_not_require_retry_verification() {
        let mut tracker = ActionTracker::new("iri://task/test", "Do");
        tracker.record(
            "file_write",
            &json!({"path": "output.txt", "content": "changed"}),
            &json!({"success": true, "effect_applied": true}),
            0.01,
        );

        assert!(!tracker.requires_post_write_verification());
    }
}
