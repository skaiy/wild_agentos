//! Medium-speed, fail-closed supervision for constrained extraction staging.
//!
//! The gate deliberately has no production-graph operation. Its evidence is
//! attached to the extraction's claims-derived staging graph for HITL review.

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::{
    isolation::IsolationClaims,
    knowledge_graph::{ontology_layer::SparqlAskAssertion, store::KnowledgeGraphStore},
};

pub const DEFAULT_POLICY_VERSION: &str = "kg-quality-gate/v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityGateRequest {
    #[serde(default)]
    pub assertions: Vec<SparqlAskAssertion>,
    #[serde(default = "default_policy_version")]
    pub policy_version: String,
    /// Compliance is deliberately the default when coverage conflicts with a
    /// deterministic policy. No candidate count can relax an anchor.
    #[serde(default = "default_arbitration")]
    pub arbitration: QualityCoverageArbitration,
    /// Turtle shapes evaluated by a locally configured pySHACL executable.
    /// Supplying shapes is an explicit opt-in to the sidecar.
    #[serde(default)]
    pub pyshacl_shapes_ttl: Option<String>,
    /// Explicitly enables the source-grounded LLM Judge for this review. The
    /// Judge is called only after every deterministic anchor has passed.
    #[serde(default)]
    pub judge: Option<JudgeConfig>,
}

fn default_policy_version() -> String {
    DEFAULT_POLICY_VERSION.to_string()
}

