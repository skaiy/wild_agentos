use std::{fs, path::PathBuf};

use serde_json::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut data_root = None;
    let mut execute = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                data_root = Some(PathBuf::from(
                    args.next().ok_or("--data-root requires a path")?,
                ))
            }
            "--execute" => execute = true,
            "--help" | "-h" => {
                println!("Usage: blob-key-migrate --data-root <PATH> [--execute]\nDefault is dry-run. Copies legacy local BlobStore objects without deleting sources.");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let root = data_root.ok_or("--data-root is required")?;
    let bases: Vec<Value> = serde_json::from_slice(&fs::read(root.join("knowledge_bases.json"))?)?;
    let mut copied = 0usize;
    let mut kb = 0usize;
    for base in &bases {
        let (Some(tenant), Some(project), Some(id)) = (
            base.get("tenant_id").and_then(Value::as_str),
            base.get("project_id").and_then(Value::as_str),
            base.get("id").and_then(Value::as_str),
        ) else {
            continue;
        };
        for doc in base
            .get("documents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(doc_id) = doc.get("doc_id").and_then(Value::as_str) {
                let old = root
                    .join("blobs")
                    .join(tenant)
                    .join("kb")
                    .join(id)
                    .join(doc_id);
                let new = root
                    .join("blobs")
                    .join(tenant)
                    .join(project)
                    .join("kb")
                    .join(id)
                    .join(doc_id);
                if old.is_file() {
                    kb += 1;
                    if execute {
                        if !new.exists() {
                            fs::create_dir_all(new.parent().unwrap())?;
                            fs::copy(old, new)?;
                            copied += 1;
                        }
                    }
                }
            }
        }
    }
    let mut images = 0usize;
    let mut ambiguous = 0usize;
    let mut orphan = 0usize;
    let blobs = root.join("blobs");
    for tenant in fs::read_dir(&blobs)? {
        let tenant = tenant?;
        let tenant_name = tenant.file_name().to_string_lossy().into_owned();
        let image_dir = tenant.path().join("images");
        if !image_dir.is_dir() {
            continue;
        }
        for image in fs::read_dir(image_dir)? {
            let image = image?;
            if !image.file_type()?.is_file() {
                continue;
            }
            let id = image.file_name().to_string_lossy().into_owned();
            let mut owners: Vec<_> = bases
                .iter()
                .filter(|base| {
                    wild_agent_os_core::api::http::models::value_references_image(base, &id)
                })
                .filter_map(|base| {
                    Some((
                        base.get("tenant_id")?.as_str()?.to_owned(),
                        base.get("project_id")?.as_str()?.to_owned(),
                    ))
                })
                .collect();
            owners.sort();
            owners.dedup();
            match owners.as_slice() {
                [(owner_tenant, project)] if owner_tenant == &tenant_name => {
                    images += 1;
                    if execute {
                        let target = blobs
                            .join(owner_tenant)
                            .join(project)
                            .join("images")
                            .join(&id);
                        if !target.exists() {
                            fs::create_dir_all(target.parent().unwrap())?;
                            fs::copy(image.path(), target)?;
                            copied += 1;
                        }
                    }
                }
                [] => orphan += 1,
                _ => ambiguous += 1,
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"dry_run": !execute, "kb_migratable": kb, "image_migratable": images, "image_ambiguous": ambiguous, "image_orphan": orphan, "copied": copied})
    );
    Ok(())
}
