//! Claims-scoped validation for ActionType staging-graph writes.
//!
//! This is deliberately a small SPARQL ASK assertion set, not a SHACL engine.

use crate::{
    isolation::IsolationClaims,
    knowledge_graph::{
        ontology_layer::{
            ActionGuardrailConfig, ActionType, OntologyDefinition, SparqlAskAssertion,
        },
        store::KnowledgeGraphStore,
    },
};

/// Backward-compatible cap when neither domain nor action supplies a value.
pub const DEFAULT_MAX_TRIPLES: usize = 5_000;
/// Backward-compatible predicate whitelist when neither domain nor action supplies one.
pub const DEFAULT_ALLOWED_PREDICATE_PREFIXES: &[&str] = &[
    "https://agentos.ontology/ev/",
    "http://www.w3.org/2000/01/rdf-schema#",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
];

/// The resolved, non-optional policy used for a single invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveGuardrails {
    pub max_triples: usize,
    pub allowed_predicate_prefixes: Vec<String>,
    pub assertions: Vec<SparqlAskAssertion>,
}

/// Validate a config supplied through the claims-authenticated ontology CRUD API.
pub fn validate_config(config: &ActionGuardrailConfig) -> Result<(), String> {
    if let Some(prefixes) = &config.allowed_predicate_prefixes {
        if prefixes.iter().any(|prefix| prefix.trim().is_empty()) {
            return Err("护栏谓词白名单不能包含空前缀".to_string());
        }
    }
    for assertion in &config.assertions {
        if !is_valid_code(&assertion.code) {
            return Err(format!("护栏断言代码无效: {}", assertion.code));
        }
        let query = assertion.query.trim();
        if !query.to_ascii_uppercase().starts_with("ASK") {
            return Err(format!(
                "护栏断言 {} 必须是 SPARQL ASK 查询",
                assertion.code
            ));
        }
        if query.to_ascii_uppercase().contains("GRAPH") {
            return Err(format!(
                "护栏断言 {} 不得指定 GRAPH；图作用域由 JWT claims 绑定",
                assertion.code
            ));
        }
    }
    Ok(())
}

/// Resolve domain defaults and action overrides without allowing omitted fields to disable
/// the built-in safety policy. Domain assertions run before action-specific assertions.
pub fn effective_config(
    domain: &OntologyDefinition,
    action: &ActionType,
) -> Result<EffectiveGuardrails, String> {
    validate_config(&domain.guardrails)?;
    validate_config(&action.guardrails)?;
    let max_triples = action
        .guardrails
        .max_triples
        .or(domain.guardrails.max_triples)
        .unwrap_or(DEFAULT_MAX_TRIPLES);
    let allowed_predicate_prefixes = action
        .guardrails
        .allowed_predicate_prefixes
        .as_ref()
        .or(domain.guardrails.allowed_predicate_prefixes.as_ref())
        .cloned()
        .unwrap_or_else(|| {
            DEFAULT_ALLOWED_PREDICATE_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_string())
                .collect()
        });

    let mut assertions = builtin_assertions();
    assertions.extend(domain.guardrails.assertions.clone());
    assertions.extend(action.guardrails.assertions.clone());
    Ok(EffectiveGuardrails {
        max_triples,
        allowed_predicate_prefixes,
        assertions,
    })
}

/// A policy that expands the built-in write surface is retained for HITL approval even
/// when all hard guardrails pass. Restrictive overrides may still auto-commit.
pub fn is_high_risk(policy: &EffectiveGuardrails) -> bool {
    policy.max_triples > DEFAULT_MAX_TRIPLES
        || policy
            .allowed_predicate_prefixes
            .iter()
            .any(|prefix| !DEFAULT_ALLOWED_PREDICATE_PREFIXES.contains(&prefix.as_str()))
}

/// Evaluate all guardrails exclusively in the staging graph minted from verified claims.
/// ASK queries return `true` to signal the associated violation.
pub fn violations(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    staging_id: &str,
    policy: &EffectiveGuardrails,
) -> Result<Vec<String>, String> {
    let mut violations = Vec::new();
    let count_q = "SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o }";
    let n = kg
        .query_staging_for_claims(claims, staging_id, count_q)?
        .into_iter()
        .next()
        .and_then(|row| row.get("?c").and_then(|v| v.as_str().map(str::to_owned)))
        .and_then(|count| count.parse::<usize>().ok())
        .ok_or_else(|| "护栏三元组计数查询返回无效结果".to_string())?;
    if n > policy.max_triples {
        violations.push(format!(
            "triple_cap: 写回三元组数 {n} 超过上限 {}",
            policy.max_triples
        ));
    }

    let filters = policy
        .allowed_predicate_prefixes
        .iter()
        .map(|prefix| format!("STRSTARTS(STR(?p), \"{}\")", sparql_literal(prefix)))
        .collect::<Vec<_>>();
    let foreign_q = format!(
        "SELECT ?p WHERE {{ ?s ?p ?o . FILTER(!({})) }} LIMIT 1",
        filters.join(" || ")
    );
    if !kg
        .query_staging_for_claims(claims, staging_id, &foreign_q)?
        .is_empty()
    {
        violations
            .push("predicate_whitelist: 存在越权谓词（不在允许的命名空间白名单内）".to_string());
    }

    for assertion in &policy.assertions {
        let result = kg.query_staging_for_claims(claims, staging_id, &assertion.query)?;
        if result
            .first()
            .and_then(|row| row.get("result"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            violations.push(format!(
                "assertion:{}: SPARQL ASK 断言未通过",
                assertion.code
            ));
        }
    }
    Ok(violations)
}

fn builtin_assertions() -> Vec<SparqlAskAssertion> {
    vec![
        SparqlAskAssertion {
            code: "repair_order_requires_label".into(),
            query: "ASK { ?order a <https://agentos.ontology/ev/RepairOrder> . FILTER NOT EXISTS { ?order <http://www.w3.org/2000/01/rdf-schema#label> ?label } }".into(),
        },
        SparqlAskAssertion {
            code: "faq_requires_label".into(),
            query: "ASK { ?faq a <https://agentos.ontology/ev/FAQ> . FILTER NOT EXISTS { ?faq <http://www.w3.org/2000/01/rdf-schema#label> ?label } }".into(),
        },
    ]
}

fn is_valid_code(code: &str) -> bool {
    !code.is_empty()
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn sparql_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::ontology_layer::ev_repair_ontology;

    #[test]
    fn defaults_preserve_the_existing_non_high_risk_policy() {
        let ontology = ev_repair_ontology();
        let action = ontology
            .action_types
            .iter()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        let policy = effective_config(&ontology, action).unwrap();
        assert_eq!(policy.max_triples, DEFAULT_MAX_TRIPLES);
        assert_eq!(
            policy.allowed_predicate_prefixes,
            DEFAULT_ALLOWED_PREDICATE_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_owned())
                .collect::<Vec<_>>()
        );
        assert!(!is_high_risk(&policy));
    }

    #[test]
    fn expanded_whitelist_requires_hitl() {
        let mut ontology = ev_repair_ontology();
        let action = ontology
            .action_types
            .iter_mut()
            .find(|action| action.id == "GenerateRepairOrder")
            .unwrap();
        action.guardrails.allowed_predicate_prefixes =
            Some(vec!["https://additional.example/".into()]);
        let action = action.clone();
        assert!(is_high_risk(&effective_config(&ontology, &action).unwrap()));
    }
}
