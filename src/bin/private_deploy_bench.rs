//! Reproducible single-node storage benchmark for private deployments.
//!
//! This binary intentionally records measurements instead of asserting or
//! publishing performance claims. Run it through `scripts/bench_private_deploy.sh`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use hyperspace_engine::engine::{HyperspaceEngine, HyperspaceEngineImpl};
use hyperspace_engine::hnsw::HnswConfig;
use hyperspace_engine::hyper_vector::{EmbeddingVector, MetricKind};
use hyperspace_engine::metric::CosineMetric;
use hyperspace_engine::wal::WalSyncMode;
use oxigraph::model::{NamedNode, Quad};
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde_json::{json, Value};

const DEFAULT_RECORDS: usize = 10_000;
const MICRO_RECORDS: usize = 1_000;
const DIMENSIONS: usize = 64;
const QUERY_ITERATIONS: usize = 100;
const L0_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("l0_entries");

#[derive(Serialize)]
struct LatencyPercentiles {
    unit: &'static str,
    samples: usize,
    p50: f64,
    p95: f64,
    p99: f64,
    min: f64,
    max: f64,
}

#[derive(Serialize)]
struct MachineSpec {
    os: String,
    arch: String,
    cpu_logical_cores: usize,
    cpu_model: Option<String>,
    memory_total_bytes: Option<u64>,
}

#[derive(Serialize)]
struct DiskUsage {
    oxigraph_bytes: u64,
    redb_bytes: u64,
    hyperspace_bytes: u64,
    total_bytes: u64,
}

#[derive(Serialize)]
struct MemoryUsage {
    rss_bytes_after_setup: Option<u64>,
    peak_rss_bytes: Option<u64>,
}

