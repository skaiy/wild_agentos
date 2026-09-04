//! Read-only, filesystem-level isolation diagnostics.
//!
//! This intentionally does not open a graph, vector, or blob backend: opening
//! one can create a WAL, acquire a writer lock, or otherwise mutate its
//! on-disk state.  Instead it inventories paths and scans existing regular
//! files for durable namespace markers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

const MAX_BYTES_PER_FILE: u64 = 32 * 1024 * 1024;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct IsolationDiagnosis {
    pub schema_version: u8,
    pub data_root: PathBuf,
    pub read_only: bool,
    pub minted_namespaces: Vec<MintedNamespace>,
    pub historical: HistoricalKeys,
    pub scan_warnings: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MintedNamespace {
    pub tenant: String,
    pub project: String,
    pub graph_iri: String,
    pub graph_artifact_count: usize,
    pub vector_namespace: String,
    pub vector_artifact_count: usize,
    pub blob_prefix: String,
    pub blob_object_count: usize,
    pub l0_path: String,
    pub l0_exists: bool,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct HistoricalKeys {
    pub graph_world_present: bool,
    pub tenant_vector_tags: Vec<String>,
    pub shared_l0_redb_present: bool,
    pub legacy_blob_prefixes: Vec<String>,
}

/// Inspects `data_root` without creating, opening, or modifying storage
/// backends. Namespace artifact counts are files containing a durable marker,
/// not backend object counts; blob object counts are regular files below the
/// minted `{tenant}/` local blob prefix.
pub fn diagnose_data_root(data_root: impl AsRef<Path>) -> io::Result<IsolationDiagnosis> {
    let data_root = data_root.as_ref().to_path_buf();
    let mut markers = MarkerIndex::default();
    let mut warnings = Vec::new();

    if !data_root.exists() {
        warnings.push("data root does not exist; no storage was opened or created".to_owned());
    } else {
        for entry in WalkDir::new(&data_root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!("could not inspect data-root entry: {error}"));
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&data_root)
                .unwrap_or(entry.path())
                .to_path_buf();
            scan_path_markers(&relative, &mut markers);
            let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if len > MAX_BYTES_PER_FILE {
                warnings.push(format!(
                    "skipped marker scan for {} (larger than {} MiB)",
                    relative.display(),
                    MAX_BYTES_PER_FILE / (1024 * 1024)
                ));
                continue;
            }
            match scan_file_markers(entry.path(), &relative, &mut markers) {
                Ok(()) => {}
                Err(error) => {
                    warnings.push(format!("could not scan {}: {error}", relative.display()))
                }
            }
        }
    }

    let mut namespaces = BTreeSet::new();
    namespaces.extend(markers.graphs.keys().cloned());
    namespaces.extend(markers.vectors.keys().cloned());

    let minted_namespaces = namespaces
        .into_iter()
        .map(|(tenant, project)| {
            let graph_iri = format!("graph://{tenant}/{project}");
            let vector_namespace = format!("vector://{tenant}/{project}");
            let blob_prefix = format!("{tenant}/");
            let l0_path = format!("l0/{tenant}");
            MintedNamespace {
                graph_artifact_count: markers
                    .graphs
                    .get(&(tenant.clone(), project.clone()))
                    .map_or(0, BTreeSet::len),
                vector_artifact_count: markers
                    .vectors
                    .get(&(tenant.clone(), project.clone()))
                    .map_or(0, BTreeSet::len),
                blob_object_count: count_regular_files(&data_root.join("blobs").join(&tenant)),
                l0_exists: data_root.join(&l0_path).is_dir(),
                tenant,
                project,
                graph_iri,
                vector_namespace,
                blob_prefix,
                l0_path,
            }
        })
        .collect();

    Ok(IsolationDiagnosis {
        schema_version: 1,
        data_root: data_root.clone(),
        read_only: true,
        minted_namespaces,
        historical: HistoricalKeys {
            graph_world_present: markers.graph_world,
            tenant_vector_tags: markers.tenant_vector_tags.into_iter().collect(),
            shared_l0_redb_present: data_root.join("l0_store/l0.redb").is_file(),
            legacy_blob_prefixes: markers.legacy_blob_prefixes.into_iter().collect(),
        },
        scan_warnings: warnings,
    })
}

