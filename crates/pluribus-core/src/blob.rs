use crate::BlobRef;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::sync::Mutex;

pub const SHA256_ALGORITHM: &str = "sha256";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BlobUploadId(String);

impl BlobUploadId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobChunk {
    pub bytes: Vec<u8>,
    pub eof: bool,
}

#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Starts a bounded upload.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, exhausted resources, or storage failure.
    async fn begin_put(
        &self,
        media_type: &str,
        expected_size: Option<u64>,
    ) -> Result<BlobUploadId, BlobError>;

    /// Appends one contiguous chunk and returns the next expected offset.
    ///
    /// # Errors
    ///
    /// Returns `Conflict` for a non-contiguous write and an error for invalid or failed storage.
    async fn write(
        &self,
        upload_id: &BlobUploadId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, BlobError>;

    /// Atomically commits an upload under its SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing upload, size mismatch, or storage failure.
    async fn finish_put(&self, upload_id: &BlobUploadId) -> Result<BlobRef, BlobError>;

    /// Discards an upload. Missing uploads are accepted.
    ///
    /// # Errors
    ///
    /// Returns an error when temporary storage cannot be removed.
    async fn abort_put(&self, upload_id: &BlobUploadId) -> Result<(), BlobError>;

    /// Reads one verified chunk from an immutable blob.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid reference, missing content, corruption, or storage failure.
    async fn read(
        &self,
        blob: &BlobRef,
        offset: u64,
        max_bytes: usize,
    ) -> Result<BlobChunk, BlobError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobError {
    NotFound,
    Conflict {
        expected_offset: u64,
        actual_offset: u64,
    },
    Invalid(String),
    ResourceExhausted(String),
    Corrupt(String),
    Storage(String),
}

impl fmt::Display for BlobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("blob or upload not found"),
            Self::Conflict {
                expected_offset,
                actual_offset,
            } => write!(
                formatter,
                "blob offset conflict: expected {expected_offset}, got {actual_offset}"
            ),
            Self::Invalid(message) => write!(formatter, "invalid blob operation: {message}"),
            Self::ResourceExhausted(message) => {
                write!(formatter, "blob resource exhausted: {message}")
            }
            Self::Corrupt(message) => write!(formatter, "corrupt blob: {message}"),
            Self::Storage(message) => write!(formatter, "blob storage failed: {message}"),
        }
    }
}

impl Error for BlobError {}

/// Validates canonical blob-reference fields.
///
/// # Errors
///
/// Returns an error for an unsupported algorithm, malformed digest, or invalid media type.
pub fn validate_blob_ref(blob: &BlobRef) -> Result<(), BlobError> {
    if blob.algorithm != SHA256_ALGORITHM {
        return Err(BlobError::Invalid("unsupported digest algorithm".into()));
    }
    if blob.digest.len() != 64
        || !blob
            .digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BlobError::Invalid(
            "SHA-256 digest must be 64 lowercase hexadecimal characters".into(),
        ));
    }
    validate_media_type(&blob.media_type)
}

/// Validates the media-type representation accepted by the core.
///
/// # Errors
///
/// Returns an error for empty, non-ASCII, or control-bearing values.
pub fn validate_media_type(media_type: &str) -> Result<(), BlobError> {
    if media_type.is_empty()
        || !media_type.is_ascii()
        || media_type.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(BlobError::Invalid(
            "media type must be non-empty ASCII".into(),
        ))
    } else {
        Ok(())
    }
}

pub struct InMemoryBlobStore {
    max_blob_bytes: u64,
    inner: Mutex<MemoryBlobs>,
}

impl InMemoryBlobStore {
    #[must_use]
    pub fn new(max_blob_bytes: u64) -> Self {
        Self {
            max_blob_bytes,
            inner: Mutex::new(MemoryBlobs::default()),
        }
    }
}

impl Default for InMemoryBlobStore {
    fn default() -> Self {
        Self::new(64 * 1024 * 1024)
    }
}

#[derive(Default)]
struct MemoryBlobs {
    next_upload: u64,
    uploads: HashMap<BlobUploadId, MemoryUpload>,
    blobs: HashMap<String, Vec<u8>>,
}

struct MemoryUpload {
    media_type: String,
    expected_size: Option<u64>,
    bytes: Vec<u8>,
    previous_write: Option<(u64, Vec<u8>)>,
}

#[async_trait::async_trait]
impl BlobStore for InMemoryBlobStore {
    async fn begin_put(
        &self,
        media_type: &str,
        expected_size: Option<u64>,
    ) -> Result<BlobUploadId, BlobError> {
        validate_media_type(media_type)?;
        if expected_size.is_some_and(|size| size > self.max_blob_bytes) {
            return Err(BlobError::ResourceExhausted(
                "expected size exceeds blob limit".into(),
            ));
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| BlobError::Storage("blob lock poisoned".into()))?;
        inner.next_upload = inner
            .next_upload
            .checked_add(1)
            .ok_or_else(|| BlobError::Storage("upload identifier exhausted".into()))?;
        let id = BlobUploadId::new(format!("memory-upload-{}", inner.next_upload));
        inner.uploads.insert(
            id.clone(),
            MemoryUpload {
                media_type: media_type.to_owned(),
                expected_size,
                bytes: Vec::new(),
                previous_write: None,
            },
        );
        Ok(id)
    }

