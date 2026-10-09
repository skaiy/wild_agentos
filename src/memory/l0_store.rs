//! L0 Store - Long-term memory persistence storage
//!
//! This module handles persistent storage of memories and knowledge, supporting MESI cache coherence states and tag secondary indexes.

use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info};

use crate::isolation::IsolationClaims;
use crate::jsonld::registry::{EntityLocation, IriRegistry, StorageLayer};
use crate::CoreError;

/// MESI cache coherence state enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MesiState {
    Modified,
    Exclusive,
    #[default]
    Shared,
    Invalid,
}

/// L0 memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L0Entry {
    pub iri: String,
    pub content: String,
    pub importance: f32,
    pub access_count: u32,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub tags: Vec<String>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub mesi_state: MesiState,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub named_graph: Option<String>,
    #[serde(default)]
    pub jsonld_context: Option<String>,
    #[serde(default)]
    pub jsonld_types: Vec<String>,
    #[serde(default)]
    pub hyperspace_point_id: Option<u32>,
}

/// L0 search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L0SearchResult {
    pub iri: String,
    pub content: String,
    pub relevance_score: f32,
    pub importance: f32,
    pub tags: Vec<String>,
}

/// L0 Store configuration
#[derive(Debug, Clone)]
pub struct L0Config {
    pub path: String,
    pub max_entries: usize,
    pub compression: bool,
}

impl Default for L0Config {
    fn default() -> Self {
        Self {
            path: "./data/l0_store".to_string(),
            max_entries: 1_000_000,
            compression: true,
        }
    }
}

/// Compute SHA-256 content hash for content-addressed deduplication.
/// Uses SHA-256 (same as workspace_monitor) — provides collision resistance
/// and cross-process reproducibility that DefaultHasher cannot guarantee.
fn compute_content_hash(content: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    format!("sha256:{}", hex::encode(result))
}

/// Table definitions for L0 Store redb database.
const ENTRIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("entries");
const TAG_INDEX_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tag_index");
const NAMED_GRAPH_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("named_graph");

/// Per-database redb page cache. redb's default is up to 1 GiB per file;
/// tenant handles share a process, so each database is capped.
pub const L0_REDB_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// How many tenant L0 databases one process will keep open.
pub const DEFAULT_MAX_TENANT_L0_HANDLES: usize = 32;

/// Process-wide registry of writable tenant L0 handles.
///
/// redb takes an exclusive lock on its file, so a second `Database::create`
/// on the same `l0.redb` in this process fails with "Database already open".
/// Every run of one tenant therefore has to share a single open handle; this
/// registry hands out that shared handle instead of opening the file per run.
///
/// - Paths are minted only from verified claims ([`tenant_path`]), so the
///   registry cannot be used to reach another tenant's directory. Projects of
///   one tenant share the tenant's L0. Isolation granularity is the tenant.
/// - The registry map lock only inserts or looks up a slot. Directory creation
///   and `Database::create` run after that lock is released, and only the
///   slot for that path waits. One tenant's open cannot block another tenant.
/// - [`Self::release_idle`] closes handles no run is using any more.
/// - The lock is per process: two Core processes still cannot share one L0
///   root.
pub struct TenantL0Registry {
    l0_root: PathBuf,
    max_handles: usize,
    handles: std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<TenantSlot>>>,
    #[cfg(test)]
    before_open: std::sync::Mutex<Option<Arc<dyn Fn(&Path) + Send + Sync>>>,
}

struct TenantSlot {
    store: std::sync::OnceLock<Arc<L0Store>>,
    open: std::sync::Mutex<()>,
}

/// Drops idle tenant handles when the run that opened them returns, including
/// panic and early-return paths. Declare it before the handle clone so the
/// clone is dropped first.
pub struct TenantL0Lease {
    registry: Arc<TenantL0Registry>,
}

impl Drop for TenantL0Lease {
    fn drop(&mut self) {
        self.registry.release_idle();
    }
}

impl TenantL0Registry {
    pub fn new(l0_root: impl Into<PathBuf>) -> Self {
        Self::with_handle_limit(l0_root, DEFAULT_MAX_TENANT_L0_HANDLES)
    }

    pub fn with_handle_limit(l0_root: impl Into<PathBuf>, max_handles: usize) -> Self {
        Self {
            l0_root: l0_root.into(),
            max_handles: max_handles.max(1),
            handles: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(test)]
            before_open: std::sync::Mutex::new(None),
        }
    }

    pub fn l0_root(&self) -> &Path {
        &self.l0_root
    }

    /// The largest number of tenant databases this registry will open.
    pub fn handle_limit(&self) -> usize {
        self.max_handles
    }

    /// A guard that closes idle handles when dropped.
    pub fn lease(self: &Arc<Self>) -> TenantL0Lease {
        TenantL0Lease {
            registry: Arc::clone(self),
        }
    }

    /// Install a hook that runs after the map lock is released and before the
    /// database file is created. Tests use it to prove one tenant's open does
    /// not block another.
    #[cfg(test)]
    pub fn set_before_open_for_test(&self, hook: impl Fn(&Path) + Send + Sync + 'static) {
        *self
            .before_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(hook));
    }

    /// The shared writable L0 handle for the tenant in `claims`, opened on
    /// first use.
    pub fn get_or_open(&self, claims: &IsolationClaims) -> Result<Arc<L0Store>, CoreError> {
        let path = tenant_path(&self.l0_root, claims)?;
        let slot = self.prepare_slot(&path)?;
        self.open_slot(&path, &slot, claims)
    }

    fn prepare_slot(&self, path: &Path) -> Result<Arc<TenantSlot>, CoreError> {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = handles.get(path) {
            return Ok(slot.clone());
        }
        evict_idle_slots(&mut handles);
        if handles.len() >= self.max_handles {
            return Err(CoreError::StorageError {
                message: format!("tenant L0 handle limit reached ({})", self.max_handles),
            });
        }
        let slot = Arc::new(TenantSlot {
            store: std::sync::OnceLock::new(),
            open: std::sync::Mutex::new(()),
        });
        handles.insert(path.to_path_buf(), slot.clone());
        Ok(slot)
    }

    fn open_slot(
        &self,
        path: &Path,
        slot: &Arc<TenantSlot>,
        claims: &IsolationClaims,
    ) -> Result<Arc<L0Store>, CoreError> {
        if let Some(handle) = slot.store.get() {
            return Ok(handle.clone());
        }
        let _open = slot
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = slot.store.get() {
            return Ok(handle.clone());
        }
        // Copy the hook out before calling it. The lock guard must not stay
        // alive across the hook: the hook blocks, and another tenant's open
        // also reads this slot.
        #[cfg(test)]
        let before_open = self
            .before_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(test)]
        if let Some(hook) = before_open {
            hook(path);
        }
        match open_database_off_worker(&self.l0_root, claims) {
            Ok(store) => {
                let handle = Arc::new(store);
                let _ = slot.store.set(handle.clone());
                Ok(handle)
            }
            Err(error) => {
                drop(_open);
                let mut handles = self
                    .handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if handles
                    .get(path)
                    .is_some_and(|current| Arc::ptr_eq(current, slot) && slot.store.get().is_none())
                {
                    handles.remove(path);
                }
                Err(error)
            }
        }
    }

    /// Close every handle that only the registry still holds. Returns how many
    /// were closed.
    pub fn release_idle(&self) -> usize {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = handles.len();
        evict_idle_slots(&mut handles);
        before - handles.len()
    }

    /// Number of tenant slots currently tracked (open or opening).
    pub fn open_handles(&self) -> usize {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

fn evict_idle_slots(handles: &mut std::collections::HashMap<PathBuf, Arc<TenantSlot>>) {
    handles.retain(|_, slot| match slot.store.get() {
        // `strong_count == 1` means only this slot still holds the database.
        Some(handle) => Arc::strong_count(handle) > 1,
        // An empty slot is an open still in progress. Keep it.
        None => Arc::strong_count(slot) > 1,
    });
}

fn open_database_off_worker(
    l0_root: &Path,
    claims: &IsolationClaims,
) -> Result<L0Store, CoreError> {
    let root = l0_root.to_path_buf();
    let claims = claims.clone();
    let open = move || L0Store::open_for_claims(&root, &claims);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
            return tokio::task::block_in_place(|| {
                handle.block_on(async move {
                    match tokio::task::spawn_blocking(open).await {
                        Ok(opened) => opened,
                        Err(_) => Err(CoreError::StorageError {
                            message: "L0 open task failed".to_string(),
                        }),
                    }
                })
            });
        }
    }
    open()
}

