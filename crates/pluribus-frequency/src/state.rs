use std::{
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use chrono::{DateTime, Utc};
use exn::ResultExt;
use futures_lite::Stream;
use loro::{ExportMode, LoroDoc, LoroList, LoroMap, LoroValue, ValueOrContainer};

use crate::protocol::{Message, NodeId};

const SNAPSHOT_FILE: &str = "frequency.snapshot";
const INCREMENTAL_FILE: &str = "frequency.incremental";
const HISTORY_KEY: &str = "history";
const CONFIGURATION_KEY: &str = "configuration";

/// Unique identifier for an [`Entry`], backed by a ULID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EntryId(ulid::Ulid);

impl EntryId {
    /// Generate a fresh, monotonic entry ID.
    #[must_use]
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self(ulid::Ulid::nil())
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A message with distributed CRDT metadata.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub id: EntryId,
    pub origin: NodeId,
    pub timestamp: DateTime<Utc>,
    pub message: Message,
    #[serde(default)]
    pub in_response_to: Option<EntryId>,
}

impl Entry {
    /// Create a new entry with a fresh [`EntryId`] and the current timestamp.
    #[must_use]
    pub fn new(message: Message, origin: NodeId) -> Self {
        Self {
            id: EntryId::new(),
            origin,
            timestamp: chrono::Utc::now(),
            message,
            in_response_to: None,
        }
    }

    /// Set the entry this one responds to.
    #[must_use]
    pub const fn with_response_to(mut self, id: EntryId) -> Self {
        self.in_response_to = Some(id);
        self
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} from {}", self.message, self.origin)
    }
}

/// Errors returned by [`Log`] operations.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join(SNAPSHOT_FILE)
}

fn incremental_path(dir: &Path) -> PathBuf {
    dir.join(INCREMENTAL_FILE)
}

/// Load a [`LoroDoc`] from persisted files in `dir`.
///
/// Reads the snapshot first (if present), then replays any incremental
/// update chunks on top.
fn load_doc(dir: &Path) -> exn::Result<LoroDoc, Error> {
    let doc = LoroDoc::new();

    let snap = snapshot_path(dir);
    if snap.exists() {
        let bytes =
            fs::read(&snap).or_raise(|| Error(format!("read snapshot {}", snap.display())))?;
        doc.import(&bytes)
            .or_raise(|| Error(format!("import snapshot {}", snap.display())))?;
    }

    let inc = incremental_path(dir);
    if inc.exists() {
        let data =
            fs::read(&inc).or_raise(|| Error(format!("read incremental {}", inc.display())))?;
        let mut cursor = io::Cursor::new(&data);
        while cursor.position() < data.len() as u64 {
            let mut len_buf = [0u8; 4];
            cursor
                .read_exact(&mut len_buf)
                .or_raise(|| Error(format!("read chunk length from {}", inc.display())))?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut chunk = vec![0u8; len];
            cursor
                .read_exact(&mut chunk)
                .or_raise(|| Error(format!("read chunk data from {}", inc.display())))?;
            doc.import(&chunk)
                .or_raise(|| Error("import incremental chunk".into()))?;
        }
    }

    Ok(doc)
}

/// Append one length-prefixed update chunk to the incremental file.
fn append_incremental(dir: &Path, update: &[u8]) -> exn::Result<(), Error> {
    let path = incremental_path(dir);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .or_raise(|| Error(format!("open incremental {}", path.display())))?;
    #[allow(clippy::cast_possible_truncation)]
    let len = update.len() as u32;
    file.write_all(&len.to_le_bytes())
        .or_raise(|| Error(format!("write to incremental {}", path.display())))?;
    file.write_all(update)
        .or_raise(|| Error(format!("write to incremental {}", path.display())))?;
    Ok(())
}

/// Commit the current transaction, export a delta since `vv`, persist it,
/// and broadcast a notification.
fn commit_and_persist(inner: &Inner, vv: &loro::VersionVector) -> exn::Result<(), Error> {
    inner.doc.commit();
    let delta = inner
        .doc
        .export(ExportMode::updates(vv))
        .or_raise(|| Error("export CRDT updates after mutation".into()))?;
    if let Some(dir) = &inner.data_dir {
        append_incremental(dir, &delta)?;
    }
    let _ = inner.sender.try_broadcast(());
    Ok(())
}

struct Inner {
    doc: LoroDoc,
    history: LoroList,
    configuration: LoroMap,
    data_dir: Option<PathBuf>,
    sender: async_broadcast::Sender<()>,
    /// Kept alive so the channel is never fully closed while the `Log`
    /// exists. Without this, new receivers would immediately see `None`.
    _keep_alive: async_broadcast::InactiveReceiver<()>,
}

/// CRDT-backed message log with optional persistence.
///
/// Obtained via [`Log::open`] (persistent) or [`Log::ephemeral`] (in-memory).
/// Cloneable — all clones share the same underlying [`LoroDoc`].
///
/// Use [`Log::history`] and [`Log::config`] for container-specific APIs.
#[derive(Clone)]
pub struct State {
    inner: Arc<Mutex<Inner>>,
}