/// Produces a copy/pasteable matrix for an incident or migration ticket.
pub fn render_markdown(diagnosis: &IsolationDiagnosis) -> String {
    let mut rows = vec![
        "| 路径 | 身份 | 期望 | 实测 |".to_owned(),
        "| --- | --- | --- | --- |".to_owned(),
    ];
    for namespace in &diagnosis.minted_namespaces {
        rows.push(format!(
            "| `{}` | 已验证 JWT `{}/{}` | 仅该 claims 图；不读取 `graph:world` | {} 个包含命名空间标记的存储文件 |",
            namespace.graph_iri, namespace.tenant, namespace.project, namespace.graph_artifact_count
        ));
        rows.push(format!(
            "| `{}` | 已验证 JWT `{}/{}` | 仅该 claims 向量命名空间；不读取 `tenant:<id>` | {} 个包含命名空间标记的存储文件 |",
            namespace.vector_namespace, namespace.tenant, namespace.project, namespace.vector_artifact_count
        ));
        rows.push(format!(
            "| `blobs/{}` | 已验证 JWT `{}` | 仅该 tenant blob 前缀 | {} 个本地对象 |",
            namespace.blob_prefix, namespace.tenant, namespace.blob_object_count
        ));
        rows.push(format!(
            "| `{}/l0.redb` | 已验证 JWT `{}` | tenant L0；历史共享库不可写 | {} |",
            namespace.l0_path,
            namespace.tenant,
            if namespace.l0_exists {
                "目录存在"
            } else {
                "未发现"
            }
        ));
    }
    rows.push(format!(
        "| `graph:world` | 无 claims（历史） | 保留，不由 claims 路径读取 | {} |",
        present(diagnosis.historical.graph_world_present)
    ));
    rows.push(format!(
        "| `tenant:<id>` vectors | 无 claims（历史） | 保留，不由 claims 路径读取 | {} |",
        if diagnosis.historical.tenant_vector_tags.is_empty() {
            "未发现".to_owned()
        } else {
            diagnosis.historical.tenant_vector_tags.join(", ")
        }
    ));
    rows.push(format!(
        "| `l0_store/l0.redb` | 无 claims（历史） | 保留且只读；不迁移 | {} |",
        present(diagnosis.historical.shared_l0_redb_present)
    ));
    rows.push(format!(
        "| `tenant:<id>/kb/...` | 无 claims（历史） | 保留，不映射到新 blob 前缀 | {} |",
        if diagnosis.historical.legacy_blob_prefixes.is_empty() {
            "未发现".to_owned()
        } else {
            diagnosis.historical.legacy_blob_prefixes.join(", ")
        }
    ));
    rows.join("\n")
}

fn present(value: bool) -> &'static str {
    if value {
        "发现"
    } else {
        "未发现"
    }
}

#[derive(Default)]
struct MarkerIndex {
    graphs: BTreeMap<(String, String), BTreeSet<PathBuf>>,
    vectors: BTreeMap<(String, String), BTreeSet<PathBuf>>,
    graph_world: bool,
    tenant_vector_tags: BTreeSet<String>,
    legacy_blob_prefixes: BTreeSet<String>,
}

fn scan_path_markers(path: &Path, markers: &mut MarkerIndex) {
    let text = path.to_string_lossy();
    let components: Vec<_> = path.components().collect();
    for pair in components.windows(2) {
        let tenant = pair[0]
            .as_os_str()
            .to_string_lossy()
            .strip_prefix("tenant:")
            .map(str::to_owned);
        if tenant.as_deref().is_some_and(safe_segment) && pair[1].as_os_str() == "kb" {
            markers
                .legacy_blob_prefixes
                .insert(format!("tenant:{}/kb/...", tenant.unwrap()));
        }
    }
    scan_text(&text, path, markers);
}

fn scan_file_markers(path: &Path, relative: &Path, markers: &mut MarkerIndex) -> io::Result<()> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    scan_text(&text, relative, markers);
    Ok(())
}