/// Count a rejected L0 write and log it. The count is the audit value; the
/// log line does not include a filesystem path.
pub(crate) fn note_l0_write_rejected(kind: &str) -> u64 {
    static REJECTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::warn!(kind, count, "L0 write rejected");
    count
}

fn permission_denied(action: &str) -> CoreError {
    CoreError::PermissionDenied {
        agent: "l0_store".to_string(),
        resource: "l0".to_string(),
        action: action.to_string(),
    }
}

fn tenant_path(l0_root: &Path, claims: &IsolationClaims) -> Result<PathBuf, CoreError> {
    let minted_path = claims
        .l0_path()
        .map_err(|_| permission_denied("open tenant L0 with invalid verified claims"))?;
    let tenant = minted_path
        .file_name()
        .ok_or_else(|| permission_denied("open tenant L0 without a tenant segment"))?;
    let tenant = tenant.to_string_lossy();
    // Case-insensitive filesystems would otherwise map `Acme` and `acme` onto
    // one directory. Tenant ids used for L0 must already be lowercase.
    if tenant.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(permission_denied(
            "open tenant L0: tenant id must be lowercase",
        ));
    }
    Ok(l0_root.join(tenant.as_ref()))
}

/// `iri://tool-result/{call_id}` (and any other form that is not the
/// five-segment owner key) is a pre-owner legacy entry. Reads refuse it.
pub(crate) fn is_legacy_unscoped_tool_result_iri(iri: &str) -> bool {
    const PREFIX: &str = "iri://tool-result/";
    let Some(rest) = iri.strip_prefix(PREFIX) else {
        return false;
    };
    let segments = rest
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count();
    segments != 5
}

/// L0 Store
pub struct L0Store {
    db: Database,
    writable: bool,
    #[allow(dead_code)]
    config: L0Config,
    #[allow(dead_code)]
    entry_count: u64,
    /// Optional IRI registry reference (auto-registers @id after injection)
    iri_registry: Option<Arc<IriRegistry>>,
}

impl L0Store {
    /// Creates a writable isolated fixture store for in-crate tests.
    ///
    /// Production callers cannot use this constructor: production L0 writes
    /// must use [`Self::open_for_claims`] with verified claims.
    #[cfg(test)]
    pub fn new(path: &str) -> Result<Self, CoreError> {
        Self::open_writable(Path::new(path))
    }

    /// Opens the legacy shared L0 database in read-only compatibility mode.
    ///
    /// New writes require [`Self::open_for_claims`], which mints a tenant
    /// directory. This constructor deliberately does not create the legacy
    /// path, preserving existing history without treating it as tenant data.
    #[cfg(not(test))]
    pub fn new(path: &str) -> Result<Self, CoreError> {
        Self::open_legacy_readonly(path)
    }

