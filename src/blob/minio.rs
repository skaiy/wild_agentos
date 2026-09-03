//! MinIO / S3 兼容后端（rust-s3，path-style，rustls）。

use async_trait::async_trait;
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;

use super::{env_nonempty, scoped_key, BlobStore};
use crate::isolation::IsolationClaims;
use crate::CoreError;

/// S3 兼容对象存储（MinIO）。经 `MINIO_*` 环境变量配置，采用 path-style 寻址。
pub struct MinioBlobStore {
    bucket: Box<Bucket>,
    bucket_name: String,
}

impl MinioBlobStore {
    /// 从环境变量构造：`MINIO_ENDPOINT`/`MINIO_ACCESS_KEY`/`MINIO_SECRET_KEY`
    /// 必填；`MINIO_BUCKET`(默认 waos-kb)/`MINIO_REGION`(默认 us-east-1) 可选。
    pub fn from_env() -> Result<Self, String> {
        let endpoint = env_nonempty("MINIO_ENDPOINT").ok_or("MINIO_ENDPOINT 未设置")?;
        let access = env_nonempty("MINIO_ACCESS_KEY").ok_or("MINIO_ACCESS_KEY 未设置")?;
        let secret = env_nonempty("MINIO_SECRET_KEY").ok_or("MINIO_SECRET_KEY 未设置")?;
        let bucket_name = env_nonempty("MINIO_BUCKET").unwrap_or_else(|| "waos-kb".into());
        let region_name = env_nonempty("MINIO_REGION").unwrap_or_else(|| "us-east-1".into());

        let region = Region::Custom {
            region: region_name,
            endpoint,
        };
        let creds = Credentials::new(Some(&access), Some(&secret), None, None, None)
            .map_err(|e| format!("credentials: {e}"))?;
        let bucket = Bucket::new(&bucket_name, region, creds)
            .map_err(|e| format!("bucket: {e}"))?
            .with_path_style();
        Ok(Self {
            bucket,
            bucket_name,
        })
    }

    pub fn bucket_name(&self) -> &str {
        &self.bucket_name
    }
}

#[async_trait]
impl BlobStore for MinioBlobStore {
    async fn put(
        &self,
        claims: &IsolationClaims,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), CoreError> {
        let key = scoped_key(claims, key)?;
        let resp = self
            .bucket
            .put_object_with_content_type(&key, bytes, content_type)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("minio put: {e}"),
            })?;
        let code = resp.status_code();
        if !(200..300).contains(&code) {
            return Err(CoreError::Internal {
                message: format!("minio put status {code}"),
            });
        }
        Ok(())
    }

    async fn get(&self, claims: &IsolationClaims, key: &str) -> Result<Vec<u8>, CoreError> {
        let key = scoped_key(claims, key)?;
        let resp = self
            .bucket
            .get_object(&key)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("minio get: {e}"),
            })?;
        let code = resp.status_code();
        if !(200..300).contains(&code) {
            return Err(CoreError::Internal {
                message: format!("minio get status {code}"),
            });
        }
        Ok(resp.bytes().to_vec())
    }

    async fn delete(&self, claims: &IsolationClaims, key: &str) -> Result<(), CoreError> {
        let key = scoped_key(claims, key)?;
        self.bucket
            .delete_object(&key)
            .await
            .map_err(|e| CoreError::Internal {
                message: format!("minio delete: {e}"),
            })?;
        Ok(())
    }

    async fn exists(&self, claims: &IsolationClaims, key: &str) -> Result<bool, CoreError> {
        let key = scoped_key(claims, key)?;
        match self.bucket.head_object(&key).await {
            Ok((_, code)) => Ok((200..300).contains(&code)),
            Err(_) => Ok(false),
        }
    }

    fn backend(&self) -> &'static str {
        "minio"
    }
}
