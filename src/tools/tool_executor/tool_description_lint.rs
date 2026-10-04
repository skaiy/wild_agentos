use serde_json::Value;

use super::ToolDescription;

pub const MIN_DESCRIPTION_BYTES: usize = 80;
pub const MAX_DESCRIPTION_BYTES: usize = 600;
#[cfg(test)]
pub const MAX_ROLE_SCHEMA_BYTES: usize = 48_000;
#[cfg(test)]
pub const MAX_LINT_EXEMPTIONS: usize = 0;
#[cfg(test)]
pub const LINT_EXEMPTIONS: &[(&str, &str)] = &[];

const GENERIC_NAMES: &[&str] = &["process_data", "run", "do_task", "helper"];
#[cfg(test)]
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

/// Inspect a single raw registration. Never changes the schema or assumes
/// parameters are objects (external registrations are not trusted).
pub(crate) fn lint_tool(tool: &ToolDescription) -> Vec<String> {
    let mut violations = Vec::new();
    if !valid_name(&tool.name) {
        violations.push(format!("{}: invalid name", tool.name));
    }
    if tool
        .name
        .rsplit_once("_v")
        .is_some_and(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
    {
        violations.push(format!("{}: version suffix", tool.name));
    }
    if GENERIC_NAMES.contains(&tool.name.as_str()) {
        violations.push(format!("{}: generic name", tool.name));
    }
    let len = tool.description.len();
    if !(MIN_DESCRIPTION_BYTES..=MAX_DESCRIPTION_BYTES).contains(&len) && !is_micro_tool(&tool.name)
    {
        violations.push(format!(
            "{}: description length {len} is out of bounds",
            tool.name
        ));
    }
    if !is_micro_tool(&tool.name) {
        if !tool.description.contains("Use when:") {
            violations.push(format!("{}: missing Use when:", tool.name));
        }
        if !tool.description.contains("Not for:") {
            violations.push(format!("{}: missing Not for:", tool.name));
        }
    }
    let Some(object) = tool.parameters.as_object() else {
        violations.push(format!("{}: parameters must be an object", tool.name));
        return violations;
    };
    if object.get("type").and_then(Value::as_str) != Some("object") {
        violations.push(format!("{}: parameters.type must be object", tool.name));
    }
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        violations.push(format!("{}: properties must be an object", tool.name));
        return violations;
    };
    for (name, value) in properties {
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if description.is_empty() {
            violations.push(format!(
                "{}.{name}: missing parameter description",
                tool.name
            ));
        }
        if value.get("type").and_then(Value::as_str) == Some("boolean")
            && !description.to_ascii_lowercase().contains("default")
            && value.get("default").is_none()
        {
            violations.push(format!("{}.{name}: boolean needs a default", tool.name));
        }
    }
    if let Some(required) = object.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !properties.contains_key(name) {
                violations.push(format!(
                    "{}: required parameter {name} is not a property",
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

#[cfg(test)]
mod registry {
    use std::collections::{BTreeSet, HashMap, HashSet};

    use serde_json::Value;

    use super::*;

    pub(crate) fn lint_registry(
        descriptions: &[ToolDescription],
        builtins: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut violations = Vec::new();
        let mut names = HashSet::new();
        for tool in descriptions {
            violations.extend(lint_tool(tool));
            if !names.insert(&tool.name) {
                violations.push(format!("{}: duplicate name", tool.name));
            }
            for family in CONFUSABLE_FAMILIES {
                if family.contains(&tool.name.as_str()) {
                    let not_for = tool.description.split("Not for:").nth(1).unwrap_or("");
                    if !family
                        .iter()
                        .any(|neighbor| *neighbor != tool.name && not_for.contains(neighbor))
                    {
                        violations.push(format!(
                            "{}: Not for: must name a confusable family neighbor",
                            tool.name
                        ));
                    }
                }
            }
        }
        let external: Vec<_> = descriptions
            .iter()
            .filter(|tool| !builtins.contains(&tool.name) && !is_micro_tool(&tool.name))
            .collect();
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
        violations.extend(check_exemptions(LINT_EXEMPTIONS, MAX_LINT_EXEMPTIONS));
        violations
    }

    pub(super) fn check_exemptions(exemptions: &[(&str, &str)], max: usize) -> Vec<String> {
        let mut violations = Vec::new();
        if exemptions.len() > max {
            violations.push("LINT_EXEMPTIONS exceeds its approved maximum".to_string());
        }
        for (name, reason) in exemptions {
            if reason.trim().is_empty() {
                violations.push(format!("{name}: exemption needs a reason"));
            }
        }
        violations
    }

    fn jaccard(left: &str, right: &str) -> f64 {
        let tokens = |value: &str| -> HashSet<String> {
            value
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|token| !token.is_empty())
                .map(str::to_ascii_lowercase)
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

    pub(crate) fn role_schema_bytes(
        definitions: impl IntoIterator<Item = (String, Vec<Value>)>,
    ) -> HashMap<String, usize> {
        definitions
            .into_iter()
            .map(|(role, definitions)| (role, serde_json::to_vec(&definitions).unwrap().len()))
            .collect()
    }

    pub(crate) fn check_role_schema_budget(bytes: &HashMap<String, usize>) -> Vec<String> {
        bytes
            .iter()
            .filter(|(_, size)| **size > MAX_ROLE_SCHEMA_BYTES)
            .map(|(role, size)| {
                format!("{role}: schema is {size} bytes, over {MAX_ROLE_SCHEMA_BYTES}")
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) use registry::{check_role_schema_budget, lint_registry, role_schema_bytes};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    fn valid_tool() -> ToolDescription {
        ToolDescription {
            name: "file_write".to_string(),
            description: "Write a complete text file with the supplied content. Use when: creating or replacing a file is required. Not for: small existing changes; use file_edit instead.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Workspace file path."}},
                "required": ["path"]
            }),
            allowed_roles: vec![],
        }
    }

    #[test]
    fn tool_description_lint_reports_each_rule_separately() {
        let valid = valid_tool();
        assert!(lint_tool(&valid).is_empty());
        let mut bad = valid.clone();
        bad.name = "Bad-Name".into();
        assert!(lint_tool(&bad).join(" ").contains("invalid name"));
        bad.name = "writer_v2".into();
        assert!(lint_tool(&bad).join(" ").contains("version suffix"));
        bad.name = "helper".into();
        assert!(lint_tool(&bad).join(" ").contains("generic name"));
        assert!(
            lint_registry(&[valid.clone(), valid.clone()], &BTreeSet::new())
                .join(" ")
                .contains("duplicate name")
        );

        bad = valid.clone();
        bad.description = "short".into();
        assert!(lint_tool(&bad).join(" ").contains("description length"));
        bad.description = "x".repeat(MAX_DESCRIPTION_BYTES + 1);
        assert!(lint_tool(&bad).join(" ").contains("description length"));
        bad.description = format!("{} Not for: file_edit", "x".repeat(80));
        assert!(lint_tool(&bad).join(" ").contains("missing Use when:"));
        bad.description = format!("{} Use when: writing", "x".repeat(80));
        assert!(lint_tool(&bad).join(" ").contains("missing Not for:"));
        bad.description = "Write a complete file with supplied content. Use when: replacing a file is required. Not for: unrelated work; choose a different operation instead.".into();
        assert!(lint_registry(&[bad], &BTreeSet::new())
            .join(" ")
            .contains("confusable family neighbor"));

        bad = valid.clone();
        bad.parameters =
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["missing"]});
        let errors = lint_tool(&bad).join(" ");
        assert!(errors.contains("missing parameter description"));
        assert!(errors.contains("required parameter missing is not a property"));
        bad.parameters = json!({"type":"object","properties":{"flag":{"type":"boolean","description":"Enable flag."}}});
        assert!(lint_tool(&bad)
            .join(" ")
            .contains("boolean needs a default"));
        for parameters in [json!(null), json!("text"), json!([])] {
            bad.parameters = parameters;
            assert!(lint_tool(&bad)
                .join(" ")
                .contains("parameters must be an object"));
        }
        bad.parameters = json!({"properties":{}});
        assert!(lint_tool(&bad)
            .join(" ")
            .contains("parameters.type must be object"));

        let bytes = role_schema_bytes([(
            "Plan".to_string(),
            vec![json!({"padding": "x".repeat(MAX_ROLE_SCHEMA_BYTES)})],
        )]);
        assert!(check_role_schema_budget(&bytes).join(" ").contains("Plan"));

        let mut left = valid.clone();
        left.name = "external_one".into();
        let mut right = left.clone();
        right.name = "external_two".into();
        assert!(lint_registry(&[left, right], &BTreeSet::new())
            .join(" ")
            .contains("overly similar"));
        assert!(registry::check_exemptions(&[("tool", "")], 1)
            .join(" ")
            .contains("needs a reason"));
        assert!(registry::check_exemptions(&[("tool", "documented")], 0)
            .join(" ")
            .contains("approved maximum"));
    }
}