    /// Opens an existing historical shared L0 database without permitting
    /// writes. This explicit API is also used to verify the fail-closed
    /// compatibility path in tests.
    ///
    /// - `l0.redb` absent (fresh install): starts with an empty, in-memory,
    ///   read-only legacy view. Nothing is created on disk at the legacy path.
    /// - `l0.redb` present: opened exactly as before; writes still fail closed.
    /// - `l0.redb` present but unreadable (zero bytes, corrupt): returns an
    ///   error that says what to do, and never overwrites the file.
    pub fn open_legacy_readonly(path: &str) -> Result<Self, CoreError> {
        let db_path = Path::new(path).join("l0.redb");
        // `symlink_metadata` so that a dangling symlink is reported as an
        // unreadable file rather than silently treated as "no history".
        match std::fs::symlink_metadata(&db_path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    "No legacy shared L0 database at {}; starting with an empty read-only legacy view (tenant writes use per-tenant directories)",
                    db_path.display()
                );
                return Self::empty_legacy_readonly(path);
            }
            Err(e) => {
                return Err(CoreError::StorageError {
                    message: format!(
                        "Failed to open legacy read-only database at {}: cannot inspect the file: {}",
                        db_path.display(),
                        e
                    ),
                });
            }
        }

        let db = Database::open(&db_path).map_err(|e| CoreError::StorageError {
            message: format!(
                "Failed to open legacy read-only database at {}: {}. The file exists but is not a \
                 readable L0 database (for example it is empty or corrupt); it was left unmodified. \
                 Restore it from a backup, or move it out of the L0 directory to start without \
                 legacy history.",
                db_path.display(),
                e
            ),
        })?;

        Self::from_database(db, path.to_owned(), false)
    }

    /// Empty legacy view for a fresh install: an in-memory redb database with
    /// the L0 tables created, wrapped read-only so writes still fail closed.
    fn empty_legacy_readonly(path: &str) -> Result<Self, CoreError> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to create empty legacy L0 view: {}", e),
            })?;
        Self::create_tables(&db)?;
        Self::from_database(db, path.to_owned(), false)
    }

    /// Opens a writable L0 database scoped to verified tenant claims.
    ///
    /// Crate-private. Production callers open through [`TenantL0Registry`] so
    /// one process does not `Database::create` the same file twice.
    ///
    /// `l0_root` is the local representation of the `/data/l0` root from the
    /// isolation contract. Only the minted tenant path is created, on demand.
    pub(crate) fn open_for_claims(
        l0_root: impl AsRef<Path>,
        claims: &IsolationClaims,
    ) -> Result<Self, CoreError> {
        let tenant_path = tenant_path(l0_root.as_ref(), claims)?;
        info!(
            "Initializing tenant-scoped L0 Store: {}",
            tenant_path.display()
        );
        Self::open_writable(&tenant_path)
    }

    fn open_writable(path: &Path) -> Result<Self, CoreError> {
        std::fs::create_dir_all(path).map_err(|e| CoreError::StorageError {
            message: format!("Failed to create storage directory: {}", e),
        })?;

        let db_path = path.join("l0.redb");
        let db = Database::builder()
            .set_cache_size(L0_REDB_CACHE_BYTES)
            .create(&db_path)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open database: {}", e),
            })?;

        Self::from_database(db, path.to_string_lossy().into_owned(), true)
    }

    fn from_database(db: Database, path: String, writable: bool) -> Result<Self, CoreError> {
        // Ensure tables exist by opening them in a write transaction
        if writable {
            Self::create_tables(&db)?;
        }

        let entry_count = {
            let read_txn = db.begin_read().map_err(|e| CoreError::StorageError {
                message: format!("Failed to begin read transaction: {}", e),
            })?;
            let table =
                read_txn
                    .open_table(ENTRIES_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open table: {}", e),
                    })?;
            table.len().map_err(|e| CoreError::StorageError {
                message: format!("Failed to get entry count: {}", e),
            })?
        };

        let store = Self {
            db,
            writable,
            config: L0Config {
                path,
                ..Default::default()
            },
            entry_count,
            iri_registry: None,
        };
        if writable {
            let purged = store.purge_legacy_unscoped_tool_results()?;
            if purged > 0 {
                info!(purged, "purged legacy unscoped tool-result entries from L0");
            }
        }
        Ok(store)
    }

    fn create_tables(db: &Database) -> Result<(), CoreError> {
        let write_txn = db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Failed to begin write transaction: {}", e),
        })?;
        write_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open entries table: {}", e),
            })?;
        write_txn
            .open_table(TAG_INDEX_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open tag index table: {}", e),
            })?;
        write_txn
            .open_table(NAMED_GRAPH_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open named graph table: {}", e),
            })?;
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })
    }

    fn assert_writable(&self) -> Result<(), CoreError> {
        if self.writable {
            return Ok(());
        }
        Err(permission_denied("write L0 data without verified claims"))
    }

    /// Update tag index: remove old tag indexes, then insert new tag indexes
    fn update_tag_index(
        &self,
        iri: &str,
        old_tags: &[String],
        new_tags: &[String],
    ) -> Result<(), CoreError> {
        for tag in old_tags {
            let index_key = format!("tag:{}", tag);
            self.remove_iri_from_tag_index(&index_key, iri)?;
        }
        for tag in new_tags {
            let index_key = format!("tag:{}", tag);
            self.add_iri_to_tag_index(&index_key, iri)?;
        }
        Ok(())
    }

    /// Add IRI to tag index
    fn add_iri_to_tag_index(&self, index_key: &str, iri: &str) -> Result<(), CoreError> {
        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        {
            let mut table =
                write_txn
                    .open_table(TAG_INDEX_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open tag index: {}", e),
                    })?;
            let mut iris: Vec<String> = match table.get(index_key) {
                Ok(Some(guard)) => serde_json::from_slice(guard.value()).unwrap_or_default(),
                _ => Vec::new(),
            };
            if !iris.contains(&iri.to_string()) {
                iris.push(iri.to_string());
            }
            let encoded = serde_json::to_vec(&iris).map_err(|e| CoreError::StorageError {
                message: format!("Failed to serialize tag index: {}", e),
            })?;
            table
                .insert(index_key, encoded.as_slice())
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to write tag index: {}", e),
                })?;
        }
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;
        Ok(())
    }

    /// Remove IRI from tag index
    fn remove_iri_from_tag_index(&self, index_key: &str, iri: &str) -> Result<(), CoreError> {
        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        let iris_empty = {
            let mut table =
                write_txn
                    .open_table(TAG_INDEX_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open tag index: {}", e),
                    })?;
            let mut iris: Vec<String> = match table.get(index_key) {
                Ok(Some(guard)) => serde_json::from_slice(guard.value()).unwrap_or_default(),
                _ => return Ok(()),
            };
            iris.retain(|i| i != iri);
            if iris.is_empty() {
                table
                    .remove(index_key)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to delete tag index: {}", e),
                    })?;
                true
            } else {
                let encoded = serde_json::to_vec(&iris).map_err(|e| CoreError::StorageError {
                    message: format!("Failed to serialize tag index: {}", e),
                })?;
                table.insert(index_key, encoded.as_slice()).map_err(|e| {
                    CoreError::StorageError {
                        message: format!("Failed to write tag index: {}", e),
                    }
                })?;
                false
            }
        };
        let _ = iris_empty;
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;
        Ok(())
    }

    pub fn store(&self, iri: &str, content: &str) -> Result<(), CoreError> {
        self.assert_writable()?;
        let content_hash = compute_content_hash(content);

        let existing_entry = self.retrieve_without_update(iri)?;

        let entry = if let Some(existing) = existing_entry {
            let new_entry = L0Entry {
                iri: iri.to_string(),
                content: content.to_string(),
                importance: 0.5,
                access_count: 0,
                created_at: Utc::now(),
                last_accessed: Utc::now(),
                tags: Vec::new(),
                metadata: serde_json::Map::new(),
                mesi_state: MesiState::Shared,
                content_hash,
                named_graph: None,

                jsonld_context: None,
                jsonld_types: Vec::new(),
                hyperspace_point_id: None,
            };
            Self::merge_entries(&existing, &new_entry)
        } else {
            L0Entry {
                iri: iri.to_string(),
                content: content.to_string(),
                importance: 0.5,
                access_count: 0,
                created_at: Utc::now(),
                last_accessed: Utc::now(),
                tags: Vec::new(),
                metadata: serde_json::Map::new(),
                mesi_state: MesiState::Shared,
                content_hash,
                named_graph: None,

                jsonld_context: None,
                jsonld_types: Vec::new(),
                hyperspace_point_id: None,
            }
        };

        let value = serde_json::to_vec(&entry).map_err(|e| CoreError::StorageError {
            message: format!("Failed to serialize entry: {}", e),
        })?;

        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        {
            let mut table =
                write_txn
                    .open_table(ENTRIES_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open table: {}", e),
                    })?;
            table
                .insert(iri, value.as_slice())
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to store entry: {}", e),
                })?;
        }
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;

        debug!(iri = %entry.iri, "Entry stored to L0");
        Ok(())
    }

    fn retrieve_without_update(&self, iri: &str) -> Result<Option<L0Entry>, CoreError> {
        if is_legacy_unscoped_tool_result_iri(iri) {
            return Ok(None);
        }
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        match table.get(iri).map_err(|e| CoreError::StorageError {
            message: format!("Failed to retrieve entry: {}", e),
        })? {
            Some(guard) => {
                let entry: L0Entry =
                    serde_json::from_slice(guard.value()).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to deserialize entry: {}", e),
                    })?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    fn merge_entries(existing: &L0Entry, new: &L0Entry) -> L0Entry {
        let mut merged_metadata = existing.metadata.clone();
        for (key, value) in &new.metadata {
            merged_metadata.insert(key.clone(), value.clone());
        }

        let mut merged_tags = existing.tags.clone();
        for tag in &new.tags {
            if !merged_tags.contains(tag) {
                merged_tags.push(tag.clone());
            }
        }

        let mut merged_types = existing.jsonld_types.clone();
        for type_iri in &new.jsonld_types {
            if !merged_types.contains(type_iri) {
                merged_types.push(type_iri.clone());
            }
        }

        L0Entry {
            iri: existing.iri.clone(),
            content: new.content.clone(),
            importance: (existing.importance + new.importance) / 2.0,
            access_count: existing.access_count,
            created_at: existing.created_at,
            last_accessed: Utc::now(),
            tags: merged_tags,
            metadata: merged_metadata,
            mesi_state: new.mesi_state,
            content_hash: new.content_hash.clone(),
            named_graph: existing.named_graph.clone().or(new.named_graph.clone()),

            jsonld_context: new
                .jsonld_context
                .clone()
                .or(existing.jsonld_context.clone()),
            jsonld_types: merged_types,
            hyperspace_point_id: existing.hyperspace_point_id.or(new.hyperspace_point_id),
        }
    }

    pub fn store_entry(&self, entry: &L0Entry) -> Result<(), CoreError> {
        self.assert_writable()?;
        let old_tags = self.get_entry_tags(&entry.iri)?;
        let old_named_graph = self.get_entry_named_graph(&entry.iri)?;

        let content_hash = if entry.content_hash.is_empty() {
            compute_content_hash(&entry.content)
        } else {
            entry.content_hash.clone()
        };

        let existing_entry = self.retrieve_without_update(&entry.iri)?;
        let entry = if let Some(existing) = existing_entry {
            let entry_with_hash = L0Entry {
                content_hash,
                ..entry.clone()
            };
            Self::merge_entries(&existing, &entry_with_hash)
        } else {
            L0Entry {
                content_hash,
                ..entry.clone()
            }
        };

        self.update_tag_index(&entry.iri, &old_tags, &entry.tags)?;

        if old_named_graph != entry.named_graph {
            if let Some(ref old_graph) = old_named_graph {
                self.remove_iri_from_named_graph_index(old_graph, &entry.iri)?;
            }
            if let Some(ref new_graph) = entry.named_graph {
                self.add_iri_to_named_graph_index(new_graph, &entry.iri)?;
            }
        }

        let value = serde_json::to_vec(&entry).map_err(|e| CoreError::StorageError {
            message: format!("Failed to serialize entry: {}", e),
        })?;

        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        {
            let mut table =
                write_txn
                    .open_table(ENTRIES_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open table: {}", e),
                    })?;
            table
                .insert(entry.iri.as_str(), value.as_slice())
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to store entry: {}", e),
                })?;
        }
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;

        debug!(iri = %entry.iri, "Entry stored to L0");
        Ok(())
    }

    fn get_entry_named_graph(&self, iri: &str) -> Result<Option<String>, CoreError> {
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        match table.get(iri).map_err(|e| CoreError::StorageError {
            message: format!("Failed to retrieve entry: {}", e),
        })? {
            Some(guard) => {
                let entry: L0Entry =
                    serde_json::from_slice(guard.value()).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to deserialize entry: {}", e),
                    })?;
                Ok(entry.named_graph)
            }
            _ => Ok(None),
        }
    }

    fn add_iri_to_named_graph_index(&self, graph: &str, iri: &str) -> Result<(), CoreError> {
        let key = format!("graph:{}", graph);
        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        {
            let mut table =
                write_txn
                    .open_table(NAMED_GRAPH_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open named graph index: {}", e),
                    })?;
            let mut iris: Vec<String> = match table.get(key.as_str()) {
                Ok(Some(guard)) => serde_json::from_slice(guard.value()).unwrap_or_default(),
                _ => Vec::new(),
            };
            if !iris.contains(&iri.to_string()) {
                iris.push(iri.to_string());
            }
            let encoded = serde_json::to_vec(&iris).map_err(|e| CoreError::StorageError {
                message: format!("Failed to serialize named graph index: {}", e),
            })?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to write named graph index: {}", e),
                })?;
        }
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;
        Ok(())
    }

    fn remove_iri_from_named_graph_index(&self, graph: &str, iri: &str) -> Result<(), CoreError> {
        let key = format!("graph:{}", graph);
        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        {
            let mut table =
                write_txn
                    .open_table(NAMED_GRAPH_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open named graph index: {}", e),
                    })?;
            let iris: Vec<String> = match table.get(key.as_str()) {
                Ok(Some(guard)) => serde_json::from_slice(guard.value()).unwrap_or_default(),
                _ => return Ok(()),
            };
            if iris.is_empty() {
                return Ok(());
            }
            // rebuild without the target iri
            let filtered: Vec<String> = iris.into_iter().filter(|i| i != iri).collect();
            if filtered.is_empty() {
                table
                    .remove(key.as_str())
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to delete named graph index: {}", e),
                    })?;
            } else {
                let encoded =
                    serde_json::to_vec(&filtered).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to serialize named graph index: {}", e),
                    })?;
                table
                    .insert(key.as_str(), encoded.as_slice())
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to write named graph index: {}", e),
                    })?;
            }
        }
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;
        Ok(())
    }

    /// Get existing tags of an entry (for index updates)
    fn get_entry_tags(&self, iri: &str) -> Result<Vec<String>, CoreError> {
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        match table.get(iri).map_err(|e| CoreError::StorageError {
            message: format!("Failed to retrieve entry: {}", e),
        })? {
            Some(guard) => {
                let entry: L0Entry =
                    serde_json::from_slice(guard.value()).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to deserialize entry: {}", e),
                    })?;
                Ok(entry.tags)
            }
            _ => Ok(Vec::new()),
        }
    }

    pub fn retrieve(&self, iri: &str) -> Result<Option<L0Entry>, CoreError> {
        if is_legacy_unscoped_tool_result_iri(iri) {
            return Ok(None);
        }
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        let value = table.get(iri).map_err(|e| CoreError::StorageError {
            message: format!("Failed to retrieve entry: {}", e),
        })?;

        match value {
            Some(guard) => {
                let mut entry: L0Entry =
                    serde_json::from_slice(guard.value()).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to deserialize entry: {}", e),
                    })?;
                drop(read_txn);

                if !self.writable {
                    return Ok(Some(entry));
                }
                entry.access_count += 1;
                entry.last_accessed = Utc::now();
                self.store_entry(&entry)?;

                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    pub fn delete(&self, iri: &str) -> Result<bool, CoreError> {
        self.assert_writable()?;
        let old_tags = self.get_entry_tags(iri)?;

        for tag in &old_tags {
            let index_key = format!("tag:{}", tag);
            self.remove_iri_from_tag_index(&index_key, iri)?;
        }

        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        let removed = {
            let mut table =
                write_txn
                    .open_table(ENTRIES_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open table: {}", e),
                    })?;
            let has_removed = match table.remove(iri) {
                Ok(_) => true,
                Err(e) => {
                    return Err(CoreError::StorageError {
                        message: format!("Failed to delete entry: {}", e),
                    });
                }
            };
            has_removed
        };
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;
        Ok(removed)
    }

    /// Delete unscoped `iri://tool-result/{call_id}` entries. Owner cannot be
    /// recovered, so they are removed rather than reassigned. Returns the
    /// count only.
    fn purge_legacy_unscoped_tool_results(&self) -> Result<usize, CoreError> {
        let iris = {
            let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
                message: format!("Read transaction failed: {}", e),
            })?;
            let table =
                read_txn
                    .open_table(ENTRIES_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open table: {}", e),
                    })?;
            let mut iris = Vec::new();
            for result in
                table
                    .range("iri://tool-result/"..)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Prefix scan failed: {}", e),
                    })?
            {
                let (key_guard, _) = result.map_err(|e| CoreError::StorageError {
                    message: format!("Iteration failed: {}", e),
                })?;
                let key = key_guard.value();
                if !key.starts_with("iri://tool-result/") {
                    break;
                }
                if is_legacy_unscoped_tool_result_iri(key) {
                    iris.push(key.to_string());
                }
            }
            iris
        };
        let count = iris.len();
        for iri in iris {
            self.delete(&iri)?;
        }
        Ok(count)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<L0SearchResult>, CoreError> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();

        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        for result in table.iter().map_err(|e| CoreError::StorageError {
            message: format!("Iteration failed: {}", e),
        })? {
            let (_, value) = result.map_err(|e| CoreError::StorageError {
                message: format!("Iteration failed: {}", e),
            })?;

            let entry: L0Entry =
                serde_json::from_slice(value.value()).map_err(|e| CoreError::StorageError {
                    message: format!("Failed to deserialize entry: {}", e),
                })?;

            let content_lower = entry.content.to_lowercase();
            let tag_match = entry
                .tags
                .iter()
                .any(|t| t.to_lowercase().contains(&query_lower));
            let content_match = content_lower.contains(&query_lower);

            if is_legacy_unscoped_tool_result_iri(&entry.iri) {
                continue;
            }
            if tag_match || content_match {
                let relevance = if content_match { 0.8 } else { 0.5 };
                results.push(L0SearchResult {
                    iri: entry.iri,
                    content: entry.content,
                    relevance_score: relevance,
                    importance: entry.importance,
                    tags: entry.tags,
                });
            }

            if results.len() >= limit {
                break;
            }
        }

        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    /// Scan by IRI prefix — uses redb key-order iteration, more efficient and reliable than search() content matching
    pub fn scan_iri_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<L0Entry>, CoreError> {
        let mut results = Vec::new();
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        for result in table.range(prefix..).map_err(|e| CoreError::StorageError {
            message: format!("Prefix scan failed: {}", e),
        })? {
            let (key_guard, value_guard) = result.map_err(|e| CoreError::StorageError {
                message: format!("Iteration failed: {}", e),
            })?;
            let key_str = key_guard.value();
            if !key_str.starts_with(prefix) {
                break;
            }
            if is_legacy_unscoped_tool_result_iri(key_str) {
                continue;
            }
            let entry: L0Entry = serde_json::from_slice(value_guard.value()).map_err(|e| {
                CoreError::StorageError {
                    message: format!("Failed to deserialize entry: {}", e),
                }
            })?;
            results.push(entry);
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    /// Search using tag index, falls back to full table scan on index miss
    pub fn search_with_index(&self, tags: &[String]) -> Result<Vec<L0Entry>, CoreError> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }

        let mut index_hit = true;
        let mut candidate_iris: Vec<String> = Vec::new();

        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(TAG_INDEX_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open tag index table: {}", e),
            })?;

        for tag in tags {
            let index_key = format!("tag:{}", tag);
            match table.get(index_key.as_str()) {
                Ok(Some(guard)) => {
                    let iris: Vec<String> =
                        serde_json::from_slice(guard.value()).unwrap_or_default();
                    if candidate_iris.is_empty() {
                        candidate_iris = iris;
                    } else {
                        let iris_set: std::collections::HashSet<_> = iris.into_iter().collect();
                        candidate_iris.retain(|iri| iris_set.contains(iri));
                    }
                }
                _ => {
                    index_hit = false;
                    break;
                }
            }
        }

        drop(read_txn);

        if index_hit {
            let mut results = Vec::new();
            for iri in &candidate_iris {
                if let Some(entry) = self.retrieve(iri)? {
                    results.push(entry);
                }
            }
            Ok(results)
        } else {
            self.search_by_tags_fallback(tags)
        }
    }

    /// Full table scan tag search (fallback)
    fn search_by_tags_fallback(&self, tags: &[String]) -> Result<Vec<L0Entry>, CoreError> {
        let mut results = Vec::new();

        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        for result in table.iter().map_err(|e| CoreError::StorageError {
            message: format!("Iteration failed: {}", e),
        })? {
            let (_, value) = result.map_err(|e| CoreError::StorageError {
                message: format!("Iteration failed: {}", e),
            })?;

            let entry: L0Entry =
                serde_json::from_slice(value.value()).map_err(|e| CoreError::StorageError {
                    message: format!("Failed to deserialize entry: {}", e),
                })?;

            if is_legacy_unscoped_tool_result_iri(&entry.iri) {
                continue;
            }
            if tags.iter().all(|t| entry.tags.contains(t)) {
                results.push(entry);
            }
        }

        Ok(results)
    }

    pub fn search_by_tags(&self, tags: &[String]) -> Result<Vec<L0Entry>, CoreError> {
        self.search_with_index(tags)
    }

    pub fn get_by_importance(&self, min_importance: f32) -> Result<Vec<L0Entry>, CoreError> {
        let mut results = Vec::new();

        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        for result in table.iter().map_err(|e| CoreError::StorageError {
            message: format!("Iteration failed: {}", e),
        })? {
            let (_, value) = result.map_err(|e| CoreError::StorageError {
                message: format!("Iteration failed: {}", e),
            })?;

            let entry: L0Entry =
                serde_json::from_slice(value.value()).map_err(|e| CoreError::StorageError {
                    message: format!("Failed to deserialize entry: {}", e),
                })?;

            if is_legacy_unscoped_tool_result_iri(&entry.iri) {
                continue;
            }
            if entry.importance >= min_importance {
                results.push(entry);
            }
        }

        results.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    /// Update entry MESI cache coherence state
    pub fn update_mesi_state(&self, iri: &str, state: MesiState) -> Result<(), CoreError> {
        self.assert_writable()?;
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        let value = table.get(iri).map_err(|e| CoreError::StorageError {
            message: format!("Failed to retrieve entry: {}", e),
        })?;

        match value {
            Some(guard) => {
                let mut entry: L0Entry =
                    serde_json::from_slice(guard.value()).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to deserialize entry: {}", e),
                    })?;
                drop(read_txn);
                entry.mesi_state = state;
                self.store_entry(&entry)?;
                Ok(())
            }
            None => Err(CoreError::StorageError {
                message: format!("Entry not found: {}", iri),
            }),
        }
    }

    pub fn count(&self) -> Result<u64, CoreError> {
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        table.len().map_err(|e| CoreError::StorageError {
            message: format!("Failed to get entry count: {}", e),
        })
    }

    pub fn flush(&self) -> Result<(), CoreError> {
        // redb persists to disk on commit; no explicit flush needed
        Ok(())
    }

    /// Query all entries by named graph
    pub fn query_by_named_graph(&self, graph: &str) -> Result<Vec<L0Entry>, CoreError> {
        let key = format!("graph:{}", graph);
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table =
            read_txn
                .open_table(NAMED_GRAPH_TABLE)
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to open named graph index: {}", e),
                })?;
        match table.get(key.as_str()) {
            Ok(Some(guard)) => {
                let iris: Vec<String> =
                    serde_json::from_slice(guard.value()).map_err(|e| CoreError::StorageError {
                        message: format!("Failed to deserialize named graph index: {}", e),
                    })?;
                let mut entries = Vec::new();
                for iri in iris {
                    if let Some(entry) = self.retrieve(&iri)? {
                        entries.push(entry);
                    }
                }
                Ok(entries)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Delete all entries in a named graph
    pub fn delete_named_graph(&self, graph: &str) -> Result<usize, CoreError> {
        self.assert_writable()?;
        let entries = self.query_by_named_graph(graph)?;
        let count = entries.len();

        for entry in &entries {
            self.delete(&entry.iri)?;
        }

        let key = format!("graph:{}", graph);
        let write_txn = self.db.begin_write().map_err(|e| CoreError::StorageError {
            message: format!("Write transaction failed: {}", e),
        })?;
        {
            let mut table =
                write_txn
                    .open_table(NAMED_GRAPH_TABLE)
                    .map_err(|e| CoreError::StorageError {
                        message: format!("Failed to open named graph index: {}", e),
                    })?;
            table
                .remove(key.as_str())
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to delete named graph index: {}", e),
                })?;
        }
        write_txn.commit().map_err(|e| CoreError::StorageError {
            message: format!("Failed to commit transaction: {}", e),
        })?;

        Ok(count)
    }

    /// List all named graphs
    pub fn list_named_graphs(&self) -> Result<Vec<String>, CoreError> {
        let mut graphs = Vec::new();
        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table =
            read_txn
                .open_table(NAMED_GRAPH_TABLE)
                .map_err(|e| CoreError::StorageError {
                    message: format!("Failed to open named graph index: {}", e),
                })?;
        for result in table.iter().map_err(|e| CoreError::StorageError {
            message: format!("Failed to iterate named graph index: {}", e),
        })? {
            let (key_guard, _) = result.map_err(|e| CoreError::StorageError {
                message: format!("Failed to iterate named graph index: {}", e),
            })?;
            let key_str = key_guard.value();
            if let Some(graph) = key_str.strip_prefix("graph:") {
                graphs.push(graph.to_string());
            }
        }
        Ok(graphs)
    }

    pub fn query_by_type(&self, type_iri: &str) -> Result<Vec<L0Entry>, CoreError> {
        let mut results = Vec::new();

        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        for result in table.iter().map_err(|e| CoreError::StorageError {
            message: format!("Iteration failed: {}", e),
        })? {
            let (_, value) = result.map_err(|e| CoreError::StorageError {
                message: format!("Iteration failed: {}", e),
            })?;

            let entry: L0Entry =
                serde_json::from_slice(value.value()).map_err(|e| CoreError::StorageError {
                    message: format!("Failed to deserialize entry: {}", e),
                })?;

            if entry.jsonld_types.contains(&type_iri.to_string()) {
                results.push(entry);
            }
        }

        Ok(results)
    }

    pub fn query_by_types(&self, type_iris: &[String]) -> Result<Vec<L0Entry>, CoreError> {
        if type_iris.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        let read_txn = self.db.begin_read().map_err(|e| CoreError::StorageError {
            message: format!("Read transaction failed: {}", e),
        })?;
        let table = read_txn
            .open_table(ENTRIES_TABLE)
            .map_err(|e| CoreError::StorageError {
                message: format!("Failed to open table: {}", e),
            })?;
        for result in table.iter().map_err(|e| CoreError::StorageError {
            message: format!("Iteration failed: {}", e),
        })? {
            let (_, value) = result.map_err(|e| CoreError::StorageError {
                message: format!("Iteration failed: {}", e),
            })?;

            let entry: L0Entry =
                serde_json::from_slice(value.value()).map_err(|e| CoreError::StorageError {
                    message: format!("Failed to deserialize entry: {}", e),
                })?;

            if type_iris.iter().any(|t| entry.jsonld_types.contains(t)) {
                results.push(entry);
            }
        }

        Ok(results)
    }

    /// Inject IRI registry, auto-registers @id on subsequent node writes
    pub fn set_iri_registry(&mut self, registry: Arc<IriRegistry>) {
        self.iri_registry = Some(registry);
    }

    pub fn store_jsonld_node(&self, node: &serde_json::Value) -> Result<String, CoreError> {
        self.assert_writable()?;
        let node_obj = node.as_object().ok_or_else(|| CoreError::StorageError {
            message: "JSON-LD node must be an object".to_string(),
        })?;

        let iri = node_obj
            .get("@id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::StorageError {
                message: "JSON-LD node missing @id field".to_string(),
            })?;

        let jsonld_context = node_obj
            .get("@context")
            .and_then(|v| serde_json::to_string(v).ok());

        let jsonld_types = node_obj
            .get("@type")
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(vec![s.clone()]),
                serde_json::Value::Array(arr) => Some(
                    arr.iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();

        let content = serde_json::to_string(node).map_err(|e| CoreError::StorageError {
            message: format!("Failed to serialize JSON-LD node: {}", e),
        })?;

        let content_hash = compute_content_hash(&content);

        // Determine namespace and type (for subsequent IRI registration)
        let primary_type = jsonld_types.first().cloned();

        let existing_entry = self.retrieve_without_update(iri)?;

        let entry = if let Some(existing) = existing_entry {
            let mut merged_metadata = existing.metadata.clone();
            for (key, value) in node_obj.iter() {
                if key != "@id" && key != "@type" && key != "@context" {
                    merged_metadata.insert(key.clone(), value.clone());
                }
            }

            let mut merged_types = existing.jsonld_types.clone();
            for type_iri in &jsonld_types {
                if !merged_types.contains(type_iri) {
                    merged_types.push(type_iri.clone());
                }
            }

            L0Entry {
                iri: iri.to_string(),
                content,
                importance: existing.importance,
                access_count: existing.access_count,
                created_at: existing.created_at,
                last_accessed: Utc::now(),
                tags: existing.tags.clone(),
                metadata: merged_metadata,
                mesi_state: existing.mesi_state,
                content_hash,
                named_graph: existing.named_graph.clone(),

                jsonld_context: jsonld_context.or(existing.jsonld_context.clone()),
                jsonld_types: merged_types,
                hyperspace_point_id: existing.hyperspace_point_id,
            }
        } else {
            let mut metadata = serde_json::Map::new();
            for (key, value) in node_obj.iter() {
                if key != "@id" && key != "@type" && key != "@context" {
                    metadata.insert(key.clone(), value.clone());
                }
            }

            L0Entry {
                iri: iri.to_string(),
                content,
                importance: 0.5,
                access_count: 0,
                created_at: Utc::now(),
                last_accessed: Utc::now(),
                tags: Vec::new(),
                metadata,
                mesi_state: MesiState::Shared,
                content_hash,
                named_graph: None,

                jsonld_context,
                jsonld_types,
                hyperspace_point_id: None,
            }
        };

        self.store_entry(&entry)?;

        // If IRI registry available, auto-register newly written @id
        if let Some(ref registry) = self.iri_registry {
            let ns = primary_type
                .as_ref()
                .map(|t| t.to_lowercase())
                .unwrap_or_else(|| "node".to_string());
            let named_graph = entry
                .named_graph
                .clone()
                .unwrap_or_else(|| format!("graph:{}", ns));
            let location = EntityLocation {
                iri: iri.to_string(),
                namespace: ns,
                named_graph: Some(named_graph),
                storage_layer: StorageLayer::L0Permanent,
                entity_type: primary_type.clone(),
                created_at: Utc::now(),
                metadata: Default::default(),
            };
            registry.register(iri, location);
        }

        Ok(iri.to_string())
    }

    pub fn retrieve_jsonld_node(&self, iri: &str) -> Result<Option<serde_json::Value>, CoreError> {
        match self.retrieve(iri)? {
            Some(entry) => {
                let mut node = serde_json::Map::new();

                node.insert(
                    "@id".to_string(),
                    serde_json::Value::String(entry.iri.clone()),
                );

                if let Some(context) = entry.jsonld_context {
                    if let Ok(context_value) = serde_json::from_str(&context) {
                        node.insert("@context".to_string(), context_value);
                    }
                }

                if !entry.jsonld_types.is_empty() {
                    if entry.jsonld_types.len() == 1 {
                        node.insert(
                            "@type".to_string(),
                            serde_json::Value::String(entry.jsonld_types[0].clone()),
                        );
                    } else {
                        node.insert(
                            "@type".to_string(),
                            serde_json::Value::Array(
                                entry
                                    .jsonld_types
                                    .into_iter()
                                    .map(serde_json::Value::String)
                                    .collect(),
                            ),
                        );
                    }
                }

                for (key, value) in entry.metadata {
                    node.insert(key, value);
                }

                Ok(Some(serde_json::Value::Object(node)))
            }
            None => Ok(None),
        }
    }
}

