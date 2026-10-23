//! Explicit, idempotent scope backfill for legacy KB catalog entries.
use std::{collections::BTreeSet, fs, path::PathBuf};

use serde_json::{json, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut data_root = None;
    let mut apply = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                data_root = Some(PathBuf::from(
                    args.next().ok_or("--data-root requires a path")?,
                ))
            }
            "--apply" => apply = true,
            "--help" | "-h" => {
                println!(
                    "Usage: kb-catalog-backfill --data-root <PATH> [--apply]\nDefault is dry-run."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let root = data_root.ok_or("--data-root is required")?;
    let path = root.join("knowledge_bases.json");
    let mut bases: Vec<Value> = serde_json::from_slice(&fs::read(&path)?)?;
    let mut report = Vec::new();
    let mut changed = 0usize;
    for base in &mut bases {
        if base.get("tenant_id").and_then(Value::as_str).is_some()
            && base.get("project_id").and_then(Value::as_str).is_some()
        {
            continue;
        }
        let mut evidence = BTreeSet::new();
        for document in base
            .get("documents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(tenant), Some(project)) = (
                document.get("tenant_id").and_then(Value::as_str),
                document.get("project_id").and_then(Value::as_str),
            ) {
                evidence.insert((tenant.to_owned(), project.to_owned()));
            }
        }
        let id = base
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_owned();
        if evidence.len() == 1 {
            let (tenant, project) = evidence.into_iter().next().unwrap();
            report.push(json!({"id": id, "status": "unambiguous", "tenant_id": tenant, "project_id": project}));
            if apply {
                base["tenant_id"] = json!(tenant);
                base["project_id"] = json!(project);
                changed += 1;
            }
        } else {
            report.push(json!({"id": id, "status": if evidence.is_empty() {"no_evidence"} else {"ambiguous"}, "evidence": evidence}));
        }
    }
    if apply && changed > 0 {
        fs::write(&path, serde_json::to_vec_pretty(&bases)?)?;
    }
    println!(
        "{}",
        json!({"dry_run": !apply, "changed": changed, "records": report})
    );
    Ok(())
}
