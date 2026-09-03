use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

impl ActionTracker {
    pub fn new(task_iri: &str, agent_role: &str) -> Self {
        Self {
            actions: Vec::new(),
            task_iri: task_iri.to_string(),
            agent_role: agent_role.to_string(),
            started_at: Utc::now(),
        }
    }

    pub fn record(&mut self, tool_name: &str, args: &Value, result: &Value, duration_secs: f64) {
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
