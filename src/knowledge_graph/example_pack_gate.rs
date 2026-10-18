//! Knowledge example-pack admission gate (filesystem contract + SHACL + golden I/O).
//!
//! Showcase packs live under `example-packs/` and are validated in CI without
//! executing pack scripts or touching production tenant graphs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::knowledge_graph::ontology_layer::{
    ev_repair_ontology, EV_REPAIR_KNOWLEDGE_PACK_ID, EV_REPAIR_RUNTIME_ASSET_ID,
};
use crate::ontology::OntologyPipeline;
use crate::tools::skill_pipeline::SkillPackageManifest;

pub const KNOWLEDGE_PACK_SCHEMA: &str = "agentos.dev/knowledge-pack/v1";
pub const EV_REPAIR_EXAMPLE_PACK_DIR: &str = "example-packs/ev-repair-fault-kb";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgePackGateReport {
    pub gate: String,
    pub pack_id: String,
    pub domain: String,
    pub shacl_conforms: bool,
    pub ontology_anchors_ok: bool,
    pub skill_package_count: usize,
    pub audit_sample_count: usize,
    pub fictional_data_only: bool,
}

#[derive(Debug, Clone)]
struct KnowledgePackManifest {
    #[allow(dead_code)]
    schema: String,
    pack_id: String,
    domain: String,
    fictional_data_only: bool,
    runtime_asset_id: String,
    ontology_anchors: String,
    shacl_shapes: String,
    sample_data: String,
    skills_dir: String,
    audit_samples: String,
    golden_input: String,
    golden_output: String,
    golden_input_sha256: String,
    golden_output_sha256: String,
}

