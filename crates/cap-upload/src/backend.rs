//! Upload backends: direct S3 (`object_store`) and presigned PUTs via `ingest-api`.

use crate::api::{ApiClient, CompleteRequest, CreateSessionRequest, FileInfo, UploadRequest};
use crate::segment::{object_key, valid_key_component, SegmentFile, VerifiedSegment};
use crate::{Error, Result};
use object_store::{path::Path as ObjPath, ObjectStore, PutPayload, WriteMultipart};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::AsyncReadExt;

pub(crate) enum Backend {
    Store(StoreBackend),
    Presigned(PresignedBackend),
}

impl Backend {
    /// Upload every file (manifest last) and confirm sizes on the server side.
    /// Returns only when every object is confirmed present with the right size.
    pub(crate) async fn upload_and_confirm(&self, seg: &VerifiedSegment) -> Result<()> {
        match self {
            Backend::Store(b) => b.upload_and_confirm(seg).await,
            Backend::Presigned(b) => b.upload_and_confirm(seg).await,
        }
    }
}

pub(crate) struct StoreBackend {
    pub store: Arc<dyn ObjectStore>,
    pub user_id: String,
    pub multipart_threshold: u64,
    pub part_size: usize,
}

impl StoreBackend {
    async fn upload_and_confirm(&self, seg: &VerifiedSegment) -> Result<()> {
        let m = &seg.manifest;
        if !valid_key_component(&self.user_id) {
            return Err(Error::Permanent(format!("invalid user_id {:?}", self.user_id)));
        }
        if !valid_key_component(&m.session_id) {
            return Err(Error::Permanent(format!("invalid session_id {:?}", m.session_id)));
        }
        for f in seg.upload_order() {
            let key = ObjPath::from(object_key(&self.user_id, &m.session_id, m.segment_idx, &f.name));
            self.put_file(&key, f).await?;
            tracing::debug!(key = %key, size = f.size, "uploaded");
        }
        // HEAD every object and compare sizes.
        for f in seg.upload_order() {
            let key = ObjPath::from(object_key(&self.user_id, &m.session_id, m.segment_idx, &f.name));
            let meta = self.store.head(&key).await.map_err(|e| Error::Store(e.to_string()))?;
            if meta.size as u64 != f.size {
                return Err(Error::SizeMismatch(format!("{key}: remote {} != local {}", meta.size, f.size)));
            }
        }
        Ok(())
    }

    async fn put_file(&self, key: &ObjPath, f: &SegmentFile) -> Result<()> {
        let mut file = tokio::fs::File::open(&f.path).await?;
        if f.size <= self.multipart_threshold {
            let mut buf = Vec::with_capacity(f.size as usize);
            file.read_to_end(&mut buf).await?;
            if buf.len() as u64 != f.size {
                return Err(Error::Permanent(format!("{} changed size during upload", f.path.display())));
            }
            self.store
                .put(key, PutPayload::from(buf))
                .await
                .map_err(|e| Error::Store(e.to_string()))?;
            return Ok(());
        }
        let upload = self.store.put_multipart(key).await.map_err(|e| Error::Store(e.to_string()))?;
        let mut w = WriteMultipart::new_with_chunk_size(upload, self.part_size);
        let mut buf = vec![0u8; self.part_size.min(8 * 1024 * 1024)];
        let res: Result<()> = async {
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                w.wait_for_capacity(4).await.map_err(|e| Error::Store(e.to_string()))?;
                w.write(&buf[..n]);
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                w.finish().await.map_err(|e| Error::Store(e.to_string()))?;
                Ok(())
            }
            Err(e) => {
                let _ = w.abort().await;
                Err(e)
            }
        }
    }
}

pub(crate) struct PresignedBackend {
    pub api: ApiClient,
    pub token: Arc<RwLock<String>>,
    pub consent_version: String,
    pub sessions: Mutex<HashSet<String>>,
}

impl PresignedBackend {
    fn token(&self) -> String {
        self.token.read().unwrap().clone()
    }

    async fn upload_and_confirm(&self, seg: &VerifiedSegment) -> Result<()> {
        let m = &seg.manifest;
        let token = self.token();
        let known = self.sessions.lock().unwrap().contains(&m.session_id);
        if !known {
            let req = CreateSessionRequest {
                session_id: Some(m.session_id.clone()),
                game_id: m.game_id.clone(),
                client_version: m.client_version.clone(),
                consent_version: self.consent_version.clone(),
            };
            self.api.create_session(&token, &req).await.map_err(api_err)?;
            self.sessions.lock().unwrap().insert(m.session_id.clone());
        }

        let infos: Vec<FileInfo> = seg
            .upload_order()
            .map(|f| FileInfo { name: f.name.clone(), size: f.size, blake3: f.blake3.clone() })
            .collect();
        let resp = self
            .api
            .request_upload(&token, &m.session_id, m.segment_idx, &UploadRequest { files: infos.clone() })
            .await
            .map_err(api_err)?;

        for f in seg.upload_order() {
            let target = resp
                .files
                .iter()
                .find(|p| p.name == f.name)
                .ok_or_else(|| Error::Http(format!("server returned no URL for {}", f.name)))?;
            self.put_presigned(&target.url, f).await?;
        }

        // The server HEADs every object and compares sizes before accepting.
        let done = self
            .api
            .complete(
                &token,
                &m.session_id,
                m.segment_idx,
                &CompleteRequest { files: infos, dropped_frames: m.dropped_frames, frame_count: m.frame_count },
            )
            .await
            .map_err(api_err)?;
        if done.state != "ready" {
            return Err(Error::SizeMismatch(format!("server reported segment state {}", done.state)));
        }
        Ok(())
    }

    async fn put_presigned(&self, url: &str, f: &SegmentFile) -> Result<()> {
        let file = tokio::fs::File::open(&f.path).await?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024));
        let resp = self
            .api
            .http()
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, f.size)
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Http(format!("PUT {} -> {status}: {text}", f.name)));
        }
        Ok(())
    }
}

/// 403/404/409/422 from the API mean the server will never accept this segment
/// (blocked game, consent mismatch, conflict): stop retrying. 401 (expired
/// token), 429 and 5xx stay retryable.
fn api_err(e: Error) -> Error {
    match e {
        Error::Api { status, code, message } if matches!(status, 403 | 404 | 409 | 422) => {
            Error::Permanent(format!("api {status} {code}: {message}"))
        }
        other => other,
    }
}

pub(crate) fn build_s3(
    endpoint: &str,
    region: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    allow_http: bool,
) -> Result<Arc<dyn ObjectStore>> {
    let s3 = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_region(region)
        .with_bucket_name(bucket)
        .with_access_key_id(access_key)
        .with_secret_access_key(secret_key)
        .with_allow_http(allow_http)
        .with_virtual_hosted_style_request(false)
        .build()
        .map_err(|e| Error::Store(e.to_string()))?;
    Ok(Arc::new(s3))
}