#[derive(Serialize)]
struct Report {
    benchmark: &'static str,
    schema_version: u8,
    profile: &'static str,
    dataset: Value,
    machine: MachineSpec,
    memory: MemoryUsage,
    disk: DiskUsage,
    latency_us: Value,
    output_dir: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let micro = args.iter().any(|arg| arg == "--micro");
    let records = if micro {
        MICRO_RECORDS
    } else {
        DEFAULT_RECORDS
    };
    let output_dir = arg_value(&args, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/private-deploy-bench"));
    fs::create_dir_all(&output_dir)?;

    let oxigraph_dir = output_dir.join("oxigraph");
    let redb_path = output_dir.join("redb").join("l0.redb");
    let hyperspace_dir = output_dir.join("hyperspace");
    fs::create_dir_all(redb_path.parent().expect("redb parent"))?;

    let sparql_latencies = bench_oxigraph(&oxigraph_dir, records)?;
    let redb_latencies = bench_redb(&redb_path, records)?;
    let hnsw_latencies = bench_hyperspace(&hyperspace_dir, records)?;

    let disk = DiskUsage {
        oxigraph_bytes: directory_size(&oxigraph_dir)?,
        redb_bytes: fs::metadata(&redb_path).map(|meta| meta.len()).unwrap_or(0),
        hyperspace_bytes: directory_size(&hyperspace_dir)?,
        total_bytes: directory_size(&output_dir)?,
    };
    let report = Report {
        benchmark: "wild-agentos-private-deploy",
        schema_version: 1,
        profile: if micro {
            "ci-micro"
        } else {
            "private-single-node"
        },
        dataset: json!({
            "id": "synthetic-deterministic-v1",
            "records": records,
            "vector_dimensions": DIMENSIONS,
            "sparql_query_iterations": QUERY_ITERATIONS,
            "hnsw_query_iterations": QUERY_ITERATIONS,
            "redb_read_iterations": QUERY_ITERATIONS,
            "generation": "record i uses deterministic IRI and f64 vector based on i and dimension",
        }),
        machine: machine_spec(),
        memory: MemoryUsage {
            rss_bytes_after_setup: proc_status_bytes("VmRSS"),
            peak_rss_bytes: proc_status_bytes("VmHWM"),
        },
        disk,
        latency_us: json!({
            "sparql_select": percentiles(sparql_latencies),
            "redb_get": percentiles(redb_latencies),
            "hnsw_search": percentiles(hnsw_latencies),
        }),
        output_dir: output_dir.display().to_string(),
    };

    fs::write(
        output_dir.join("report.json"),
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    fs::write(output_dir.join("report.md"), markdown_report(&report))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn bench_oxigraph(dir: &Path, records: usize) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let store = Store::open(dir)?;
    let graph = NamedNode::new("urn:wild-agentos:bench")?;
    let predicate = NamedNode::new("urn:wild-agentos:related")?;
    for i in 0..records {
        store.insert(&Quad::new(
            NamedNode::new(format!("urn:wild-agentos:record:{i}"))?,
            predicate.clone(),
            NamedNode::new(format!("urn:wild-agentos:record:{}", (i + 1) % records))?,
            graph.clone(),
        ))?;
    }

    let query = "SELECT ?object WHERE { GRAPH <urn:wild-agentos:bench> { \
        <urn:wild-agentos:record:42> <urn:wild-agentos:related> ?object } }";
    let mut samples = Vec::with_capacity(QUERY_ITERATIONS);
    for _ in 0..QUERY_ITERATIONS {
        let start = Instant::now();
        let QueryResults::Solutions(mut solutions) = store.query(query)? else {
            return Err("expected SELECT solutions".into());
        };
        if solutions.next().transpose()?.is_none() {
            return Err("SPARQL query returned no benchmark row".into());
        }
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(samples)
}

fn bench_redb(path: &Path, records: usize) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let db = Database::create(path)?;
    {
        let write = db.begin_write()?;
        {
            let mut table = write.open_table(L0_TABLE)?;
            for i in 0..records {
                let key = format!("record:{i:08}");
                let value = deterministic_value(i).into_bytes();
                table.insert(key.as_str(), value.as_slice())?;
            }
        }
        write.commit()?;
    }

    let read = db.begin_read()?;
    let table = read.open_table(L0_TABLE)?;
    let mut samples = Vec::with_capacity(QUERY_ITERATIONS);
    for _ in 0..QUERY_ITERATIONS {
        let start = Instant::now();
        let value = table.get("record:00000042")?;
        if value.is_none() {
            return Err("redb read returned no benchmark value".into());
        }
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Ok(samples)
}

fn bench_hyperspace(dir: &Path, records: usize) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let engine = HyperspaceEngineImpl::open(
            dir,
            WalSyncMode::Immediate,
            DIMENSIONS,
            Box::new(CosineMetric),
            HnswConfig::default(),
        )?;
        for i in 0..records {
            engine
                .insert(
                    &format!("urn:wild-agentos:vector:{i}"),
                    deterministic_vector(i),
                    json!({"@type": "urn:wild-agentos:BenchVector"}),
                )
                .await?;
        }
        engine.checkpoint().await?;

        let query = deterministic_vector(42);
        let mut samples = Vec::with_capacity(QUERY_ITERATIONS);
        for _ in 0..QUERY_ITERATIONS {
            let start = Instant::now();
            let results = engine.search(&query, 10, &[]).await?;
            if results.is_empty() {
                return Err("HNSW search returned no benchmark rows".into());
            }
            samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        }
        Ok(samples)
    })
}

fn deterministic_vector(record: usize) -> EmbeddingVector {
    let coords = (0..DIMENSIONS)
        .map(|dimension| ((record * 31 + dimension * 17) % 997) as f64 / 997.0)
        .collect();
    EmbeddingVector::new(coords, MetricKind::Cosine).expect("finite deterministic vector")
}

fn deterministic_value(record: usize) -> String {
    format!(
        "{{\"record\":{record},\"kind\":\"private-deploy-benchmark\",\"payload\":\"deterministic-v1\"}}"
    )
}

