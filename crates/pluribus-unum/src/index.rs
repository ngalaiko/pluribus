use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_lite::{Stream, StreamExt};

use pluribus_frequency::state::{Entry, EntryId, State};

struct Inner {
    entries: HashMap<EntryId, Entry>,
    parents: HashMap<EntryId, EntryId>,
}

/// Centralized index over history entries.
///
/// Wraps [`State`] and tracks `in_response_to` links so that any consumer can
/// walk from a child entry up to the root of its thread. This is used to
/// suppress display of entries that belong to scheduled (background) threads.
#[derive(Clone)]
pub struct Index {
    state: State,
    inner: Arc<Mutex<Inner>>,
}

impl Index {
    #[must_use]
    pub fn new(state: State) -> Self {
        Self {
            state,
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                parents: HashMap::new(),
            })),
        }
    }

    /// Index an entry so its thread ancestry is available via [`root`].
    fn track(&self, entry: &Entry) {
        let mut inner = self.inner.lock().expect("poisoned");
        inner.entries.insert(entry.id, entry.clone());
        if let Some(parent_id) = entry.in_response_to {
            inner.parents.insert(entry.id, parent_id);
        }
        drop(inner);
    }

    /// Find the root entry of the thread containing `id`.
    ///
    /// Walks up `in_response_to` links until it reaches an entry with no
    /// parent.
    #[must_use]
    pub fn root(&self, id: EntryId) -> Option<Entry> {
        let inner = self.inner.lock().expect("poisoned");
        let mut current = id;
        while let Some(&parent) = inner.parents.get(&current) {
            current = parent;
        }
        inner.entries.get(&current).cloned()
    }

    /// Snapshot of all indexed entries, ordered.
    ///
    /// Replaces `state.history().messages()`.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        let entries = self.state.history().messages();
        for entry in &entries {
            self.track(entry);
        }
        entries
    }

    /// Push an entry into the history, indexing it first.
    ///
    /// Replaces `state.history().push()`.
    pub fn push(&self, entry: &Entry) -> exn::Result<(), pluribus_frequency::state::Error> {
        self.track(entry);
        self.state.history().push(entry)
    }

    /// Stream every entry (past + future), indexing each one.
    pub fn listen(&self) -> impl Stream<Item = Entry> + Send {
        let inner = Arc::clone(&self.inner);
        self.state.history().listen().map(move |entry| {
            let mut guard = inner.lock().expect("poisoned");
            guard.entries.insert(entry.id, entry.clone());
            if let Some(pid) = entry.in_response_to {
                guard.parents.insert(entry.id, pid);
            }
            drop(guard);
            entry
        })
    }

    /// Stream only new entries arriving after this call, indexing each one.
    pub fn listen_live(&self) -> impl Stream<Item = Entry> + Send {
        let inner = Arc::clone(&self.inner);
        self.state.history().listen_live().map(move |entry| {
            let mut guard = inner.lock().expect("poisoned");
            guard.entries.insert(entry.id, entry.clone());
            if let Some(pid) = entry.in_response_to {
                guard.parents.insert(entry.id, pid);
            }
            drop(guard);
            entry
        })
    }

    /// Stream entries starting from position `start`, indexing each one.
    pub fn listen_since(&self, start: usize) -> impl Stream<Item = Entry> + Send {
        let inner = Arc::clone(&self.inner);
        self.state.history().listen_since(start).map(move |entry| {
            let mut guard = inner.lock().expect("poisoned");
            guard.entries.insert(entry.id, entry.clone());
            if let Some(pid) = entry.in_response_to {
                guard.parents.insert(entry.id, pid);
            }
            drop(guard);
            entry
        })
    }

    /// Delegate to [`State`] for operations not covered by the index.
    #[must_use]
    pub const fn state(&self) -> &State {
        &self.state
    }
}
