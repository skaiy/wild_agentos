use std::path::PathBuf;

use oxigraph::store::Store;
use wild_agent_os_core::isolation::migrate::{migrate, plan, read_plan, write_audit_log};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut data_root = None;
    let mut plan_path = None;
    let mut execute = false;
    let mut delete_source = false;
    let mut confirm_delete_source = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                data_root = Some(PathBuf::from(
                    args.next().ok_or("--data-root requires a path")?,
                ))
            }
            "--plan" => {
                plan_path = Some(PathBuf::from(args.next().ok_or("--plan requires a path")?))
            }
            "--execute" => execute = true,
            "--delete-source" => delete_source = true,
            "--confirm-delete-source" => confirm_delete_source = true,
            "--help" | "-h" => {
                println!(
                    "Usage: isolation-migrate --data-root <PATH> --plan <PLAN.json> [--execute]\n\
                     \n\
                     Plans named-graph migration by default. --execute runs plan → copy → verify.\n\
                     The graph store is <PATH>/kg. Source graphs remain intact unless both\n\
                     --delete-source and --confirm-delete-source are provided."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg} (try --help)").into()),
        }
    }

    let data_root = data_root.ok_or("--data-root is required")?;
    let plan_path = plan_path.ok_or("--plan is required")?;
    let migration_plan = read_plan(plan_path)?;
    if execute {
        // This CLI is explicitly offline. It is not reachable from HTTP handlers.
        let store = Store::open(data_root.join("kg"))?;
        let report = migrate(
            &store,
            &migration_plan,
            delete_source,
            confirm_delete_source,
        )?;
        let audit = write_audit_log(&data_root, &report, delete_source)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        eprintln!("audit log: {}", audit.display());
    } else {
        if delete_source || confirm_delete_source {
            return Err("--delete-source flags require --execute".into());
        }
        // Oxigraph's read-only open prevents a dry-run from creating a RocksDB
        // directory, WAL, or audit file.
        let store = Store::open_read_only(data_root.join("kg"))?;
        let report = plan(&store, &migration_plan)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}