impl State {
    /// Open a persistent log backed by files in `data_dir`.
    pub fn open(data_dir: &Path) -> exn::Result<Self, Error> {
        fs::create_dir_all(data_dir)
            .or_raise(|| Error(format!("create data dir {}", data_dir.display())))?;
        let doc = load_doc(data_dir)?;
        let history = doc.get_list(HISTORY_KEY);
        let configuration = doc.get_map(CONFIGURATION_KEY);
        let (mut sender, receiver) = async_broadcast::broadcast(64);
        sender.set_overflow(true);
        let keep_alive = receiver.deactivate();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                doc,
                history,
                configuration,
                data_dir: Some(data_dir.to_owned()),
                sender,
                _keep_alive: keep_alive,
            })),
        })
    }

    /// Create an ephemeral (in-memory, no persistence) log.
    #[must_use]
    pub fn ephemeral() -> Self {
        let doc = LoroDoc::new();
        let history = doc.get_list(HISTORY_KEY);
        let configuration = doc.get_map(CONFIGURATION_KEY);
        let (mut sender, receiver) = async_broadcast::broadcast(64);
        sender.set_overflow(true);
        let keep_alive = receiver.deactivate();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                doc,
                history,
                configuration,
                data_dir: None,
                sender,
                _keep_alive: keep_alive,
            })),
        }
    }

    /// Access the history (message) container.
    #[must_use]
    pub fn history(&self) -> History {
        History {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Access the configuration container.
    #[must_use]
    pub fn configuration(&self) -> Configuration {
        Configuration {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Disconnect from the log.
    ///
    /// After calling this, `listen` streams obtained before will stop
    /// receiving live updates.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn leave(&self) {
        let inner = self.inner.lock().expect("poisoned");
        inner.sender.close();
    }

    /// Return the current `Loro` version vector.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn oplog_vv(&self) -> loro::VersionVector {
        self.inner.lock().expect("poisoned").doc.oplog_vv()
    }

    /// Export `Loro` updates since the given version vector.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn export_updates(&self, vv: &loro::VersionVector) -> exn::Result<Vec<u8>, Error> {
        let inner = self.inner.lock().expect("poisoned");
        let bytes = inner
            .doc
            .export(ExportMode::updates(vv))
            .or_raise(|| Error("export CRDT updates".into()))?;
        drop(inner);
        tracing::trace!(size = bytes.len(), "exported CRDT updates");
        Ok(bytes)
    }

    /// Import remote `Loro` updates, optionally persist, and notify listeners.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn import_remote(&self, bytes: &[u8]) -> exn::Result<(), Error> {
        let inner = self.inner.lock().expect("poisoned");
        inner
            .doc
            .import(bytes)
            .or_raise(|| Error("import remote CRDT updates".into()))?;
        if let Some(dir) = &inner.data_dir {
            append_incremental(dir, bytes)?;
        }
        let _ = inner.sender.try_broadcast(());
        drop(inner);
        tracing::debug!(size = bytes.len(), "imported remote CRDT updates");
        Ok(())
    }

    /// Get a new broadcast receiver for change notifications.
    ///
    /// Used by the sync layer to detect local changes.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn new_broadcast_receiver(&self) -> async_broadcast::Receiver<()> {
        self.inner.lock().expect("poisoned").sender.new_receiver()
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// Handle to the history (message list) container within a [`Log`].
///
/// Obtained via [`Log::history`].
pub struct History {
    inner: Arc<Mutex<Inner>>,
}

impl History {
    /// Append an entry to the log.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn push(&self, entry: &Entry) -> exn::Result<(), Error> {
        let bytes = rmp_serde::to_vec_named(entry).or_raise(|| Error("serialize entry".into()))?;
        let inner = self.inner.lock().expect("poisoned");
        let vv = inner.doc.oplog_vv();
        inner
            .history
            .push(LoroValue::Binary(bytes.into()))
            .expect("push failed");
        commit_and_persist(&inner, &vv)?;
        drop(inner);
        tracing::debug!(entry = %entry, "history push");
        Ok(())
    }

    /// Read all entries from the log.
    #[must_use]
    pub fn messages(&self) -> Vec<Entry> {
        self.read_from(0)
    }

    /// Stream of all entries: existing entries first, then live as they arrive.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn listen(&self) -> impl Stream<Item = Entry> + Send {
        self.listen_since(0)
    }

    /// Stream entries from `start` onward, then live as they arrive.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn listen_since(&self, start: usize) -> impl Stream<Item = Entry> + Send {
        let inner = self.inner.lock().expect("poisoned");

        let existing = read_from_inner(&inner, start);
        let cursor = inner.history.len();
        let receiver = inner.sender.new_receiver();

        drop(inner);

        Listener {
            inner: Arc::clone(&self.inner),
            receiver,
            existing,
            existing_idx: 0,
            cursor,
        }
    }

    /// Stream of only new entries arriving after this call.
    ///
    /// Unlike [`listen`](Self::listen), this skips all existing entries
    /// and only yields entries pushed after the stream is created.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn listen_live(&self) -> impl Stream<Item = Entry> + Send {
        let inner = self.inner.lock().expect("poisoned");
        let cursor = inner.history.len();
        let receiver = inner.sender.new_receiver();
        drop(inner);

        Listener {
            inner: Arc::clone(&self.inner),
            receiver,
            existing: Vec::new(),
            existing_idx: 0,
            cursor,
        }
    }

    /// Read entries from `start` onward.
    fn read_from(&self, start: usize) -> Vec<Entry> {
        let inner = self.inner.lock().expect("poisoned");
        read_from_inner(&inner, start)
    }
}