fn scan_text(text: &str, relative: &Path, markers: &mut MarkerIndex) {
    markers.graph_world |= text.contains("graph:world");
    for (tenant, project) in namespace_matches(text, "graph://") {
        markers
            .graphs
            .entry((tenant, project))
            .or_default()
            .insert(relative.to_path_buf());
    }
    for (tenant, project) in namespace_matches(text, "vector://") {
        markers
            .vectors
            .entry((tenant, project))
            .or_default()
            .insert(relative.to_path_buf());
    }
    if relative.starts_with("vector_store") {
        for tenant in prefixed_segments(text, "tenant:") {
            markers
                .tenant_vector_tags
                .insert(format!("tenant:{tenant}"));
        }
    }
    for tenant in prefixed_segments(text, "tenant:") {
        if text.contains(&format!("tenant:{tenant}/kb/")) {
            markers
                .legacy_blob_prefixes
                .insert(format!("tenant:{tenant}/kb/..."));
        }
    }
}

fn namespace_matches(text: &str, prefix: &str) -> Vec<(String, String)> {
    text.match_indices(prefix)
        .filter_map(|(start, _)| {
            let suffix = &text[start + prefix.len()..];
            let mut parts = suffix.split('/');
            let tenant = parts
                .next()?
                .chars()
                .take_while(|c| safe_char(*c))
                .collect::<String>();
            let project = parts
                .next()?
                .chars()
                .take_while(|c| safe_char(*c))
                .collect::<String>();
            (safe_segment(&tenant) && safe_segment(&project)).then_some((tenant, project))
        })
        .collect()
}

fn prefixed_segments(text: &str, prefix: &str) -> BTreeSet<String> {
    text.match_indices(prefix)
        .filter_map(|(start, _)| {
            let segment = text[start + prefix.len()..]
                .chars()
                .take_while(|c| safe_char(*c))
                .collect::<String>();
            safe_segment(&segment).then_some(segment)
        })
        .collect()
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty() && value.chars().all(safe_char)
}

fn safe_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '-' | '_')
}

fn count_regular_files(path: &Path) -> usize {
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnoses_minted_and_historical_fixture_without_writing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("blobs/acme/kb/1")).unwrap();
        std::fs::write(root.path().join("blobs/acme/kb/1/document"), b"source").unwrap();
        std::fs::create_dir_all(root.path().join("l0/acme")).unwrap();
        std::fs::write(root.path().join("l0/acme/l0.redb"), b"fixture").unwrap();
        std::fs::create_dir_all(root.path().join("l0_store")).unwrap();
        std::fs::write(root.path().join("l0_store/l0.redb"), b"historical").unwrap();
        std::fs::create_dir_all(root.path().join("blobs/tenant:default/kb/old")).unwrap();
        std::fs::write(
            root.path().join("kg.fixture"),
            b"graph://acme/research graph:world tenant:default/kb/old",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("vector_store")).unwrap();
        std::fs::write(
            root.path().join("vector_store/fixture"),
            b"vector://acme/research tenant:legacy",
        )
        .unwrap();

        let result = diagnose_data_root(root.path()).unwrap();

        assert!(result.read_only);
        assert_eq!(result.schema_version, 1);
        assert_eq!(result.minted_namespaces.len(), 1);
        let acme = &result.minted_namespaces[0];
        assert_eq!(acme.tenant, "acme");
        assert_eq!(acme.project, "research");
        assert_eq!(acme.blob_object_count, 1);
        assert!(acme.l0_exists);
        assert_eq!(acme.graph_artifact_count, 1);
        assert_eq!(acme.vector_artifact_count, 1);
        assert!(result.historical.graph_world_present);
        assert!(result.historical.shared_l0_redb_present);
        assert_eq!(
            result.historical.tenant_vector_tags,
            vec!["tenant:legacy".to_owned()]
        );
        assert_eq!(
            result.historical.legacy_blob_prefixes,
            vec!["tenant:default/kb/...".to_owned()]
        );
        assert!(render_markdown(&result).contains("| 路径 | 身份 | 期望 | 实测 |"));
    }
}
