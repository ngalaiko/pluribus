//! Durable content-addressed blob storage.

use pluribus_core::{
    BlobChunk, BlobError, BlobRef, BlobStore, BlobUploadId, SHA256_ALGORITHM, validate_blob_ref,
    validate_media_type,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::{NamedTempFile, TempDir, TempPath};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex as AsyncMutex;

type Upload = Arc<AsyncMutex<Option<FileUpload>>>;

pub struct FileBlobStore {
    objects: PathBuf,
    upload_directory: Arc<TempDir>,
    max_blob_bytes: u64,
    inner: Mutex<StoreState>,
    verified: Arc<Mutex<HashSet<BlobRef>>>,
}

#[derive(Default)]
struct StoreState {
    next_upload: u64,
    uploads: HashMap<BlobUploadId, Upload>,
}

struct FileUpload {
    file: File,
    path: TempPath,
    media_type: String,
    expected_size: Option<u64>,
    size: u64,
    previous_write: Option<PreviousWrite>,
    _directory: Arc<TempDir>,
}

struct PreviousWrite {
    offset: u64,
    length: usize,
    digest: [u8; 32],
}

impl FileBlobStore {
    /// Opens or creates a filesystem blob store.
    ///
    /// # Errors
    /// Returns an error when its directories cannot be created.
    pub async fn open(root: impl AsRef<Path>, max_blob_bytes: u64) -> Result<Self, BlobError> {
        let root = root.as_ref();
        let objects = root.join("objects").join(SHA256_ALGORITHM);
        let uploads = root.join("uploads");
        fs::create_dir_all(&objects).await.map_err(storage)?;
        fs::create_dir_all(&uploads).await.map_err(storage)?;
        let upload_directory = tokio::task::spawn_blocking(move || {
            tempfile::Builder::new()
                .prefix("session-")
                .tempdir_in(uploads)
        })
        .await
        .map_err(storage)?
        .map_err(storage)?;
        Ok(Self {
            objects,
            upload_directory: Arc::new(upload_directory),
            max_blob_bytes,
            inner: Mutex::new(StoreState::default()),
            verified: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    fn object_path(&self, digest: &str) -> PathBuf {
        self.objects.join(&digest[..2]).join(&digest[2..])
    }

    fn upload(&self, id: &BlobUploadId) -> Result<Upload, BlobError> {
        self.inner
            .lock()
            .map_err(storage)?
            .uploads
            .get(id)
            .cloned()
            .ok_or(BlobError::NotFound)
    }
}

#[async_trait::async_trait]
impl BlobStore for FileBlobStore {
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
        let directory = self.upload_directory.clone();
        let file = tokio::task::spawn_blocking(move || NamedTempFile::new_in(directory.path()))
            .await
            .map_err(storage)?
            .map_err(storage)?;
        let (file, path) = file.into_parts();
        let mut inner = self.inner.lock().map_err(storage)?;
        inner.next_upload = inner
            .next_upload
            .checked_add(1)
            .ok_or_else(|| BlobError::Storage("upload identifier exhausted".into()))?;
        let id = BlobUploadId::new(format!("file-upload-{}", inner.next_upload));
        inner.uploads.insert(
            id.clone(),
            Arc::new(AsyncMutex::new(Some(FileUpload {
                file: File::from_std(file),
                path,
                media_type: media_type.to_owned(),
                expected_size,
                size: 0,
                previous_write: None,
                _directory: self.upload_directory.clone(),
            }))),
        );
        Ok(id)
    }

    async fn write(
        &self,
        upload_id: &BlobUploadId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, BlobError> {
        let upload = self.upload(upload_id)?;
        let bytes = bytes.to_vec();
        let limit = self.max_blob_bytes;
        // Complete an admitted write before another operation observes its offset.
        tokio::spawn(async move {
            let mut upload = upload.lock().await;
            upload
                .as_mut()
                .ok_or(BlobError::NotFound)?
                .write(offset, &bytes, limit)
                .await
        })
        .await
        .map_err(storage)?
    }

    async fn finish_put(&self, upload_id: &BlobUploadId) -> Result<BlobRef, BlobError> {
        let upload = self
            .inner
            .lock()
            .map_err(storage)?
            .uploads
            .remove(upload_id)
            .ok_or(BlobError::NotFound)?;
        let objects = self.objects.clone();
        let verified = self.verified.clone();
        tokio::spawn(async move {
            let mut guard = upload.lock().await;
            let mut upload = guard.take().ok_or(BlobError::NotFound)?;
            if upload
                .expected_size
                .is_some_and(|expected| expected != upload.size)
            {
                return Err(BlobError::Invalid(format!(
                    "expected {} bytes, received {}",
                    upload.expected_size.unwrap_or_default(),
                    upload.size
                )));
            }
            upload.file.flush().await.map_err(storage)?;
            upload.file.sync_all().await.map_err(storage)?;
            let digest = hash_file(&mut upload.file).await?;
            let blob = BlobRef {
                algorithm: SHA256_ALGORITHM.into(),
                digest,
                size: upload.size,
                media_type: upload.media_type,
            };
            let destination = objects.join(&blob.digest[..2]).join(&blob.digest[2..]);
            let parent = destination
                .parent()
                .ok_or_else(|| storage("blob destination has no parent"))?;
            fs::create_dir_all(parent).await.map_err(storage)?;
            let mut permissions = upload.file.metadata().await.map_err(storage)?.permissions();
            permissions.set_readonly(true);
            upload
                .file
                .set_permissions(permissions)
                .await
                .map_err(storage)?;
            let published = destination.clone();
            // TempPath publication and cleanup retain ownership until completion.
            let result =
                tokio::task::spawn_blocking(move || upload.path.persist_noclobber(published))
                    .await
                    .map_err(storage)?;
            match result {
                Ok(()) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_file(&verified, &blob, &destination).await?;
                }
                Err(error) => return Err(storage(error.error)),
            }
            verified.lock().map_err(storage)?.insert(blob.clone());
            Ok(blob)
        })
        .await
        .map_err(storage)?
    }

    async fn abort_put(&self, upload_id: &BlobUploadId) -> Result<(), BlobError> {
        let upload = self
            .inner
            .lock()
            .map_err(storage)?
            .uploads
            .remove(upload_id);
        if let Some(upload) = upload {
            tokio::spawn(async move {
                let upload = upload.lock().await.take();
                tokio::task::spawn_blocking(move || drop(upload))
                    .await
                    .map_err(storage)
            })
            .await
            .map_err(storage)??;
        }
        Ok(())
    }

    async fn read(
        &self,
        blob: &BlobRef,
        offset: u64,
        max_bytes: usize,
    ) -> Result<BlobChunk, BlobError> {
        validate_blob_ref(blob)?;
        if offset > blob.size {
            return Err(BlobError::Invalid("offset exceeds blob size".into()));
        }
        let path = self.object_path(&blob.digest);
        verify_file(&self.verified, blob, &path).await?;
        let mut file = File::open(path).await.map_err(storage)?;
        file.seek(SeekFrom::Start(offset)).await.map_err(storage)?;
        let limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .min(blob.size - offset);
        let mut bytes = Vec::with_capacity(usize::try_from(limit).unwrap_or(max_bytes));
        file.take(limit)
            .read_to_end(&mut bytes)
            .await
            .map_err(storage)?;
        let read = u64::try_from(bytes.len()).map_err(storage)?;
        Ok(BlobChunk {
            bytes,
            eof: offset + read == blob.size,
        })
    }
}

impl FileUpload {
    async fn write(&mut self, offset: u64, bytes: &[u8], limit: u64) -> Result<u64, BlobError> {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if offset != self.size {
            if self.previous_write.as_ref().is_some_and(|previous| {
                previous.offset == offset
                    && previous.length == bytes.len()
                    && previous.digest == digest
            }) {
                return Ok(self.size);
            }
            return Err(BlobError::Conflict {
                expected_offset: self.size,
                actual_offset: offset,
            });
        }
        let length = u64::try_from(bytes.len()).map_err(storage)?;
        let next = self
            .size
            .checked_add(length)
            .ok_or_else(|| BlobError::ResourceExhausted("blob size exceeds u64".into()))?;
        if next > limit || self.expected_size.is_some_and(|size| next > size) {
            return Err(BlobError::ResourceExhausted(
                "write exceeds declared or configured blob size".into(),
            ));
        }
        let result = async {
            self.file.write_all(bytes).await?;
            self.file.flush().await
        }
        .await;
        if let Err(error) = result {
            self.file.set_len(self.size).await.map_err(storage)?;
            self.file
                .seek(SeekFrom::Start(self.size))
                .await
                .map_err(storage)?;
            return Err(storage(error));
        }
        self.size = next;
        self.previous_write = Some(PreviousWrite {
            offset,
            length: bytes.len(),
            digest,
        });
        Ok(next)
    }
}

async fn verify_file(
    verified: &Mutex<HashSet<BlobRef>>,
    blob: &BlobRef,
    path: &Path,
) -> Result<(), BlobError> {
    if verified.lock().map_err(storage)?.contains(blob) {
        return Ok(());
    }
    let mut file = File::open(path).await.map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => BlobError::NotFound,
        _ => storage(error),
    })?;
    if file.metadata().await.map_err(storage)?.len() != blob.size
        || hash_file(&mut file).await? != blob.digest
    {
        return Err(BlobError::Corrupt(
            "reference does not match stored bytes".into(),
        ));
    }
    verified.lock().map_err(storage)?.insert(blob.clone());
    Ok(())
}

async fn hash_file(file: &mut File) -> Result<String, BlobError> {
    file.seek(SeekFrom::Start(0)).await.map_err(storage)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(storage)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        }))
}

