use std::path::PathBuf;

use hyperspace_engine::hnsw::HnswConfig;
use hyperspace_engine::metric::CosineMetric;
use hyperspace_engine::wal::WalSyncMode;
use hyperspace_engine::HyperspaceEngineImpl;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut data_dir = None;
    let mut dimension = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(
                    args.next().ok_or("--data-dir requires a path")?,
                ));
            }
            "--dimension" => {
                dimension = Some(
                    args.next()
                        .ok_or("--dimension requires a value")?
                        .parse::<usize>()?,
                );
            }
            "--help" | "-h" => {
                println!(
                    "Usage: vector-store-check --data-dir <PATH> --dimension <N>\n\
                     \n\
                     Reports the number of persisted vectors that have no text payload."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg} (try --help)").into()),
        }
    }

    let data_dir = data_dir.ok_or("--data-dir is required")?;
    let dimension = dimension.ok_or("--dimension is required")?;
    let store = HyperspaceEngineImpl::open(
        &data_dir,
        WalSyncMode::Strict,
        dimension,
        Box::new(CosineMetric),
        HnswConfig::default(),
    )?;
    let health = store.persistence_health_report();
    println!(
        "{}",
        serde_json::json!({
            "data_dir": data_dir,
            "vector_count": health.vector_count,
            "missing_text_count": health.missing_text_count,
        })
    );
    Ok(())
}
