//! Explicit, idempotent scope backfill for legacy KB catalog entries.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

fn parse_assignment(value: &str) -> Result<(String, (String, String)), String> {
    let (id, scope) = value
        .split_once('=')
        .ok_or("assignment must be <kb_id>=<tenant>/<project>")?;
    let (tenant, project) = scope
        .split_once('/')
        .ok_or("assignment must be <kb_id>=<tenant>/<project>")?;
    if id.is_empty() || tenant.is_empty() || project.is_empty() || project.contains('/') {
        return Err("assignment must be <kb_id>=<tenant>/<project>".into());
    }
    Ok((id.into(), (tenant.into(), project.into())))
}

fn backfill(
    bases: &mut [Value],
    apply: bool,
    assigns: &BTreeMap<String, (String, String)>,
) -> Result<(usize, Vec<Value>), String> {
    let mut report = Vec::new();
    let mut changed = 0usize;
    for base in &mut *bases {
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
        if evidence.is_empty() {
            if let Some(scope) = assigns.get(&id) {
                evidence.insert(scope.clone());
            }
        }
        if evidence.len() == 1 {
            let (tenant, project) = evidence.into_iter().next().unwrap();
            let tenant_conflict = base
                .get("tenant_id")
                .and_then(Value::as_str)
                .is_some_and(|value| value != tenant);
            let project_conflict = base
                .get("project_id")
                .and_then(Value::as_str)
                .is_some_and(|value| value != project);
            if tenant_conflict || project_conflict {
                report.push(json!({"id": id, "status": "conflict"}));
            } else {
                report.push(json!({"id": id, "status": if assigns.contains_key(&id) {"assigned"} else {"unambiguous"}, "tenant_id": tenant, "project_id": project}));
                if apply {
                    if base.get("tenant_id").and_then(Value::as_str).is_none() {
                        base["tenant_id"] = json!(tenant);
                    }
                    if base.get("project_id").and_then(Value::as_str).is_none() {
                        base["project_id"] = json!(project);
                    }
                    changed += 1;
                }
            }
        } else {
            report.push(json!({"id": id, "status": if evidence.is_empty() {"no_evidence"} else {"ambiguous"}, "evidence": evidence}));
        }
    }
    for id in assigns.keys() {
        if !bases
            .iter()
            .any(|base| base.get("id").and_then(Value::as_str) == Some(id))
        {
            return Err(format!("assigned KB id does not exist: {id}"));
        }
    }
    Ok((changed, report))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("catalog path has no parent"))?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("catalog")
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut data_root = None;
    let mut apply = false;
    let mut assigns = BTreeMap::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                data_root = Some(PathBuf::from(
                    args.next().ok_or("--data-root requires a path")?,
                ))
            }
            "--apply" => apply = true,
            "--assign" => {
                let (id, scope) =
                    parse_assignment(&args.next().ok_or("--assign requires a value")?)?;
                if assigns.insert(id.clone(), scope).is_some() {
                    return Err(format!("duplicate assignment for KB id: {id}").into());
                }
            }
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
    let (changed, report) = backfill(&mut bases, apply, &assigns)?;
    if apply && changed > 0 {
        atomic_write(&path, &serde_json::to_vec_pretty(&bases)?)?;
    }
    println!(
        "{}",
        json!({"dry_run": !apply, "changed": changed, "records": report})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dry_run_apply_skip_and_rerun() {
        let mut bases = vec![
            json!({"id":"unique","documents":[{"tenant_id":"t","project_id":"p"}]}),
            json!({"id":"ambiguous","documents":[{"tenant_id":"a","project_id":"p"},{"tenant_id":"b","project_id":"p"}]}),
            json!({"id":"missing","documents":[]}),
        ];
        let original = bases.clone();
        assert_eq!(backfill(&mut bases, false, &BTreeMap::new()).unwrap().0, 0);
        assert_eq!(bases, original);
        assert_eq!(backfill(&mut bases, true, &BTreeMap::new()).unwrap().0, 1);
        assert_eq!(bases[0]["tenant_id"], "t");
        assert_eq!(backfill(&mut bases, true, &BTreeMap::new()).unwrap().0, 0);
    }

    #[test]
    fn conflicting_existing_scope_is_reported_without_overwrite() {
        let mut bases = vec![json!({
            "id":"conflict",
            "tenant_id":"existing",
            "documents":[{"tenant_id":"evidence","project_id":"project"}]
        })];
        let (changed, report) = backfill(&mut bases, true, &BTreeMap::new()).unwrap();
        assert_eq!(changed, 0);
        assert_eq!(bases[0]["tenant_id"], "existing");
        assert!(bases[0].get("project_id").is_none());
        assert_eq!(report[0]["status"], "conflict");
    }

    #[test]
    fn assignment_parser_rejects_invalid_and_duplicate_ids() {
        assert!(parse_assignment("bad").is_err());
        assert!(parse_assignment("id=tenant").is_err());
        let (id, scope) = parse_assignment("kb-a=tenant/project").unwrap();
        assert_eq!(id, "kb-a");
        assert_eq!(scope, ("tenant".into(), "project".into()));
        let mut assignments = BTreeMap::new();
        assert!(assignments.insert(id.clone(), scope).is_none());
        assert!(assignments
            .insert(id, ("other".into(), "project".into()))
            .is_some());
    }

    #[test]
    fn assignment_applies_only_missing_scope_and_rejects_unknown_ids() {
        let mut bases = vec![
            json!({"id":"assigned","documents":[]}),
            json!({
                "id":"conflict","tenant_id":"existing",
                "documents":[{"tenant_id":"evidence","project_id":"project"}]
            }),
        ];
        let mut assignments = BTreeMap::new();
        assignments.insert("assigned".into(), ("tenant".into(), "project".into()));
        let (changed, report) = backfill(&mut bases, true, &assignments).unwrap();
        assert_eq!(changed, 1);
        assert_eq!(bases[0]["tenant_id"], "tenant");
        assert_eq!(report[0]["status"], "assigned");
        assert_eq!(bases[1]["tenant_id"], "existing");
        let mut unknown = BTreeMap::new();
        unknown.insert("missing".into(), ("tenant".into(), "project".into()));
        assert!(backfill(&mut bases, false, &unknown).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge_bases.json");
        atomic_write(&path, b"[]").unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn atomic_write_failure_keeps_original_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog");
        fs::write(&path, b"original").unwrap();
        let error = atomic_write(&path.join("child"), b"new").unwrap_err();
        assert!(matches!(error.kind(), std::io::ErrorKind::NotADirectory));
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }
}