/// Read entries from `start` onward, given a borrowed `Inner`.
fn read_from_inner(inner: &Inner, start: usize) -> Vec<Entry> {
    let len = inner.history.len();
    (start..len)
        .filter_map(|i| {
            if let Some(ValueOrContainer::Value(LoroValue::Binary(b))) = inner.history.get(i) {
                rmp_serde::from_slice(&b).ok()
            } else {
                None
            }
        })
        .collect()
}

struct Listener {
    inner: Arc<Mutex<Inner>>,
    receiver: async_broadcast::Receiver<()>,
    existing: Vec<Entry>,
    existing_idx: usize,
    cursor: usize,
}

impl Listener {
    fn read_from(&self, start: usize) -> Vec<Entry> {
        let inner = self.inner.lock().expect("poisoned");
        read_from_inner(&inner, start)
    }
}

impl Stream for Listener {
    type Item = Entry;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Yield buffered existing entries first.
        if this.existing_idx < this.existing.len() {
            let entry = this.existing[this.existing_idx].clone();
            this.existing_idx += 1;
            return Poll::Ready(Some(entry));
        }

        // Poll for broadcast notifications.
        match Pin::new(&mut this.receiver).poll_next(cx) {
            Poll::Ready(Some(())) => {
                // New entries available — read from cursor.
                let new_entries = this.read_from(this.cursor);
                if new_entries.is_empty() {
                    return Poll::Pending;
                }
                this.cursor += new_entries.len();
                // Buffer them and yield the first.
                this.existing = new_entries;
                this.existing_idx = 1;
                let entry = this.existing[0].clone();
                Poll::Ready(Some(entry))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Handle to the configuration (`LoroMap`) container within a [`Log`].
///
/// Obtained via [`Log::config`].
#[derive(Clone)]
pub struct Configuration {
    inner: Arc<Mutex<Inner>>,
}

impl Configuration {
    /// Read a configuration value, deserializing from msgpack.
    ///
    /// Returns `None` if the key is absent or deserialization fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        let inner = self.inner.lock().expect("poisoned");
        match inner.configuration.get(key) {
            Some(ValueOrContainer::Value(LoroValue::Binary(bytes))) => {
                rmp_serde::from_slice(&bytes).ok()
            }
            _ => None,
        }
    }

    /// Set a configuration value, serializing to msgpack.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn set<T: serde::Serialize>(&self, key: &str, value: &T) -> exn::Result<(), Error> {
        let bytes = rmp_serde::to_vec_named(value)
            .or_raise(|| Error(format!("serialize config value for key {key}")))?;
        let inner = self.inner.lock().expect("poisoned");
        let vv = inner.doc.oplog_vv();
        inner
            .configuration
            .insert(key, LoroValue::Binary(bytes.into()))
            .or_raise(|| Error(format!("insert config key {key}")))?;
        commit_and_persist(&inner, &vv)?;
        drop(inner);
        tracing::debug!(key, "config set");
        Ok(())
    }

    /// List all entries whose key starts with `prefix`, deserializing each value.
    ///
    /// Entries that fail to deserialize are silently skipped.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn list_by_prefix<T: serde::de::DeserializeOwned>(&self, prefix: &str) -> Vec<(String, T)> {
        let inner = self.inner.lock().expect("poisoned");
        let mut out = Vec::new();
        inner.configuration.for_each(|key, value| {
            if key.starts_with(prefix) {
                if let ValueOrContainer::Value(LoroValue::Binary(bytes)) = value {
                    if let Ok(v) = rmp_serde::from_slice(&bytes) {
                        out.push((key.to_owned(), v));
                    }
                }
            }
        });
        drop(inner);
        out
    }

    /// Delete a configuration value.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn delete(&self, key: &str) -> exn::Result<(), Error> {
        let inner = self.inner.lock().expect("poisoned");
        let vv = inner.doc.oplog_vv();
        inner
            .configuration
            .delete(key)
            .or_raise(|| Error(format!("delete config key {key}")))?;
        commit_and_persist(&inner, &vv)?;
        drop(inner);
        tracing::debug!(key, "config delete");
        Ok(())
    }
}
