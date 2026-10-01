use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::ToolDescription;

pub const MIN_DESCRIPTION_BYTES: usize = 80;
pub const MAX_DESCRIPTION_BYTES: usize = 600;
pub const MAX_ROLE_SCHEMA_BYTES: usize = 48_000;
pub const MAX_LINT_EXEMPTIONS: usize = 0;
pub const LINT_EXEMPTIONS: &[(&str, &str)] = &[];

const GENERIC_NAMES: &[&str] = &["process_data", "run", "do_task", "helper"];
const CONFUSABLE_FAMILIES: &[&[&str]] = &[
    &[
        "grep_search",
        "glob_search",
        "rag_search",
        "kg_search",
        "kb_vector_search",
        "knowledge_search",
        "knowledge_query",
        "knowledge_list",
    ],
    &["file_write", "file_edit"],
    &["bash", "powershell"],
    &["web_search", "web_fetch"],
    &[
        "knowledge_import_file",
        "knowledge_import_url",
        "knowledge_import_directory",
        "knowledge_import_json",
    ],
    &[
        "ontology_validate_turtle",
        "ontology_lint_turtle",
        "ontology_validate_shacl",
    ],
];

const BUILTIN_NAMES: &[&str] = &[
    "glob_search",
    "grep_search",
    "web_fetch",
    "web_search",
    "tool_search",
    "file_read",
    "file_write",
    "workspace_status",
    "file_list",
    "bash",
    "file_edit",
    "powershell",
    "rag_search",
    "rag_index",
    "rag_chunk",
    "knowledge_import_file",
    "knowledge_import_url",
    "knowledge_import_directory",
    "knowledge_list",
    "knowledge_delete",
    "knowledge_search",
    "knowledge_update",
    "create_skill",
    "convert_skill",
    "knowledge_extract",
    "knowledge_query",
    "kg_search",
    "kb_vector_search",
    "knowledge_neighbors",
    "knowledge_import_json",
    "ontology_register",
    "knowledge_bridge",
    "knowledge_extract_code",
    "read_agent_output",
    "ontology_validate_turtle",
    "ontology_lint_turtle",
    "ontology_diff_turtle",
    "ontology_validate_shacl",
    "ontology_reason",
];

pub fn normalize_builtin_description(name: &str, description: &str) -> String {
    if !BUILTIN_NAMES.contains(&name) {
        return description.to_string();
    }
    let base: String = description.chars().take(280).collect();
    let sibling = confusable_sibling(name).unwrap_or("a more specific tool");
    format!(
        "{base} Use when: the current task specifically needs {} and its required input is available. Not for: {}; choose the tool that matches that operation instead.",
        name.replace('_', " "),
        sibling
    )
}

pub fn normalize_parameter_descriptions(mut parameters: Value) -> Value {
    let object = parameters
        .as_object_mut()
        .expect("tool parameters must be objects");
    object
        .entry("type".to_string())
        .or_insert_with(|| Value::String("object".to_string()));
    let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) else {
        return parameters;
    };
    for (name, property) in properties {
        let Some(property) = property.as_object_mut() else {
            continue;
        };
        let type_name = property
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("value");
        let description = property
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut normalized = if description.is_empty() {
            format!("{} value.", name.replace('_', " "))
        } else {
            description.to_string()
        };
        if matches!(name.as_str(), "path" | "file_path" | "source_path") {
            normalized.push_str(" Format: workspace path.");
        } else if name.contains("url") {
            normalized.push_str(" Format: absolute http or https URL.");
        } else if matches!(name.as_str(), "pattern" | "selector") {
            normalized.push_str(" Format: glob, CSS selector, or regular expression as required.");
        }
        if type_name == "boolean" && !normalized.to_lowercase().contains("default") {
            normalized.push_str(" Default: false.");
        }
        property.insert("description".to_string(), Value::String(normalized));
    }
    parameters
}

