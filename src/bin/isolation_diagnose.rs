use std::path::PathBuf;

use wild_agent_os_core::isolation::diagnose::{diagnose_data_root, render_markdown};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut data_root = None;
    let mut json = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                data_root = Some(PathBuf::from(
                    args.next().ok_or("--data-root requires a path")?,
                ));
            }
            "--json" => json = true,
            "--help" | "-h" => {
                println!(
                    "Usage: isolation-diagnose --data-root <PATH> [--json]\n\
                     \n\
                     Read-only filesystem diagnostic for minted and historical isolation keys.\n\
                     It never opens, creates, migrates, or deletes a storage backend."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg} (try --help)").into()),
        }
    }

    let data_root = data_root.ok_or("--data-root is required")?;
    let diagnosis = diagnose_data_root(data_root)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&diagnosis)?);
    } else {
        println!("{}", render_markdown(&diagnosis));
        if !diagnosis.scan_warnings.is_empty() {
            eprintln!("\nScan warnings:");
            for warning in &diagnosis.scan_warnings {
                eprintln!("- {warning}");
            }
        }
    }
    Ok(())
}