    async fn write(
        &self,
        upload_id: &BlobUploadId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, BlobError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| BlobError::Storage("blob lock poisoned".into()))?;
        let upload = inner
            .uploads
            .get_mut(upload_id)
            .ok_or(BlobError::NotFound)?;
        let current = u64::try_from(upload.bytes.len())
            .map_err(|_| BlobError::ResourceExhausted("blob size exceeds u64".into()))?;
        if offset != current {
            if upload
                .previous_write
                .as_ref()
                .is_some_and(|(previous_offset, previous)| {
                    *previous_offset == offset && previous.as_slice() == bytes
                })
            {
                return Ok(current);
            }
            return Err(BlobError::Conflict {
                expected_offset: current,
                actual_offset: offset,
            });
        }
        let length = u64::try_from(bytes.len())
            .map_err(|_| BlobError::ResourceExhausted("chunk size exceeds u64".into()))?;
        let next = current
            .checked_add(length)
            .ok_or_else(|| BlobError::ResourceExhausted("blob size exceeds u64".into()))?;
        if next > self.max_blob_bytes || upload.expected_size.is_some_and(|size| next > size) {
            return Err(BlobError::ResourceExhausted(
                "write exceeds declared or configured blob size".into(),
            ));
        }
        upload.bytes.extend_from_slice(bytes);
        upload.previous_write = Some((offset, bytes.to_vec()));
        Ok(next)
    }

    async fn finish_put(&self, upload_id: &BlobUploadId) -> Result<BlobRef, BlobError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| BlobError::Storage("blob lock poisoned".into()))?;
        let upload = inner.uploads.remove(upload_id).ok_or(BlobError::NotFound)?;
        let size = u64::try_from(upload.bytes.len())
            .map_err(|_| BlobError::ResourceExhausted("blob size exceeds u64".into()))?;
        if upload
            .expected_size
            .is_some_and(|expected| expected != size)
        {
            return Err(BlobError::Invalid(format!(
                "expected {} bytes, received {size}",
                upload.expected_size.unwrap_or_default()
            )));
        }
        let digest = sha256_hex(&upload.bytes);
        inner.blobs.entry(digest.clone()).or_insert(upload.bytes);
        Ok(BlobRef {
            algorithm: SHA256_ALGORITHM.into(),
            digest,
            size,
            media_type: upload.media_type,
        })
    }

    async fn abort_put(&self, upload_id: &BlobUploadId) -> Result<(), BlobError> {
        self.inner
            .lock()
            .map_err(|_| BlobError::Storage("blob lock poisoned".into()))?
            .uploads
            .remove(upload_id);
        Ok(())
    }

    async fn read(
        &self,
        blob: &BlobRef,
        offset: u64,
        max_bytes: usize,
    ) -> Result<BlobChunk, BlobError> {
        validate_blob_ref(blob)?;
        let inner = self
            .inner
            .lock()
            .map_err(|_| BlobError::Storage("blob lock poisoned".into()))?;
        let bytes = inner.blobs.get(&blob.digest).ok_or(BlobError::NotFound)?;
        let actual_size = u64::try_from(bytes.len())
            .map_err(|_| BlobError::Corrupt("stored size exceeds u64".into()))?;
        if actual_size != blob.size || sha256_hex(bytes) != blob.digest {
            return Err(BlobError::Corrupt(
                "reference does not match stored bytes".into(),
            ));
        }
        let start = usize::try_from(offset)
            .map_err(|_| BlobError::Invalid("offset exceeds addressable range".into()))?;
        if start > bytes.len() {
            return Err(BlobError::Invalid("offset exceeds blob size".into()));
        }
        let end = start.saturating_add(max_bytes).min(bytes.len());
        Ok(BlobChunk {
            bytes: bytes[start..end].to_vec(),
            eof: end == bytes.len(),
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn uploads_are_contiguous_replayable_and_deduplicated() {
        let store = InMemoryBlobStore::new(16);
        let first = store.begin_put("text/plain", Some(5)).await.unwrap();

        assert_eq!(store.write(&first, 0, b"hel").await.unwrap(), 3);
        assert_eq!(store.write(&first, 0, b"hel").await.unwrap(), 3);
        assert!(matches!(
            store.write(&first, 1, b"x").await,
            Err(BlobError::Conflict { .. })
        ));
        assert_eq!(store.write(&first, 3, b"lo").await.unwrap(), 5);
        let first_ref = store.finish_put(&first).await.unwrap();

        let second = store.begin_put("text/plain", None).await.unwrap();
        store.write(&second, 0, b"hello").await.unwrap();
        let second_ref = store.finish_put(&second).await.unwrap();

        assert_eq!(first_ref, second_ref);
        assert_eq!(store.read(&first_ref, 1, 2).await.unwrap().bytes, b"el");
        assert!(!store.read(&first_ref, 1, 2).await.unwrap().eof);
        assert!(store.read(&first_ref, 5, 0).await.unwrap().eof);
    }

    #[tokio::test]
    async fn size_mismatch_consumes_the_upload() {
        let store = InMemoryBlobStore::new(16);
        let upload = store.begin_put("text/plain", Some(2)).await.unwrap();
        store.write(&upload, 0, b"x").await.unwrap();

        assert!(matches!(
            store.finish_put(&upload).await,
            Err(BlobError::Invalid(_))
        ));
        assert_eq!(store.finish_put(&upload).await, Err(BlobError::NotFound));
    }
}