#[derive(Debug, Deserialize)]
struct OntologyAnchors {
    domain: String,
    required_object_types: Vec<String>,
    required_link_types: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuditSamplesFile {
    schema: String,
    fictional_data_only: bool,
    samples: Vec<AuditSample>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AuditSample {
    id: String,
    tenant_id: String,
    project_id: String,
    actor_id: String,
    action: String,
    resource: String,
    isolation_verified: bool,
}

#[derive(Debug, Deserialize)]
struct GoldenInputContract {
    pack_id: String,
    domain: String,
    validate_shacl: bool,
    min_skill_packages: usize,
    min_audit_samples: usize,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct GoldenOutputContract {
    gate: String,
    pack_id: String,
    domain: String,
    shacl_conforms: bool,
    ontology_anchors_ok: bool,
    skill_package_count: usize,
    audit_sample_count: usize,
    fictional_data_only: bool,
}

pub fn default_ev_repair_pack_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(EV_REPAIR_EXAMPLE_PACK_DIR)
}

/// Run the full admission gate for a knowledge example pack directory.
pub fn run_knowledge_pack_gate(pack_dir: &Path) -> Result<KnowledgePackGateReport, String> {
    let manifest = load_pack_manifest(pack_dir)?;
    let golden_input = load_golden_input(pack_dir, &manifest)?;
    verify_frozen_fixture(
        &pack_dir.join(&manifest.golden_input),
        &manifest.golden_input_sha256,
        "pack golden input",
    )?;
    verify_frozen_fixture(
        &pack_dir.join(&manifest.golden_output),
        &manifest.golden_output_sha256,
        "pack golden output",
    )?;

    if manifest.pack_id != golden_input.pack_id {
        return Err("pack metadata id does not match golden-input pack_id".into());
    }
    if manifest.domain != golden_input.domain {
        return Err("pack domain does not match golden-input domain".into());
    }
    if manifest.pack_id != EV_REPAIR_KNOWLEDGE_PACK_ID {
        return Err(format!(
            "showcase gate only admits the built-in pack id {EV_REPAIR_KNOWLEDGE_PACK_ID}"
        ));
    }
    if manifest.runtime_asset_id != EV_REPAIR_RUNTIME_ASSET_ID {
        return Err("pack runtime_asset_id must match the ev-repair alias".into());
    }
    if !manifest.fictional_data_only {
        return Err("showcase packs must declare fictional_data_only: true".into());
    }

    let ontology_anchors_ok = verify_ontology_anchors(pack_dir, &manifest)?;
    let shacl_conforms = if golden_input.validate_shacl {
        verify_shacl_sample(pack_dir, &manifest)?
    } else {
        true
    };
    let skill_package_count = verify_skill_packages(pack_dir, &manifest)?;
    if skill_package_count < golden_input.min_skill_packages {
        return Err(format!(
            "expected at least {} skill packages, found {}",
            golden_input.min_skill_packages, skill_package_count
        ));
    }
    let audit_sample_count = verify_audit_samples(pack_dir, &manifest)?;
    if audit_sample_count < golden_input.min_audit_samples {
        return Err(format!(
            "expected at least {} audit samples, found {}",
            golden_input.min_audit_samples, audit_sample_count
        ));
    }

    let report = KnowledgePackGateReport {
        gate: "pass".into(),
        pack_id: manifest.pack_id.clone(),
        domain: manifest.domain.clone(),
        shacl_conforms,
        ontology_anchors_ok,
        skill_package_count,
        audit_sample_count,
        fictional_data_only: manifest.fictional_data_only,
    };

    let expected: GoldenOutputContract = serde_json::from_slice(
        &std::fs::read(pack_dir.join(&manifest.golden_output))
            .map_err(|e| format!("pack golden output unreadable: {e}"))?,
    )
    .map_err(|e| format!("pack golden output invalid JSON: {e}"))?;
    let actual = GoldenOutputContract {
        gate: report.gate.clone(),
        pack_id: report.pack_id.clone(),
        domain: report.domain.clone(),
        shacl_conforms: report.shacl_conforms,
        ontology_anchors_ok: report.ontology_anchors_ok,
        skill_package_count: report.skill_package_count,
        audit_sample_count: report.audit_sample_count,
        fictional_data_only: report.fictional_data_only,
    };
    if actual != expected {
        return Err(format!(
            "pack gate report does not match frozen golden-output contract: actual={:?} expected={:?}",
            actual,
            expected
        ));
    }

    Ok(report)
}

fn load_pack_manifest(pack_dir: &Path) -> Result<KnowledgePackManifest, String> {
    let yaml = std::fs::read_to_string(pack_dir.join("pack.yaml"))
        .map_err(|_| "knowledge pack is missing readable pack.yaml".to_string())?;
    let values = parse_flat_yaml(&yaml);
    let schema = field(&values, "package.schema")?;
    if schema != KNOWLEDGE_PACK_SCHEMA {
        return Err(format!("unsupported package.schema: {schema}"));
    }
    let pack_id = field(&values, "metadata.id")?;
    let domain = field(&values, "spec.domain")?;
    let fictional = values
        .get("spec.fictional_data_only")
        .map(|v| v == "true")
        .unwrap_or(false);
    Ok(KnowledgePackManifest {
        schema,
        pack_id,
        domain,
        fictional_data_only: fictional,
        runtime_asset_id: field(&values, "spec.runtime_asset_id")?,
        ontology_anchors: field(&values, "package.ontology_anchors")?,
        shacl_shapes: field(&values, "package.shacl_shapes")?,
        sample_data: field(&values, "package.sample_data")?,
        skills_dir: field(&values, "package.skills_dir")?,
        audit_samples: field(&values, "package.audit_samples")?,
        golden_input: field(&values, "package.golden_input")?,
        golden_output: field(&values, "package.golden_output")?,
        golden_input_sha256: field(&values, "package.golden_input_sha256")?,
        golden_output_sha256: field(&values, "package.golden_output_sha256")?,
    })
}

fn load_golden_input(
    pack_dir: &Path,
    manifest: &KnowledgePackManifest,
) -> Result<GoldenInputContract, String> {
    let path = pack_dir.join(&manifest.golden_input);
    let bytes = std::fs::read(&path).map_err(|e| format!("golden input unreadable: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("golden input invalid JSON: {e}"))
}

fn verify_ontology_anchors(
    pack_dir: &Path,
    manifest: &KnowledgePackManifest,
) -> Result<bool, String> {
    let path = pack_dir.join(&manifest.ontology_anchors);
    let anchors: OntologyAnchors = serde_json::from_slice(
        &std::fs::read(&path).map_err(|e| format!("ontology anchors unreadable: {e}"))?,
    )
    .map_err(|e| format!("ontology anchors invalid JSON: {e}"))?;
    if anchors.domain != manifest.domain {
        return Err("ontology anchors domain mismatch".into());
    }
    let seed = ev_repair_ontology();
    if seed.domain != anchors.domain {
        return Err("seed ontology domain mismatch".into());
    }
    let object_ids: HashSet<&str> = seed.object_types.iter().map(|o| o.id.as_str()).collect();
    let link_ids: HashSet<&str> = seed.link_types.iter().map(|l| l.id.as_str()).collect();
    for id in &anchors.required_object_types {
        if !object_ids.contains(id.as_str()) {
            return Err(format!(
                "anchor object type {id} is not in seeded ev-repair ontology"
            ));
        }
    }
    for id in &anchors.required_link_types {
        if !link_ids.contains(id.as_str()) {
            return Err(format!(
                "anchor link type {id} is not in seeded ev-repair ontology"
            ));
        }
    }
    Ok(true)
}

fn verify_shacl_sample(pack_dir: &Path, manifest: &KnowledgePackManifest) -> Result<bool, String> {
    let shapes = std::fs::read_to_string(pack_dir.join(&manifest.shacl_shapes))
        .map_err(|e| format!("SHACL shapes unreadable: {e}"))?;
    let data = std::fs::read_to_string(pack_dir.join(&manifest.sample_data))
        .map_err(|e| format!("sample data unreadable: {e}"))?;
    let store = crate::ontology::new_shared_store()
        .map_err(|e| format!("failed to create ontology store: {e}"))?;
    store
        .load_turtle(&data, None)
        .map_err(|e| format!("failed to load sample Turtle: {e}"))?;
    let report = store
        .validate_shacl(&shapes)
        .map_err(|e| format!("SHACL validation failed: {e}"))?;
    let parsed: Value =
        serde_json::from_str(&report).map_err(|e| format!("SHACL report not JSON: {e}"))?;
    let conforms = parsed
        .get("conforms")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !conforms {
        return Err(format!(
            "fictional sample data does not conform to SHACL: {report}"
        ));
    }
    Ok(true)
}

fn verify_skill_packages(
    pack_dir: &Path,
    manifest: &KnowledgePackManifest,
) -> Result<usize, String> {
    let skills_root = pack_dir.join(&manifest.skills_dir);
    if !skills_root.is_dir() {
        return Err("skills_dir is missing or not a directory".into());
    }
    let mut count = 0usize;
    for entry in
        std::fs::read_dir(&skills_root).map_err(|e| format!("cannot read skills_dir: {e}"))?
    {
        let entry = entry.map_err(|e| format!("cannot read skills_dir entry: {e}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_yaml = path.join("skill.yaml");
        if !skill_yaml.is_file() {
            continue;
        }
        let yaml = std::fs::read_to_string(&skill_yaml)
            .map_err(|e| format!("skill.yaml unreadable: {e}"))?;
        let skill_manifest = SkillPackageManifest::parse(&yaml)?;
        for (rel, digest, label) in [
            (
                skill_manifest.golden_input.as_str(),
                skill_manifest.golden_input_sha256.as_deref(),
                "skill golden input",
            ),
            (
                skill_manifest.golden_output.as_str(),
                skill_manifest.golden_output_sha256.as_deref(),
                "skill golden output",
            ),
        ] {
            let expected =
                digest.ok_or_else(|| format!("{label} must declare a SHA-256 digest"))?;
            verify_frozen_fixture(&path.join(rel), expected, label)?;
        }
        if !path.join(&skill_manifest.entrypoint).is_file() {
            return Err(format!(
                "skill package entrypoint missing: {}",
                path.join(&skill_manifest.entrypoint).display()
            ));
        }
        count += 1;
    }
    if count == 0 {
        return Err("skills_dir contains no skill packages".into());
    }
    Ok(count)
}

fn verify_audit_samples(
    pack_dir: &Path,
    manifest: &KnowledgePackManifest,
) -> Result<usize, String> {
    let path = pack_dir.join(&manifest.audit_samples);
    let file: AuditSamplesFile = serde_json::from_slice(
        &std::fs::read(&path).map_err(|e| format!("audit samples unreadable: {e}"))?,
    )
    .map_err(|e| format!("audit samples invalid JSON: {e}"))?;
    if file.schema != "agentos.dev/knowledge-pack-audit/v1" {
        return Err("audit samples schema mismatch".into());
    }
    if !file.fictional_data_only {
        return Err("audit samples must declare fictional_data_only: true".into());
    }
    for sample in &file.samples {
        if sample.id.trim().is_empty()
            || sample.tenant_id.trim().is_empty()
            || sample.project_id.trim().is_empty()
        {
            return Err("audit sample identity fields cannot be empty".into());
        }
        if !sample.isolation_verified {
            return Err(format!(
                "audit sample {} must keep isolation_verified=true",
                sample.id
            ));
        }
        if sample.tenant_id == "default" {
            return Err("audit samples must not use production default tenant id".into());
        }
    }
    Ok(file.samples.len())
}

fn verify_frozen_fixture(path: &Path, expected: &str, label: &str) -> Result<(), String> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{label} SHA-256 digest must be 64 hex characters"));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{label} cannot be read: {e}"))?;
    let actual = hex::encode(Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "{label} no longer matches its frozen SHA-256 digest; refusing evaluation"
        ));
    }
    Ok(())
}

fn parse_flat_yaml(yaml: &str) -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    let mut section = String::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some((key, raw)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = raw.trim().trim_matches('"').trim_matches('\'').to_string();
            if indent == 0 && value.is_empty() {
                section = key.to_string();
            } else if !value.is_empty() {
                let key = if section.is_empty() || indent == 0 {
                    key.to_string()
                } else {
                    format!("{section}.{key}")
                };
                values.insert(key, value);
            }
        }
    }
    values
}