fn default_arbitration() -> QualityCoverageArbitration {
    QualityCoverageArbitration::Compliance
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QualityCoverageArbitration {
    Compliance,
    Coverage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeConfig {
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JudgeReport {
    pub verdict: JudgeVerdict,
    pub rationale: String,
    #[serde(default)]
    pub source_citations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JudgeVerdict {
    Approve,
    NeedsReview,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityGateReport {
    pub extraction_id: String,
    pub policy_version: String,
    pub arbitration: QualityCoverageArbitration,
    pub ask: Vec<AskResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pyshacl: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeReport>,
    pub deterministic_passed: bool,
    pub passed: bool,
    pub review_status: String,
    pub production_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResult {
    pub code: String,
    /// Like existing Action guardrails, `true` means the ASK found a violation.
    pub violation: bool,
}

/// Boundary for an optional source-grounded LLM Judge. Production wiring can
/// inject a gateway-backed judge; tests use this boundary to prove its output
/// cannot overturn an anchor failure.
pub trait QualityJudge {
    fn judge(&self) -> Result<JudgeReport, String>;
}

pub struct KgQualityGate;

impl KgQualityGate {
    pub fn evaluate(
        kg: &KnowledgeGraphStore,
        claims: &IsolationClaims,
        extraction_id: &str,
        request: &QualityGateRequest,
        judge: Option<&dyn QualityJudge>,
    ) -> Result<QualityGateReport, String> {
        validate_request(request)?;
        let ask = request
            .assertions
            .iter()
            .map(|assertion| {
                let rows = kg.query_staging_for_claims(claims, extraction_id, &assertion.query)?;
                let violation = rows
                    .first()
                    .and_then(|row| row.get("result"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true);
                Ok(AskResult {
                    code: assertion.code.clone(),
                    violation,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let ask_passed = ask.iter().all(|result| !result.violation);

        // Sidecar execution is strictly after ASK and never happens for a
        // known deterministic failure. A configured/unavailable sidecar is a
        // failed anchor, not a warning.
        let pyshacl = if ask_passed {
            request
                .pyshacl_shapes_ttl
                .as_deref()
                .map(|shapes| run_pyshacl(kg, claims, extraction_id, shapes))
                .transpose()?
        } else {
            None
        };
        let shacl_passed = pyshacl
            .as_ref()
            .and_then(|report| report.get("conforms"))
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        let deterministic_passed = ask_passed && shacl_passed;

        // An LLM only sees a fully compliant staging graph. Its approval is
        // supervisory evidence, never authority to bypass anchors.
        let judge = if deterministic_passed {
            judge.map(|judge| judge.judge()).transpose()?
        } else {
            None
        };
        let judge_passed = judge
            .as_ref()
            .map(|report| report.verdict == JudgeVerdict::Approve)
            .unwrap_or(true);
        let passed = deterministic_passed && judge_passed;
        Ok(QualityGateReport {
            extraction_id: extraction_id.to_owned(),
            policy_version: request.policy_version.clone(),
            arbitration: request.arbitration.clone(),
            ask,
            pyshacl,
            judge,
            deterministic_passed,
            passed,
            review_status: if passed { "pending_review" } else { "blocked" }.to_string(),
            production_write: false,
        })
    }

    /// Applies a Judge outcome without weakening deterministic authority.
    pub fn apply_judge(mut report: QualityGateReport, judge: JudgeReport) -> QualityGateReport {
        if report.deterministic_passed {
            report.passed = judge.verdict == JudgeVerdict::Approve;
            report.review_status = if report.passed {
                "pending_review".into()
            } else {
                "blocked".into()
            };
            report.judge = Some(judge);
        }
        report
    }
}

pub fn validate_request(request: &QualityGateRequest) -> Result<(), String> {
    if request.policy_version.trim().is_empty() {
        return Err("quality gate policy_version is required".into());
    }
    for assertion in &request.assertions {
        if assertion.code.is_empty()
            || !assertion
                .code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(format!(
                "invalid quality gate assertion code: {}",
                assertion.code
            ));
        }
        let query = assertion.query.trim();
        if !query.to_ascii_uppercase().starts_with("ASK")
            || query.to_ascii_uppercase().contains("GRAPH")
        {
            return Err(format!(
                "quality gate assertion {} must be graph-free SPARQL ASK",
                assertion.code
            ));
        }
    }
    Ok(())
}

fn run_pyshacl(
    kg: &KnowledgeGraphStore,
    claims: &IsolationClaims,
    extraction_id: &str,
    shapes: &str,
) -> Result<serde_json::Value, String> {
    let command = std::env::var("AGENTOS_KG_PYSHACL_COMMAND").map_err(|_| {
        "pySHACL requested but AGENTOS_KG_PYSHACL_COMMAND is not configured".to_string()
    })?;
    if command.trim().is_empty() || command.contains(char::is_whitespace) {
        return Err("AGENTOS_KG_PYSHACL_COMMAND must be one executable path".into());
    }
    let dir = tempfile::Builder::new()
        .prefix("agentos-pyshacl-")
        .tempdir()
        .map_err(|e| format!("create pySHACL sidecar directory: {e}"))?;
    let data = dir.path().join("staging.nt");
    let shape = dir.path().join("shapes.ttl");
    std::fs::write(
        &data,
        kg.staging_ntriples_for_claims(claims, extraction_id)?,
    )
    .map_err(|e| format!("write pySHACL data: {e}"))?;
    std::fs::write(&shape, shapes).map_err(|e| format!("write pySHACL shapes: {e}"))?;
    let output = Command::new(command)
        .arg("--shacl")
        .arg(shape)
        .arg("--format")
        .arg("json")
        .arg(data)
        .output()
        .map_err(|e| format!("run pySHACL sidecar: {e}"))?;
    let mut report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("pySHACL did not return a JSON report: {e}"))?;
    if !output.status.success() {
        report["conforms"] = serde_json::Value::Bool(false);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::store::ClaimsGraphUpdate;

    struct ApprovingJudge;
    impl QualityJudge for ApprovingJudge {
        fn judge(&self) -> Result<JudgeReport, String> {
            Ok(JudgeReport {
                verdict: JudgeVerdict::Approve,
                rationale: "looks good".into(),
                source_citations: vec!["blob:v1".into()],
            })
        }
    }

    #[test]
    fn judge_cannot_overturn_an_ask_failure() {
        let store = std::sync::Arc::new(oxigraph::store::Store::new().unwrap());
        let kg = KnowledgeGraphStore::with_shared_store(store).unwrap();
        let claims = IsolationClaims::from_verified("tenant", "project", "actor").unwrap();
        kg.update_staging_for_claims(
            &claims,
            "extract1",
            &ClaimsGraphUpdate::insert_data("<urn:x> <urn:p> <urn:y> ."),
        )
        .unwrap();
        let request = QualityGateRequest {
            assertions: vec![SparqlAskAssertion {
                code: "always_fails".into(),
                query: "ASK { ?s ?p ?o }".into(),
            }],
            policy_version: DEFAULT_POLICY_VERSION.into(),
            arbitration: QualityCoverageArbitration::Coverage,
            pyshacl_shapes_ttl: None,
            judge: None,
        };
        let report =
            KgQualityGate::evaluate(&kg, &claims, "extract1", &request, Some(&ApprovingJudge))
                .unwrap();
        assert!(!report.deterministic_passed);
        assert!(!report.passed);
        assert!(
            report.judge.is_none(),
            "Judge must not run after an ASK failure"
        );
        assert_eq!(report.review_status, "blocked");
    }
}