fn storage(error: impl std::fmt::Display) -> BlobError {
    BlobError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn upload_contention_leaves_executor_responsive() {
        let root = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FileBlobStore::open(root.path(), 1024).await.unwrap());
        let upload = store.begin_put("text/plain", None).await.unwrap();
        let locked = store.upload(&upload).unwrap();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = locked.blocking_lock();
            ready.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
        });
        started.recv().unwrap();
        let start = std::time::Instant::now();
        let (_, latency) = tokio::join!(
            async { store.write(&upload, 0, b"hello").await.unwrap() },
            async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                start.elapsed()
            }
        );
        worker.join().unwrap();
        assert!(
            latency < std::time::Duration::from_millis(100),
            "executor stalled: {latency:?}"
        );
    }

    #[tokio::test]
    async fn blobs_survive_reopen_and_equal_content_deduplicates() {
        let root = tempfile::tempdir().unwrap();
        let first_ref;
        {
            let store = FileBlobStore::open(root.path(), 1024).await.unwrap();
            let upload = store.begin_put("text/plain", Some(5)).await.unwrap();
            assert_eq!(store.write(&upload, 0, b"hello").await.unwrap(), 5);
            assert_eq!(store.write(&upload, 0, b"hello").await.unwrap(), 5);
            first_ref = store.finish_put(&upload).await.unwrap();
        }
        {
            let store = FileBlobStore::open(root.path(), 1024).await.unwrap();
            assert_eq!(store.read(&first_ref, 1, 3).await.unwrap().bytes, b"ell");
            let upload = store.begin_put("text/plain", None).await.unwrap();
            store.write(&upload, 0, b"hello").await.unwrap();
            assert_eq!(store.finish_put(&upload).await.unwrap(), first_ref);
        }
    }

    #[tokio::test]
    async fn reads_detect_content_corruption() {
        let root = tempfile::tempdir().unwrap();
        let store = FileBlobStore::open(root.path(), 1024).await.unwrap();
        let upload = store.begin_put("text/plain", None).await.unwrap();
        store.write(&upload, 0, b"hello").await.unwrap();
        let blob = store.finish_put(&upload).await.unwrap();
        let path = store.object_path(&blob.digest);
        fs::remove_file(&path).await.unwrap();
        fs::write(&path, b"bad").await.unwrap();
        store.verified.lock().unwrap().clear();

        assert!(matches!(
            store.read(&blob, 0, 5).await,
            Err(BlobError::Corrupt(_))
        ));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_write_preserves_replay_and_other_uploads_progress() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FileBlobStore::open(root.path(), 1024).await.unwrap());
        let first = store.begin_put("text/plain", None).await.unwrap();
        let second = store.begin_put("text/plain", None).await.unwrap();
        let upload = store.upload(&first).unwrap();
        let guard = upload.lock().await;
        let writing = store.clone();
        let id = first.clone();
        let writer = tokio::spawn(async move { writing.write(&id, 0, b"hello").await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while Arc::strong_count(&upload) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            store.write(&second, 0, b"other"),
        )
        .await
        .expect("unrelated upload stalled")
        .unwrap();
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        drop(guard);
        assert_eq!(store.write(&first, 0, b"hello").await.unwrap(), 5);
        let blob = store.finish_put(&first).await.unwrap();
        assert_eq!(store.read(&blob, 0, 1024).await.unwrap().bytes, b"hello");
    }
}
