//! Object storage: presigned PUT URLs, HEAD checks, prefix deletion.

use crate::Config;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path as ObjPath;
use object_store::signer::Signer;
use object_store::ObjectStore;
use std::time::Duration;

pub struct Storage {
    /// Used for HEAD / LIST / DELETE.
    store: AmazonS3,
    /// Used only for signing (endpoint as seen by clients).
    signer: AmazonS3,
}

fn build(cfg: &Config, endpoint: &str) -> object_store::Result<AmazonS3> {
    AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_region(&cfg.s3_region)
        .with_bucket_name(&cfg.s3_bucket)
        .with_access_key_id(&cfg.s3_access_key)
        .with_secret_access_key(&cfg.s3_secret_key)
        .with_allow_http(cfg.s3_allow_http)
        .with_virtual_hosted_style_request(false)
        .build()
}

impl Storage {
    pub fn from_config(cfg: &Config) -> object_store::Result<Self> {
        let store = build(cfg, &cfg.s3_endpoint)?;
        let signer = build(cfg, cfg.s3_public_endpoint.as_deref().unwrap_or(&cfg.s3_endpoint))?;
        Ok(Self { store, signer })
    }

    pub async fn presign_put(&self, key: &str, ttl: Duration) -> object_store::Result<String> {
        let url = self.signer.signed_url(reqwest::Method::PUT, &ObjPath::from(key), ttl).await?;
        Ok(url.to_string())
    }

    /// Object size, or `None` if it does not exist.
    pub async fn size(&self, key: &str) -> object_store::Result<Option<u64>> {
        match self.store.head(&ObjPath::from(key)).await {
            Ok(m) => Ok(Some(m.size as u64)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete every object under `prefix/`. Returns how many were deleted.
    pub async fn delete_prefix(&self, prefix: &str) -> object_store::Result<u64> {
        let p = ObjPath::from(prefix);
        let metas: Vec<_> = self.store.list(Some(&p)).try_collect().await?;
        let mut n = 0;
        for m in metas {
            match self.store.delete(&m.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => n += 1,
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    pub fn store(&self) -> &AmazonS3 {
        &self.store
    }
}