pub fn lint_registry(descriptions: &[ToolDescription]) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();
    let mut names = HashSet::new();
    for tool in descriptions {
        violations.extend(lint_tool(tool, &mut names));
    }
    for family in CONFUSABLE_FAMILIES {
        for name in *family {
            if let Some(tool) = descriptions.iter().find(|tool| tool.name == *name) {
                let not_for = tool
                    .description
                    .split("Not for:")
                    .nth(1)
                    .unwrap_or_default();
                if !family
                    .iter()
                    .any(|sibling| *sibling != *name && not_for.contains(sibling))
                {
                    violations.push(format!(
                        "{name}: Not for: must name a confusable family neighbor"
                    ));
                }
            }
        }
    }
    violations.extend(similar_external_tools(descriptions));
    if LINT_EXEMPTIONS.len() > MAX_LINT_EXEMPTIONS {
        violations.push("LINT_EXEMPTIONS exceeds its approved maximum".to_string());
    }
    for (name, reason) in LINT_EXEMPTIONS {
        if reason.trim().is_empty() {
            violations.push(format!("{name}: exemption needs a reason"));
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn lint_tool(tool: &ToolDescription, names: &mut HashSet<String>) -> Vec<String> {
    let mut violations = Vec::new();
    if !valid_name(&tool.name) || GENERIC_NAMES.contains(&tool.name.as_str()) {
        violations.push(format!("{}: invalid or generic name", tool.name));
    }
    if !names.insert(tool.name.clone()) {
        violations.push(format!("{}: duplicate name", tool.name));
    }
    let len = tool.description.len();
    if !(MIN_DESCRIPTION_BYTES..=MAX_DESCRIPTION_BYTES).contains(&len) {
        violations.push(format!(
            "{}: description length {len} is out of bounds",
            tool.name
        ));
    }
    if !is_micro_tool(&tool.name)
        && (!tool.description.contains("Use when:") || !tool.description.contains("Not for:"))
    {
        violations.push(format!("{}: missing Use when: or Not for:", tool.name));
    }
    if tool.parameters["type"] != "object" {
        violations.push(format!("{}: parameters.type must be object", tool.name));
    }
    let properties = tool.parameters["properties"].as_object();
    if let Some(properties) = properties {
        for (name, value) in properties {
            if value["description"]
                .as_str()
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                violations.push(format!(
                    "{}.{name}: missing parameter description",
                    tool.name
                ));
            }
            if value["type"] == "boolean"
                && !value["description"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("default")
                && value.get("default").is_none()
            {
                violations.push(format!("{}.{name}: boolean needs a default", tool.name));
            }
        }
        for required in tool
            .parameters
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !properties.contains_key(required) {
                violations.push(format!(
                    "{}: required parameter {required} is not a property",
                    tool.name
                ));
            }
        }
    }
    violations
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=64).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        && !name.rsplit_once("_v").is_some_and(|(_, suffix)| {
            !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
        })
}

fn is_micro_tool(name: &str) -> bool {
    [
        "read_full_result_",
        "query_",
        "get_entity_details_",
        "expand_relation_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

fn confusable_sibling(name: &str) -> Option<&'static str> {
    CONFUSABLE_FAMILIES
        .iter()
        .find(|family| family.contains(&name))
        .and_then(|family| family.iter().copied().find(|sibling| *sibling != name))
}

fn similar_external_tools(descriptions: &[ToolDescription]) -> Vec<String> {
    let external: Vec<&ToolDescription> = descriptions
        .iter()
        .filter(|tool| !BUILTIN_NAMES.contains(&tool.name.as_str()) && !is_micro_tool(&tool.name))
        .collect();
    let mut violations = Vec::new();
    for (index, left) in external.iter().enumerate() {
        for right in external.iter().skip(index + 1) {
            if jaccard(&left.description, &right.description) > 0.6 {
                violations.push(format!(
                    "{} and {} have overly similar descriptions; add a family or rewrite",
                    left.name, right.name
                ));
            }
        }
    }
    violations
}

fn jaccard(left: &str, right: &str) -> f64 {
    let tokens = |value: &str| -> HashSet<String> {
        value
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .filter(|token| !token.is_empty())
            .map(|token| token.to_ascii_lowercase())
            .collect()
    };
    let left = tokens(left);
    let right = tokens(right);
    let union = left.union(&right).count();
    if union == 0 {
        0.0
    } else {
        left.intersection(&right).count() as f64 / union as f64
    }
}

pub fn role_schema_bytes(
    definitions: impl IntoIterator<Item = (String, Vec<Value>)>,
) -> HashMap<String, usize> {
    definitions
        .into_iter()
        .map(|(role, definitions)| {
            let bytes = serde_json::to_vec(&definitions)
                .expect("tool definitions are serializable")
                .len();
            (role, bytes)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn valid_tool() -> ToolDescription {
        ToolDescription {
            name: "file_write".to_string(),
            description: "Write a complete text file with the supplied content. Use when: creating or replacing a file is required. Not for: file_edit; use replacement edits for a small existing change.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Workspace path. Format: workspace path."}},
                "required": ["path"]
            }),
            allowed_roles: vec![],
        }
    }

    #[test]
    fn lint_reports_each_rule_category() {
        let mut invalid = valid_tool();
        invalid.name = "run_v2".to_string();
        invalid.description = "short".to_string();
        invalid.parameters = json!({
            "type": "array",
            "properties": {"enabled": {"type": "boolean"}},
            "required": ["missing"]
        });
        let errors = lint_registry(&[invalid]).unwrap_err().join("\n");
        assert!(errors.contains("invalid or generic name"));
        assert!(errors.contains("description length"));
        assert!(errors.contains("missing Use when: or Not for:"));
        assert!(errors.contains("parameters.type must be object"));
        assert!(errors.contains("missing parameter description"));
        assert!(errors.contains("boolean needs a default"));
        assert!(errors.contains("required parameter missing is not a property"));
    }

    #[test]
    fn lint_requires_a_named_confusable_neighbor() {
        let mut tool = valid_tool();
        tool.description = "Write a complete text file with supplied content. Use when: replacing a file is required. Not for: unrelated work; choose another tool instead.".to_string();
        let errors = lint_registry(&[tool]).unwrap_err().join("\n");
        assert!(errors.contains("confusable family neighbor"));
    }

    #[test]
    fn lint_detects_over_budget_role_schema() {
        let bytes = role_schema_bytes([(
            "Plan".to_string(),
            vec![json!({"padding": "x".repeat(MAX_ROLE_SCHEMA_BYTES)})],
        )]);
        assert!(bytes["Plan"] > MAX_ROLE_SCHEMA_BYTES);
    }
}
