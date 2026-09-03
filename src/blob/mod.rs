//! Blob 对象存储抽象（原文持久化）。
//!
//! 为知识库上传的原始文件提供落盘/对象存储能力，是「重建索引 / 原文预览 / 溯源」的前置。
//! 两个实现：`MinioBlobStore`（S3 兼容，生产默认）与 `LocalFsBlobStore`（PVC 兜底）。
//! 由 `open_blob_store()` 依环境变量选择后端。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::isolation::IsolationClaims;
use crate::CoreError;

mod minio;
pub use minio::MinioBlobStore;

/// Tenant-scoped object storage.
///
/// Callers provide a key relative to the verified claims prefix, such as
/// `kb/<kbid>/<sha256>`. The backend mints `{tenant}/` with
/// [`IsolationClaims::object_key_prefix`]; there is no unscoped operation.
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(
        &self,
        claims: &IsolationClaims,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), CoreError>;
    async fn get(&self, claims: &IsolationClaims, key: &str) -> Result<Vec<u8>, CoreError>;
    async fn delete(&self, claims: &IsolationClaims, key: &str) -> Result<(), CoreError>;
    async fn exists(&self, claims: &IsolationClaims, key: &str) -> Result<bool, CoreError>;
    /// 后端标识（写入文档台账 blob_ref.backend）。
    fn backend(&self) -> &'static str;
}

pub(crate) fn scoped_key(claims: &IsolationClaims, key: &str) -> Result<String, CoreError> {
    if key.is_empty()
        || key.starts_with('/')
        || key
            .split('/')
            .any(|component| component == "." || component == "..")
    {
        return Err(CoreError::Internal {
            message: format!("非法相对 blob key: {key}"),
        });
    }

    let prefix = claims
        .object_key_prefix()
        .map_err(|error| CoreError::Internal {
            message: format!("无法创建 blob tenant 前缀: {error}"),
        })?;
    Ok(format!("{prefix}{key}"))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

/// 依环境变量构造 BlobStore。
/// `BLOB_BACKEND`=minio|local；缺省时：设了 `MINIO_ENDPOINT` 走 minio，否则 local。
pub fn open_blob_store() -> Option<Arc<dyn BlobStore>> {
    let backend = env_nonempty("BLOB_BACKEND").unwrap_or_else(|| {
        if env_nonempty("MINIO_ENDPOINT").is_some() {
            "minio".into()
        } else {
            "local".into()
        }
    });
    match backend.as_str() {
        "minio" => match MinioBlobStore::from_env() {
            Ok(s) => {
                tracing::info!(bucket = %s.bucket_name(), "BlobStore: MinIO 已接入");
                Some(Arc::new(s))
            }
            Err(e) => {
                tracing::warn!("BlobStore: MinIO 初始化失败({e})，回退 LocalFs");
                Some(Arc::new(LocalFsBlobStore::from_env()))
            }
        },
        _ => {
            let s = LocalFsBlobStore::from_env();
            tracing::info!(root = %s.root_display(), "BlobStore: LocalFs 已接入");
            Some(Arc::new(s))
        }
    }
}

// ── LocalFs 实现 ──────────────────────────────────────────────────────────────

/// 本地文件系统实现（生产可落在 waos-data PVC 的 `data/blobs`）。
pub struct LocalFsBlobStore {
    root: PathBuf,
}

impl LocalFsBlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn from_env() -> Self {
        let root = env_nonempty("BLOB_LOCAL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::api::http::data_dir().join("blobs"));
        Self { root }
    }
    fn root_display(&self) -> String {
        self.root.display().to_string()
    }
    /// 拒绝路径穿越，返回 root 下的安全绝对路径。
    fn safe_path(&self, key: &str) -> Result<PathBuf, CoreError> {
        if key.is_empty() || key.split('/').any(|c| c == ".." || c == ".") {
            return Err(CoreError::Internal {
                message: format!("非法 blob key: {key}"),
            });
        }
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl BlobStore for LocalFsBlobStore {
    async fn put(
        &self,
        claims: &IsolationClaims,
        key: &str,
        bytes: &[u8],
        _content_type: &str,
    ) -> Result<(), CoreError> {
        let path = self.safe_path(&scoped_key(claims, key)?)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Internal {
                    message: format!("blob mkdir: {e}"),
                })?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("blob write: {e}"),
            })
    }
    async fn get(&self, claims: &IsolationClaims, key: &str) -> Result<Vec<u8>, CoreError> {
        let path = self.safe_path(&scoped_key(claims, key)?)?;
        tokio::fs::read(&path)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("blob read: {e}"),
            })
    }
    async fn delete(&self, claims: &IsolationClaims, key: &str) -> Result<(), CoreError> {
        let path = self.safe_path(&scoped_key(claims, key)?)?;
        match tokio::fs::remove_file(&path).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::Internal {
                message: format!("blob delete: {e}"),
            }),
        }
    }
    async fn exists(&self, claims: &IsolationClaims, key: &str) -> Result<bool, CoreError> {
        let path = self.safe_path(&scoped_key(claims, key)?)?;
        Ok(tokio::fs::metadata(&path).await.is_ok())
    }
    fn backend(&self) -> &'static str {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobStore, LocalFsBlobStore};
    use crate::isolation::IsolationClaims;

    #[tokio::test]
    async fn tenant_cannot_get_another_tenants_new_object() {
        let root = std::env::temp_dir().join(format!("wild-agentos-blob-{}", uuid::Uuid::new_v4()));
        let store = LocalFsBlobStore::new(root.clone());
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project", "actor-b").unwrap();
        let key = "kb/source/chunk";

        store
            .put(&tenant_a, key, b"tenant-a data", "text/plain")
            .await
            .unwrap();

        assert_eq!(store.get(&tenant_a, key).await.unwrap(), b"tenant-a data");
        assert!(store.get(&tenant_b, key).await.is_err());

        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