fn field(values: &std::collections::HashMap<String, String>, key: &str) -> Result<String, String> {
    values
        .get(key)
        .cloned()
        .ok_or_else(|| format!("pack.yaml missing {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn knowledge_pack_gate_accepts_ev_repair_example_pack() {
        let dir = default_ev_repair_pack_dir();
        let report = run_knowledge_pack_gate(&dir).expect("ev-repair example pack should pass");
        assert_eq!(report.gate, "pass");
        assert_eq!(report.pack_id, EV_REPAIR_KNOWLEDGE_PACK_ID);
        assert!(report.shacl_conforms);
        assert!(report.ontology_anchors_ok);
        assert!(report.fictional_data_only);
        assert!(report.skill_package_count >= 1);
        assert!(report.audit_sample_count >= 2);
    }

    #[test]
    fn knowledge_pack_gate_rejects_broken_shacl_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("broken-pack");
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::create_dir_all(root.join("ontology")).unwrap();
        std::fs::create_dir_all(root.join("shacl")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        let skill_dir = root.join("skills/fake-skill");
        std::fs::create_dir_all(skill_dir.join("tests")).unwrap();
        std::fs::create_dir_all(root.join("audit")).unwrap();

        let golden_in = json!({
            "pack_id": EV_REPAIR_KNOWLEDGE_PACK_ID,
            "domain": "ev-repair",
            "validate_shacl": true,
            "min_skill_packages": 1,
            "min_audit_samples": 0
        });
        let golden_out = json!({
            "gate": "pass",
            "pack_id": EV_REPAIR_KNOWLEDGE_PACK_ID,
            "domain": "ev-repair",
            "shacl_conforms": true,
            "ontology_anchors_ok": true,
            "skill_package_count": 1,
            "audit_sample_count": 0,
            "fictional_data_only": true
        });
        let gi = serde_json::to_vec(&golden_in).unwrap();
        let go = serde_json::to_vec(&golden_out).unwrap();
        std::fs::write(root.join("tests/golden-input.json"), &gi).unwrap();
        std::fs::write(root.join("tests/golden-output.json"), &go).unwrap();
        let gi_sha = hex::encode(Sha256::digest(&gi));
        let go_sha = hex::encode(Sha256::digest(&go));

        std::fs::write(
            root.join("ontology/anchors.json"),
            br#"{"domain":"ev-repair","required_object_types":["FaultCode"],"required_link_types":["affectsSystem"]}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("shacl/bad.ttl"),
            br#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ev: <https://agentos.ontology/ev/> .
ev:Shape a sh:NodeShape ; sh:targetClass ev:FaultCode ; sh:property [
  sh:path <https://agentos.ontology/ev/prop/missing> ; sh:minCount 1 ] ."#,
        )
        .unwrap();
        std::fs::write(
            root.join("data/bad.ttl"),
            br#"@prefix ev: <https://agentos.ontology/ev/> .
<https://agentos.ontology/ev/FaultCode/BAD-X> a ev:FaultCode ."#,
        )
        .unwrap();
        std::fs::write(
            root.join("audit/samples.json"),
            br#"{"schema":"agentos.dev/knowledge-pack-audit/v1","fictional_data_only":true,"samples":[]}"#,
        )
        .unwrap();
        let ski = br#"{"q":"x"}"#;
        let sko = br#"{"result":"y"}"#;
        std::fs::write(skill_dir.join("tests/golden-input.json"), ski).unwrap();
        std::fs::write(skill_dir.join("tests/golden-output.json"), sko).unwrap();
        let ski_sha = hex::encode(Sha256::digest(ski));
        let sko_sha = hex::encode(Sha256::digest(sko));
        std::fs::write(skill_dir.join("SKILL.md"), b"# fake").unwrap();
        std::fs::write(
            skill_dir.join("skill.yaml"),
            format!(
                r#"package:
  schema: agentos.dev/skill-package/v1
  side_effect_level: read
  visibility: tenant
  entrypoint: SKILL.md
  golden_input: tests/golden-input.json
  golden_output: tests/golden-output.json
  golden_input_sha256: {ski_sha}
  golden_output_sha256: {sko_sha}
"#
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("pack.yaml"),
            format!(
                r#"metadata:
  id: {EV_REPAIR_KNOWLEDGE_PACK_ID}
spec:
  domain: ev-repair
  fictional_data_only: true
  runtime_asset_id: {EV_REPAIR_RUNTIME_ASSET_ID}
package:
  schema: {KNOWLEDGE_PACK_SCHEMA}
  ontology_anchors: ontology/anchors.json
  shacl_shapes: shacl/bad.ttl
  sample_data: data/bad.ttl
  skills_dir: skills
  audit_samples: audit/samples.json
  golden_input: tests/golden-input.json
  golden_output: tests/golden-output.json
  golden_input_sha256: {gi_sha}
  golden_output_sha256: {go_sha}
"#
            ),
        )
        .unwrap();

        let err = run_knowledge_pack_gate(&root).unwrap_err();
        assert!(
            err.contains("SHACL") || err.contains("conform"),
            "expected SHACL failure, got: {err}"
        );
    }
}