fn percentiles(mut samples: Vec<f64>) -> LatencyPercentiles {
    samples.sort_by(f64::total_cmp);
    let at = |percentile: f64| {
        let index = ((samples.len() as f64 * percentile).ceil() as usize).saturating_sub(1);
        samples[index]
    };
    LatencyPercentiles {
        unit: "microseconds",
        samples: samples.len(),
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        min: samples[0],
        max: samples[samples.len() - 1],
    }
}

fn directory_size(path: &Path) -> Result<u64, std::io::Error> {
    if !path.exists() {
        return Ok(0);
    }
    fs::read_dir(path)?.try_fold(0, |total, entry| {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            Ok(total + directory_size(&entry.path())?)
        } else {
            Ok(total + metadata.len())
        }
    })
}

fn machine_spec() -> MachineSpec {
    MachineSpec {
        os: format!("{} {}", env::consts::OS, env::consts::FAMILY),
        arch: env::consts::ARCH.to_string(),
        cpu_logical_cores: std::thread::available_parallelism().map_or(0, usize::from),
        cpu_model: fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|cpuinfo| {
                cpuinfo.lines().find_map(|line| {
                    line.strip_prefix("model name\t: ")
                        .or_else(|| line.strip_prefix("Hardware\t: "))
                        .map(str::to_owned)
                })
            }),
        memory_total_bytes: proc_status_bytes_from("/proc/meminfo", "MemTotal"),
    }
}

fn proc_status_bytes(field: &str) -> Option<u64> {
    proc_status_bytes_from("/proc/self/status", field)
}

fn proc_status_bytes_from(path: &str, field: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key == field).then(|| value.split_whitespace().next()?.parse::<u64>().ok()? * 1024)
    })
}

fn arg_value(args: &[String], name: &str) -> Option<&str> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then_some(pair[1].as_str()))
}

fn markdown_report(report: &Report) -> String {
    let metrics = &report.latency_us;
    let line = |name: &str| {
        let metric = &metrics[name];
        format!(
            "| {name} | {:.2} | {:.2} | {:.2} | {} |\n",
            metric["p50"].as_f64().unwrap_or_default(),
            metric["p95"].as_f64().unwrap_or_default(),
            metric["p99"].as_f64().unwrap_or_default(),
            metric["samples"].as_u64().unwrap_or_default()
        )
    };
    format!(
        "# Private deployment benchmark\n\n\
This is a measured run from this machine. It is not a comparison or a speedup claim.\n\n\
## Profile\n\n\
- Profile: `{}`\n- Dataset: `{}` ({} records, {} dimensions)\n\
- CPU: {} logical cores{}\n- Memory: {} bytes\n\n\
## Latency (microseconds)\n\n\
| Metric | p50 | p95 | p99 | Samples |\n|---|---:|---:|---:|---:|\n{}{}{}\
\n## Footprint\n\n\
| Store | Disk bytes |\n|---|---:|\n| Oxigraph | {} |\n| redb | {} |\n| Hyperspace | {} |\n| Total | {} |\n\n\
Memory RSS after setup: {} bytes; peak RSS: {} bytes.\n\n\
Machine data and raw metrics are in `report.json`.\n",
        report.profile,
        report.dataset["id"].as_str().unwrap_or("unknown"),
        report.dataset["records"],
        report.dataset["vector_dimensions"],
        report.machine.cpu_logical_cores,
        report.machine.cpu_model.as_ref().map(|model| format!(" ({model})")).unwrap_or_default(),
        report.machine.memory_total_bytes.unwrap_or(0),
        line("sparql_select"),
        line("redb_get"),
        line("hnsw_search"),
        report.disk.oxigraph_bytes,
        report.disk.redb_bytes,
        report.disk.hyperspace_bytes,
        report.disk.total_bytes,
        report.memory.rss_bytes_after_setup.unwrap_or(0),
        report.memory.peak_rss_bytes.unwrap_or(0),
    )
}