/// Memory compressor for L2 -> L0 archival
pub struct MemoryCompressor;

impl MemoryCompressor {
    pub fn compress_session(
        session_id: &str,
        task_id: &str,
        agent_role: &str,
        summary: &str,
    ) -> L0Entry {
        let content_hash = compute_content_hash(summary);
        L0Entry {
            iri: format!("iri://memory/{}", uuid::Uuid::new_v4().hyphenated()),
            content: summary.to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: vec![
                format!("session:{}", session_id),
                format!("task:{}", task_id),
                format!("role:{}", agent_role),
            ],
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash,
            named_graph: Some(format!("session:{}", session_id)),
            jsonld_context: None,
            jsonld_types: vec!["Memory".to_string()],
            hyperspace_point_id: None,
        }
    }

    pub fn compress_nodes(nodes: &[String]) -> String {
        format!(
            r#"{{"@type":"Summary","node_count":{},"compressed_at":"{}"}}"#,
            nodes.len(),
            Utc::now().to_rfc3339()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// #355: every run of one tenant (any project) must get the same open
    /// handle; opening `l0.redb` again in-process fails on redb's file lock.
    #[test]
    fn tenant_registry_shares_one_handle_per_tenant_across_runs_and_projects() {
        let dir = tempdir().unwrap();
        let registry = Arc::new(TenantL0Registry::new(dir.path()));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let claims = IsolationClaims::from_verified(
                        "acme",
                        format!("project-{}", i % 4),
                        "actor",
                    )
                    .unwrap();
                    barrier.wait();
                    registry.get_or_open(&claims)
                })
            })
            .collect();
        let stores: Vec<Arc<L0Store>> = handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap()
                    .expect("same-tenant open must not hit the redb lock")
            })
            .collect();
        assert!(stores.iter().all(|s| Arc::ptr_eq(s, &stores[0])));
        assert_eq!(registry.open_handles(), 1);

        // The shared handle is writable and the writes are visible to all runs.
        let entry = L0Entry {
            iri: "iri://acme/shared".to_string(),
            content: "{}".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: Vec::new(),
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: Vec::new(),
            hyperspace_point_id: None,
        };
        stores[3].store_entry(&entry).unwrap();
        assert!(stores[11].retrieve("iri://acme/shared").unwrap().is_some());

        let other = IsolationClaims::from_verified("other", "project-0", "actor").unwrap();
        let other_store = registry.get_or_open(&other).unwrap();
        assert!(!Arc::ptr_eq(&other_store, &stores[0]));
        assert!(other_store.retrieve("iri://acme/shared").unwrap().is_none());
        assert_eq!(registry.open_handles(), 2);
    }

    #[test]
    fn tenant_registry_releases_only_idle_handles_and_unlocks_the_file() {
        let dir = tempdir().unwrap();
        let registry = TenantL0Registry::new(dir.path());
        let acme = IsolationClaims::from_verified("acme", "p", "actor").unwrap();
        let other = IsolationClaims::from_verified("other", "p", "actor").unwrap();

        let in_use = registry.get_or_open(&acme).unwrap();
        drop(registry.get_or_open(&other).unwrap());

        assert_eq!(registry.release_idle(), 1, "only the unused handle closes");
        assert_eq!(registry.open_handles(), 1);
        assert!(Arc::ptr_eq(&in_use, &registry.get_or_open(&acme).unwrap()));

        // The released tenant's file lock is gone: it can be opened again.
        drop(L0Store::open_for_claims(dir.path(), &other).unwrap());

        drop(in_use);
        assert_eq!(registry.release_idle(), 1);
        assert_eq!(registry.open_handles(), 0);
        drop(L0Store::open_for_claims(dir.path(), &acme).unwrap());
    }

    #[test]
    fn one_tenant_open_does_not_block_another_tenant() {
        let dir = tempdir().unwrap();
        let registry = Arc::new(TenantL0Registry::new(dir.path()));
        let release = Arc::new(std::sync::Barrier::new(2));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_hook = release.clone();
        let started_hook = started.clone();
        let slow = dir.path().join("slow");
        registry.set_before_open_for_test(move |path| {
            if path == slow {
                started_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                release_hook.wait();
            }
        });
        let slow_claims = IsolationClaims::from_verified("slow", "p", "actor").unwrap();
        let fast_claims = IsolationClaims::from_verified("fast", "p", "actor").unwrap();
        let registry_slow = registry.clone();
        let opener = std::thread::spawn(move || registry_slow.get_or_open(&slow_claims));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !started.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "slow tenant open never reached the database step"
            );
            std::thread::yield_now();
        }
        let fast = registry.get_or_open(&fast_claims);
        assert!(
            fast.is_ok(),
            "another tenant must open while the first tenant is still inside Database::create"
        );
        release.wait();
        assert!(opener.join().unwrap().is_ok());
    }

    #[test]
    fn tenant_handle_count_is_bounded_and_idle_handles_are_evicted() {
        let dir = tempdir().unwrap();
        let registry = TenantL0Registry::with_handle_limit(dir.path(), 2);
        assert_eq!(registry.handle_limit(), 2);
        assert!(L0_REDB_CACHE_BYTES < 1024 * 1024 * 1024);
        let a = IsolationClaims::from_verified("a", "p", "actor").unwrap();
        let b = IsolationClaims::from_verified("b", "p", "actor").unwrap();
        let c = IsolationClaims::from_verified("c", "p", "actor").unwrap();
        let hold_a = registry.get_or_open(&a).unwrap();
        let _hold_b = registry.get_or_open(&b).unwrap();
        let denied = match registry.get_or_open(&c) {
            Err(error) => error,
            Ok(_) => panic!("opening a third tenant must fail once the handle limit is reached"),
        };
        let text = denied.to_string();
        assert!(text.contains("handle limit"), "{text}");
        assert!(
            !text.contains(&dir.path().display().to_string()),
            "limit error must not include the L0 root: {text}"
        );
        drop(hold_a);
        assert!(registry.get_or_open(&c).is_ok());
        assert!(registry.open_handles() <= 2);
    }

    #[test]
    fn lease_releases_the_handle_when_the_run_panics() {
        let dir = tempdir().unwrap();
        let registry = Arc::new(TenantL0Registry::new(dir.path()));
        let claims = IsolationClaims::from_verified("acme", "p", "actor").unwrap();
        let registry_run = registry.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lease = registry_run.lease();
            let handle = registry_run.get_or_open(&claims).unwrap();
            let _also = handle.clone();
            panic!("run failed");
        }));
        assert!(panicked.is_err());
        assert_eq!(registry.open_handles(), 0);
    }

    #[test]
    fn permission_errors_do_not_include_the_l0_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().display().to_string();
        let mixed = IsolationClaims::from_verified("Acme", "p", "actor").unwrap();
        let denied = match TenantL0Registry::new(dir.path()).get_or_open(&mixed) {
            Err(error) => error,
            Ok(_) => panic!("a mixed-case tenant id must be rejected"),
        };
        let text = denied.to_string();
        assert!(
            !text.contains(&root) && !text.contains("Acme"),
            "permission error leaked a path or tenant segment: {text}"
        );
        assert!(text.contains("lowercase"), "{text}");

        let legacy = L0Store::open_legacy_readonly(dir.path().to_str().unwrap()).unwrap();
        let write = legacy.store("iri://test/1", "x").unwrap_err();
        let write_text = write.to_string();
        assert!(
            !write_text.contains(&root),
            "no-claims write error leaked the L0 root: {write_text}"
        );
        assert!(
            write_text.contains("without verified claims"),
            "{write_text}"
        );
    }

    #[test]
    fn legacy_unscoped_tool_result_iris_are_unreadable_and_purged() {
        let dir = tempdir().unwrap();
        let legacy = "iri://tool-result/call_old";
        let owned = "iri://tool-result/tenant/project/run/agent/call_old";
        {
            let store = L0Store::open_writable(dir.path()).unwrap();
            store.store(legacy, "legacy-secret").unwrap();
            store.store(owned, "owned").unwrap();
            assert!(store.retrieve(legacy).unwrap().is_none());
            assert!(store
                .scan_iri_prefix("iri://tool-result/", 10)
                .unwrap()
                .iter()
                .all(|entry| entry.iri != legacy));
            assert_eq!(store.retrieve(owned).unwrap().unwrap().content, "owned");
            assert_eq!(store.count().unwrap(), 2);
        }
        let reopened = L0Store::open_writable(dir.path()).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
        assert!(reopened.retrieve(legacy).unwrap().is_none());
        assert_eq!(reopened.retrieve(owned).unwrap().unwrap().content, "owned");
    }

    fn test_store(dir: &tempfile::TempDir) -> L0Store {
        let claims =
            IsolationClaims::from_verified("test-tenant", "test-project", "test-actor").unwrap();
        L0Store::open_for_claims(dir.path(), &claims).unwrap()
    }

    #[test]
    fn test_l0_store() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        store.store("iri://test/1", r#"{"test": true}"#).unwrap();

        let retrieved = store.retrieve("iri://test/1").unwrap();
        assert!(retrieved.is_some());
        let entry = retrieved.unwrap();
        assert_eq!(entry.mesi_state, MesiState::Shared);
        assert!(!entry.content_hash.is_empty());
    }

    #[test]
    fn tenant_scoped_stores_do_not_expose_other_tenant_entries() {
        let dir = tempdir().unwrap();
        let acme = IsolationClaims::from_verified("acme", "project", "actor").unwrap();
        let other = IsolationClaims::from_verified("other", "project", "actor").unwrap();
        let acme_path = dir.path().join("acme");
        let other_path = dir.path().join("other");

        assert!(!acme_path.exists());
        assert!(!other_path.exists());

        let acme_store = L0Store::open_for_claims(dir.path(), &acme).unwrap();
        acme_store
            .store("iri://checkpoint/task/1", "acme-only")
            .unwrap();
        let other_store = L0Store::open_for_claims(dir.path(), &other).unwrap();

        assert!(acme_path.join("l0.redb").exists());
        assert!(other_path.join("l0.redb").exists());
        assert!(other_store
            .retrieve("iri://checkpoint/task/1")
            .unwrap()
            .is_none());
        assert_eq!(
            acme_store
                .retrieve("iri://checkpoint/task/1")
                .unwrap()
                .unwrap()
                .content,
            "acme-only"
        );
    }

    #[test]
    fn legacy_l0_store_rejects_writes_without_claims() {
        let dir = tempdir().unwrap();
        let db = Database::create(dir.path().join("l0.redb")).unwrap();
        let write_txn = db.begin_write().unwrap();
        write_txn.open_table(ENTRIES_TABLE).unwrap();
        write_txn.open_table(TAG_INDEX_TABLE).unwrap();
        write_txn.open_table(NAMED_GRAPH_TABLE).unwrap();
        write_txn.commit().unwrap();
        drop(db);
        let store = L0Store::open_legacy_readonly(dir.path().to_str().unwrap()).unwrap();

        let error = store.store("iri://test/1", "unclaimed").unwrap_err();

        assert!(matches!(error, CoreError::PermissionDenied { .. }));
    }

    #[test]
    fn legacy_l0_missing_starts_empty_without_creating_files() {
        let dir = tempdir().unwrap();
        // Both an empty L0 directory and a not-yet-created one are "no history".
        for root in [dir.path().to_path_buf(), dir.path().join("not-created")] {
            let store = L0Store::open_legacy_readonly(root.to_str().unwrap()).unwrap();

            assert_eq!(store.count().unwrap(), 0);
            assert!(store.retrieve("iri://test/1").unwrap().is_none());
            assert!(store.search("anything", 10).unwrap().is_empty());
            assert!(store.list_named_graphs().unwrap().is_empty());
            assert!(matches!(
                store.store("iri://test/1", "unclaimed").unwrap_err(),
                CoreError::PermissionDenied { .. }
            ));
            assert!(!root.join("l0.redb").exists());
        }
        assert!(!dir.path().join("not-created").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn legacy_l0_existing_database_reads_as_before() {
        let dir = tempdir().unwrap();
        {
            let fixture = L0Store::open_writable(dir.path()).unwrap();
            fixture.store("iri://legacy/1", "historical").unwrap();
        }
        let store = L0Store::open_legacy_readonly(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            store.retrieve("iri://legacy/1").unwrap().unwrap().content,
            "historical"
        );
        assert!(matches!(
            store.store("iri://legacy/2", "unclaimed").unwrap_err(),
            CoreError::PermissionDenied { .. }
        ));
        drop(store);

        // Data is unchanged after the read-only session. (redb itself may update
        // header bookkeeping on open; that pre-existing behaviour is not changed here.)
        let reopened = L0Store::open_legacy_readonly(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
        assert!(reopened.retrieve("iri://legacy/2").unwrap().is_none());
        assert_eq!(
            reopened
                .retrieve("iri://legacy/1")
                .unwrap()
                .unwrap()
                .content,
            "historical"
        );
    }

    #[test]
    fn legacy_l0_unreadable_file_fails_with_guidance_and_is_not_overwritten() {
        for contents in [&b""[..], &b"not a redb database, just junk bytes"[..]] {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("l0.redb");
            std::fs::write(&db_path, contents).unwrap();

            let error = match L0Store::open_legacy_readonly(dir.path().to_str().unwrap()) {
                Ok(_) => panic!("unreadable legacy L0 file must not open"),
                Err(error) => error.to_string(),
            };

            assert!(
                error.contains("Failed to open legacy read-only database"),
                "{error}"
            );
            assert!(error.contains("left unmodified"), "{error}");
            assert!(error.contains("Restore it from a backup"), "{error}");
            assert_eq!(std::fs::read(&db_path).unwrap(), contents);
        }
    }

    #[test]
    fn isolation_contract_l0_fresh_install_keeps_unclaimed_writes_closed_and_tenants_separate() {
        let dir = tempdir().unwrap();
        let legacy = L0Store::open_legacy_readonly(dir.path().to_str().unwrap()).unwrap();
        assert!(matches!(
            legacy.store("iri://shared/1", "unclaimed").unwrap_err(),
            CoreError::PermissionDenied { .. }
        ));
        assert!(matches!(
            legacy
                .update_mesi_state("iri://shared/1", MesiState::Modified)
                .unwrap_err(),
            CoreError::PermissionDenied { .. }
        ));
        assert!(!dir.path().join("l0.redb").exists());

        let tenant_a = IsolationClaims::from_verified("tenant-a", "project", "actor").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project", "actor").unwrap();
        let store_a = L0Store::open_for_claims(dir.path(), &tenant_a).unwrap();
        store_a.store("iri://task/1", "tenant-a-only").unwrap();
        let store_b = L0Store::open_for_claims(dir.path(), &tenant_b).unwrap();

        assert!(store_b.retrieve("iri://task/1").unwrap().is_none());
        assert!(legacy.retrieve("iri://task/1").unwrap().is_none());
        assert!(!dir.path().join("l0.redb").exists());
    }

    #[test]
    fn test_mesi_state_default() {
        assert_eq!(MesiState::default(), MesiState::Shared);
    }

    #[test]
    fn test_update_mesi_state() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        store.store("iri://test/mesi", "content").unwrap();
        store
            .update_mesi_state("iri://test/mesi", MesiState::Modified)
            .unwrap();

        let entry = store.retrieve("iri://test/mesi").unwrap().unwrap();
        assert_eq!(entry.mesi_state, MesiState::Modified);
    }

    #[test]
    fn test_tag_index() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let entry = L0Entry {
            iri: "iri://test/tagged".to_string(),
            content: "tagged content".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: vec!["rust".to_string(), "test".to_string()],
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: Vec::new(),
            hyperspace_point_id: None,
        };
        store.store_entry(&entry).unwrap();

        let results = store.search_by_tags(&["rust".to_string()]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].iri, "iri://test/tagged");
    }

    #[test]
    fn test_delete_cleans_tag_index() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let entry = L0Entry {
            iri: "iri://test/del".to_string(),
            content: "to be deleted".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: vec!["deleteme".to_string()],
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: Vec::new(),
            hyperspace_point_id: None,
        };
        store.store_entry(&entry).unwrap();
        store.delete("iri://test/del").unwrap();

        let results = store.search_by_tags(&["deleteme".to_string()]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_content_hash() {
        let hash1 = compute_content_hash("hello");
        let hash2 = compute_content_hash("hello");
        let hash3 = compute_content_hash("world");
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_search_with_index_fallback() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        store
            .store("iri://test/fallback", "fallback content")
            .unwrap();

        let results = store
            .search_with_index(&["nonexistent".to_string()])
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_entity_alignment() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let mut entry1 = L0Entry {
            iri: "iri://test/entity".to_string(),
            content: r#"{"name": "Alice"}"#.to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: vec!["person".to_string()],
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: Some(r#"{"@vocab": "http://example.org/"}"#.to_string()),
            jsonld_types: vec!["Person".to_string()],
            hyperspace_point_id: None,
        };
        entry1
            .metadata
            .insert("name".to_string(), serde_json::json!("Alice"));

        store.store_entry(&entry1).unwrap();

        let mut entry2 = L0Entry {
            iri: "iri://test/entity".to_string(),
            content: r#"{"name": "Alice", "age": 30}"#.to_string(),
            importance: 0.7,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: vec!["employee".to_string()],
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: vec!["Employee".to_string()],
            hyperspace_point_id: None,
        };
        entry2
            .metadata
            .insert("age".to_string(), serde_json::json!(30));

        store.store_entry(&entry2).unwrap();

        let merged = store.retrieve("iri://test/entity").unwrap().unwrap();

        assert_eq!(merged.iri, "iri://test/entity");
        assert!(merged.tags.contains(&"person".to_string()));
        assert!(merged.tags.contains(&"employee".to_string()));
        assert!(merged.jsonld_types.contains(&"Person".to_string()));
        assert!(merged.jsonld_types.contains(&"Employee".to_string()));
        assert!(merged.metadata.contains_key("name"));
        assert!(merged.metadata.contains_key("age"));
        assert_eq!(merged.importance, 0.6);
    }

    #[test]
    fn test_query_by_type() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let entry1 = L0Entry {
            iri: "iri://test/person1".to_string(),
            content: "Person 1".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: Vec::new(),
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: vec!["Person".to_string()],
            hyperspace_point_id: None,
        };
        store.store_entry(&entry1).unwrap();

        let entry2 = L0Entry {
            iri: "iri://test/person2".to_string(),
            content: "Person 2".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: Vec::new(),
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: vec!["Person".to_string(), "Employee".to_string()],
            hyperspace_point_id: None,
        };
        store.store_entry(&entry2).unwrap();

        let entry3 = L0Entry {
            iri: "iri://test/organization".to_string(),
            content: "Organization".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: Vec::new(),
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: vec!["Organization".to_string()],
            hyperspace_point_id: None,
        };
        store.store_entry(&entry3).unwrap();

        let person_results = store.query_by_type("Person").unwrap();
        assert_eq!(person_results.len(), 2);

        let employee_results = store.query_by_type("Employee").unwrap();
        assert_eq!(employee_results.len(), 1);
        assert_eq!(employee_results[0].iri, "iri://test/person2");

        let org_results = store.query_by_type("Organization").unwrap();
        assert_eq!(org_results.len(), 1);
    }

    #[test]
    fn test_query_by_types() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let entry1 = L0Entry {
            iri: "iri://test/entity1".to_string(),
            content: "Entity 1".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: Vec::new(),
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: vec!["Person".to_string()],
            hyperspace_point_id: None,
        };
        store.store_entry(&entry1).unwrap();

        let entry2 = L0Entry {
            iri: "iri://test/entity2".to_string(),
            content: "Entity 2".to_string(),
            importance: 0.5,
            access_count: 0,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            tags: Vec::new(),
            metadata: serde_json::Map::new(),
            mesi_state: MesiState::Shared,
            content_hash: String::new(),
            named_graph: None,
            jsonld_context: None,
            jsonld_types: vec!["Organization".to_string()],
            hyperspace_point_id: None,
        };
        store.store_entry(&entry2).unwrap();

        let results = store
            .query_by_types(&["Person".to_string(), "Organization".to_string()])
            .unwrap();
        assert_eq!(results.len(), 2);

        let person_only = store.query_by_types(&["Person".to_string()]).unwrap();
        assert_eq!(person_only.len(), 1);
    }

    #[test]
    fn test_jsonld_node_storage() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let node = serde_json::json!({
            "@id": "iri://test/person/alice",
            "@type": "Person",
            "@context": {
                "@vocab": "http://example.org/"
            },
            "name": "Alice",
            "age": 30
        });

        let iri = store.store_jsonld_node(&node).unwrap();
        assert_eq!(iri, "iri://test/person/alice");

        let retrieved = store
            .retrieve_jsonld_node("iri://test/person/alice")
            .unwrap();
        assert!(retrieved.is_some());

        let retrieved_node = retrieved.unwrap();
        assert_eq!(retrieved_node["@id"], "iri://test/person/alice");
        assert_eq!(retrieved_node["name"], "Alice");
        assert_eq!(retrieved_node["age"], 30);
    }

    #[test]
    fn test_jsonld_node_merge() {
        let dir = tempdir().unwrap();
        let store = test_store(&dir);

        let node1 = serde_json::json!({
            "@id": "iri://test/person/bob",
            "@type": "Person",
            "name": "Bob",
            "age": 25
        });

        store.store_jsonld_node(&node1).unwrap();

        let node2 = serde_json::json!({
            "@id": "iri://test/person/bob",
            "@type": "Employee",
            "department": "Engineering"
        });

        store.store_jsonld_node(&node2).unwrap();

        let retrieved = store
            .retrieve_jsonld_node("iri://test/person/bob")
            .unwrap()
            .unwrap();

        assert_eq!(retrieved["@id"], "iri://test/person/bob");
        assert_eq!(retrieved["name"], "Bob");
        assert_eq!(retrieved["age"], 25);
        assert_eq!(retrieved["department"], "Engineering");

        let types = retrieved["@type"].as_array().unwrap();
        assert!(types.contains(&serde_json::json!("Person")));
        assert!(types.contains(&serde_json::json!("Employee")));
    }
}
