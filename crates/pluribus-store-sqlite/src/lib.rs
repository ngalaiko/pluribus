mod migration;

use pluribus_core::{
    AppendError, AppendRequest, AuthorityId, BlobRef, CommittedEvent, CursorKey, DeliveryCommit,
    DeliveryError, DeliveryReceipt, DeliveryStore, EventId, EventMetadataSource, EventPayload,
    EventQuery, EventStore, PrincipalKind, PrincipalRef, SecretError, SecretHandle, StateEntry,
    StateError, StateMutation, StateNamespace, StatePage, StateSnapshot, StateStore, StreamId,
    StreamKind,
};
use rusqlite::types::Type;
use rusqlite::types::Value;
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params, params_from_iter,
};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const EVENT_COLUMNS: &str = "
    schema,
    event_id,
    stream_id,
    stream_kind,
    sequence,
    recorded_at_ms,
    observed_at_ms,
    event_type,
    payload_schema,
    payload_kind,
    payload_json,
    blob_digest,
    blob_size,
    blob_media_type,
    actor_kind,
    actor_id,
    authority_id,
    activity_id,
    correlation_id,
    causation_id,
    deduplication_key
";

const EVENT_SCHEMA: &str = "
    CREATE TABLE streams (
        stream_id TEXT PRIMARY KEY,
        stream_kind INTEGER NOT NULL CHECK (stream_kind IN (0, 1)),
        next_sequence INTEGER NOT NULL CHECK (next_sequence > 0)
    ) STRICT;

    CREATE TABLE events (
        schema TEXT NOT NULL,
        event_id TEXT PRIMARY KEY,
        stream_id TEXT NOT NULL REFERENCES streams(stream_id),
        stream_kind INTEGER NOT NULL CHECK (stream_kind IN (0, 1)),
        sequence INTEGER NOT NULL CHECK (sequence > 0),
        recorded_at_ms INTEGER NOT NULL,
        observed_at_ms INTEGER,
        event_type TEXT NOT NULL CHECK (event_type <> ''),
        payload_schema TEXT NOT NULL CHECK (payload_schema <> ''),
        payload_kind INTEGER NOT NULL CHECK (payload_kind IN (0, 1)),
        payload_json BLOB,
        blob_digest TEXT,
        blob_size TEXT,
        blob_media_type TEXT,
        actor_kind INTEGER NOT NULL CHECK (actor_kind BETWEEN 0 AND 4),
        actor_id TEXT NOT NULL,
        authority_id TEXT,
        activity_id TEXT,
        correlation_id TEXT,
        causation_id TEXT,
        deduplication_key TEXT CHECK (
            deduplication_key IS NULL OR deduplication_key <> ''
        ),
        UNIQUE (stream_id, sequence),
        UNIQUE (stream_id, deduplication_key),
        CHECK (
            (payload_kind = 0 AND payload_json IS NOT NULL
                AND blob_digest IS NULL AND blob_size IS NULL AND blob_media_type IS NULL)
            OR
            (payload_kind = 1 AND payload_json IS NULL
                AND blob_digest IS NOT NULL AND blob_size IS NOT NULL
                AND blob_media_type IS NOT NULL)
        )
    ) STRICT;

    CREATE INDEX events_stream_sequence
        ON events(stream_id, sequence);
";

const STATE_SCHEMA: &str = "
    CREATE TABLE component_state_namespaces (
        namespace TEXT PRIMARY KEY,
        revision TEXT NOT NULL
    ) STRICT;

    CREATE TABLE component_state (
        namespace TEXT NOT NULL REFERENCES component_state_namespaces(namespace),
        key TEXT NOT NULL,
        value BLOB NOT NULL,
        PRIMARY KEY (namespace, key)
    ) STRICT;
";

const DELIVERY_SCHEMA: &str = "
    CREATE TABLE delivery_cursors (
        stream_id TEXT NOT NULL,
        namespace TEXT NOT NULL,
        checkpoint TEXT NOT NULL,
        PRIMARY KEY (stream_id, namespace)
    ) STRICT;

    CREATE INDEX events_stream_type_sequence
        ON events(stream_id, event_type, sequence);

    CREATE INDEX events_stream_correlation_sequence
        ON events(stream_id, correlation_id, sequence);

    CREATE INDEX events_stream_activity_sequence
        ON events(stream_id, activity_id, sequence);

    CREATE INDEX events_stream_recorded_sequence
        ON events(stream_id, recorded_at_ms, sequence);
";

const EVENT_SEARCH_SCHEMA: &str = "
    CREATE VIRTUAL TABLE event_search USING fts5(
        event_id UNINDEXED,
        stream_id UNINDEXED,
        event_type,
        payload_text,
        tokenize = 'unicode61'
    );
";

/// Credential tables schema 11 drops. The ladder still creates them so a
/// database from any earlier version migrates through the same steps.
const CREDENTIAL_SCHEMA: &str = "
    CREATE TABLE http_credentials (
        handle TEXT PRIMARY KEY CHECK (handle <> ''),
        header_name TEXT CHECK (header_name IS NULL OR header_name <> ''),
        header_value BLOB CHECK (header_value IS NULL OR length(header_value) > 0),
        path_prefix BLOB,
        CHECK ((header_name IS NULL) = (header_value IS NULL)),
        CHECK (header_name IS NOT NULL OR path_prefix IS NOT NULL)
    ) STRICT;

    CREATE TABLE http_credential_origins (
        handle TEXT NOT NULL REFERENCES http_credentials(handle) ON DELETE CASCADE,
        origin TEXT NOT NULL CHECK (origin <> ''),
        PRIMARY KEY (handle, origin)
    ) STRICT;

    CREATE TABLE http_credential_components (
        handle TEXT NOT NULL REFERENCES http_credentials(handle) ON DELETE CASCADE,
        component_kind INTEGER NOT NULL CHECK (component_kind BETWEEN 0 AND 4),
        component_id TEXT NOT NULL CHECK (component_id <> ''),
        PRIMARY KEY (handle, component_kind, component_id)
    ) STRICT;

    CREATE TABLE http_credential_extra_headers (
        handle TEXT NOT NULL REFERENCES http_credentials(handle) ON DELETE CASCADE,
        position INTEGER NOT NULL CHECK (position > 0),
        header_name TEXT NOT NULL CHECK (header_name <> ''),
        header_value BLOB NOT NULL CHECK (length(header_value) > 0),
        PRIMARY KEY (handle, position)
    ) STRICT;

    CREATE TABLE oauth_credentials (
        handle TEXT PRIMARY KEY REFERENCES http_credentials(handle) ON DELETE CASCADE,
        provider TEXT NOT NULL CHECK (provider <> ''),
        refresh_token BLOB NOT NULL CHECK (length(refresh_token) > 0),
        expires_at_ms INTEGER NOT NULL,
        token_url TEXT NOT NULL CHECK (token_url <> ''),
        client_id TEXT NOT NULL CHECK (client_id <> '')
    ) STRICT;
";

const OAUTH_SCHEMA: &str = "
    CREATE TABLE http_credential_extra_headers (
        handle TEXT NOT NULL REFERENCES http_credentials(handle) ON DELETE CASCADE,
        position INTEGER NOT NULL CHECK (position > 0),
        header_name TEXT NOT NULL CHECK (header_name <> ''),
        header_value BLOB NOT NULL CHECK (length(header_value) > 0),
        PRIMARY KEY (handle, position)
    ) STRICT;

    CREATE TABLE oauth_credentials (
        handle TEXT PRIMARY KEY REFERENCES http_credentials(handle) ON DELETE CASCADE,
        provider TEXT NOT NULL CHECK (provider <> ''),
        refresh_token BLOB NOT NULL CHECK (length(refresh_token) > 0),
        expires_at_ms INTEGER NOT NULL,
        token_url TEXT NOT NULL CHECK (token_url <> ''),
        client_id TEXT NOT NULL CHECK (client_id <> '')
    ) STRICT;
";

const PATH_CREDENTIAL_MIGRATION: &str = "
    PRAGMA foreign_keys=OFF;
    BEGIN IMMEDIATE;
    CREATE TABLE http_credentials_v5 (
        handle TEXT PRIMARY KEY CHECK (handle <> ''),
        header_name TEXT CHECK (header_name IS NULL OR header_name <> ''),
        header_value BLOB CHECK (header_value IS NULL OR length(header_value) > 0),
        path_prefix BLOB,
        CHECK ((header_name IS NULL) = (header_value IS NULL)),
        CHECK (header_name IS NOT NULL OR path_prefix IS NOT NULL)
    ) STRICT;
    INSERT INTO http_credentials_v5 (handle, header_name, header_value)
        SELECT handle, header_name, header_value FROM http_credentials;
    DROP TABLE http_credentials;
    ALTER TABLE http_credentials_v5 RENAME TO http_credentials;
    PRAGMA user_version=5;
    COMMIT;
    PRAGMA foreign_keys=ON;
";

/// Persistent `SQLite` implementation of the authoritative event store.
pub struct SqliteEventStore<M> {
    metadata: Arc<M>,
    connection: async_sqlite::Client,
}

impl<M: Send + Sync + 'static> SqliteEventStore<M> {
    /// Writes a consistent database snapshot to a new file.
    ///
    /// # Errors
    /// Returns an error if the destination exists or `SQLite` cannot copy it.
    pub async fn snapshot_to(&self, destination: &Path) -> Result<(), AppendError> {
        let destination_owned = destination.to_owned();
        let operation = move |connection: &mut Connection| {
            let destination = &destination_owned;

            if destination.exists() {
                return Err(storage("snapshot destination exists"));
            }

            connection
                .execute(
                    "VACUUM main INTO ?1",
                    [destination
                        .to_str()
                        .ok_or_else(|| storage("invalid snapshot path"))?],
                )
                .map_err(storage)?;
            restrict_database_permissions(destination)
        };
        database_call(&self.connection, operation, storage).await
    }

    /// Verifies an existing snapshot without modifying it.
    ///
    /// # Errors
    /// Returns an error for corruption or foreign-key violations.
    pub async fn verify_snapshot(path: &Path) -> Result<(), AppendError> {
        let client = async_sqlite::ClientBuilder::new()
            .path(path)
            .flags(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .open()
            .await
            .map_err(storage)?;
        let operation = move |connection: &mut Connection| {
            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(storage)?;
            if !(9..=11).contains(&version) {
                return Err(storage("unsupported snapshot schema version"));
            }
            for query in [
                "SELECT stream_id,next_sequence FROM streams LIMIT 0",
                "SELECT event_id,payload_json FROM events LIMIT 0",
                "SELECT namespace,key,value FROM component_state LIMIT 0",
                "SELECT stream_id,namespace,checkpoint FROM delivery_cursors LIMIT 0",
                "SELECT handle FROM plugin_credentials LIMIT 0",
            ] {
                connection.prepare(query).map_err(storage)?;
            }
            if version >= 10 {
                connection
                    .prepare("SELECT event_id,stream_id FROM event_search LIMIT 0")
                    .map_err(storage)?;
            }
            let result: String = connection
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(storage)?;
            if result != "ok" {
                return Err(storage(result));
            }
            let mut statement = connection
                .prepare("PRAGMA foreign_key_check")
                .map_err(storage)?;
            if statement
                .query([])
                .map_err(storage)?
                .next()
                .map_err(storage)?
                .is_some()
            {
                return Err(storage("snapshot has foreign-key violations"));
            }
            Ok(())
        };
        database_call(&client, operation, storage).await
    }

    /// Supplies durable payloads for conservative blob-reference discovery.
    ///
    /// # Errors
    /// Returns an error when stored payloads cannot be read.
    pub async fn retention_payloads(&self) -> Result<Vec<Vec<u8>>, AppendError> {
        let operation = move |connection: &mut Connection| {
            let mut statement = connection.prepare("SELECT CASE WHEN payload_kind=0 THEN payload_json ELSE CAST(blob_digest AS BLOB) END FROM events ORDER BY stream_id, sequence").map_err(storage)?;
            let mut payloads = statement
                .query_map([], |row| row.get(0))
                .map_err(storage)?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(storage)?;
            let mut state = connection
                .prepare("SELECT value FROM component_state ORDER BY namespace, key")
                .map_err(storage)?;
            let values = state
                .query_map([], |row| row.get(0))
                .map_err(storage)?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(storage)?;
            payloads.push(values.concat());
            Ok(payloads)
        };
        database_call(&self.connection, operation, storage).await
    }

    /// Opens or creates a file-backed store.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot open, configure, or migrate the database.
    pub async fn open(path: impl AsRef<Path>, metadata: M) -> Result<Self, AppendError> {
        let path = path.as_ref();
        let connection = async_sqlite::ClientBuilder::new()
            .path(path)
            .open()
            .await
            .map_err(storage)?;
        let path = path.to_owned();
        connection
            .conn(move |_| Ok(restrict_database_permissions(&path)))
            .await
            .map_err(storage)??;
        Self::initialize(connection, metadata).await
    }

    /// Opens an isolated in-memory store.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot initialize the schema.
    pub async fn open_in_memory(metadata: M) -> Result<Self, AppendError> {
        let connection = async_sqlite::ClientBuilder::new()
            .open()
            .await
            .map_err(storage)?;
        Self::initialize(connection, metadata).await
    }

    async fn initialize(
        connection: async_sqlite::Client,
        metadata: M,
    ) -> Result<Self, AppendError> {
        let operation = move |connection: &mut Connection| {
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(storage)?;
            connection
                .execute_batch(
                    "PRAGMA foreign_keys=ON;
                 PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;",
                )
                .map_err(storage)?;

            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(storage)?;
            match version {
            0 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE; {EVENT_SCHEMA} {STATE_SCHEMA} {CREDENTIAL_SCHEMA} {DELIVERY_SCHEMA} PRAGMA user_version=6; COMMIT;"
                ))
                .map_err(storage)?,
            1 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE; {STATE_SCHEMA} {CREDENTIAL_SCHEMA} {DELIVERY_SCHEMA} PRAGMA user_version=6; COMMIT;"
                ))
                .map_err(storage)?,
            2 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE; {CREDENTIAL_SCHEMA} {DELIVERY_SCHEMA} PRAGMA user_version=6; COMMIT;"
                ))
                .map_err(storage)?,
            3 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE; {OAUTH_SCHEMA} PRAGMA user_version=4; COMMIT; {PATH_CREDENTIAL_MIGRATION} BEGIN IMMEDIATE; {DELIVERY_SCHEMA} PRAGMA user_version=6; COMMIT;"
                ))
                .map_err(storage)?,
            4 => connection
                .execute_batch(&format!(
                    "{PATH_CREDENTIAL_MIGRATION} BEGIN IMMEDIATE; {DELIVERY_SCHEMA} PRAGMA user_version=6; COMMIT;"
                ))
                .map_err(storage)?,
            5 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE; {DELIVERY_SCHEMA} PRAGMA user_version=6; COMMIT;"
                ))
                .map_err(storage)?,
            6..=11 => {}
            version => {
                return Err(AppendError::Storage(format!(
                    "unsupported SQLite schema version: {version}"
                )));
            }
        }

            if version < 7 {
                connection.execute_batch("BEGIN IMMEDIATE;
                ALTER TABLE oauth_credentials ADD COLUMN refresh_recipe BLOB;
                ALTER TABLE oauth_credentials ADD COLUMN access_token BLOB;
                CREATE TABLE credential_generations (handle TEXT PRIMARY KEY, generation INTEGER NOT NULL, status TEXT NOT NULL, deadline_ms INTEGER);
                INSERT INTO credential_generations SELECT handle, 1, 'usable', NULL FROM http_credentials;
                PRAGMA user_version=7; COMMIT;").map_err(storage)?;
            }
            if version < 8 {
                connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE credential_lifecycle (sequence INTEGER PRIMARY KEY AUTOINCREMENT, handle TEXT NOT NULL, generation INTEGER NOT NULL, outcome TEXT NOT NULL, at_ms INTEGER NOT NULL, deadline_ms INTEGER);
                PRAGMA user_version=8; COMMIT;").map_err(storage)?;
            }
            if version < 9 {
                connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE plugin_credentials (handle TEXT NOT NULL, provider TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(handle,provider));
                PRAGMA user_version=9; COMMIT;").map_err(storage)?;
            }
            if version < 10 {
                connection.execute_batch(&format!("BEGIN IMMEDIATE;
                {EVENT_SEARCH_SCHEMA}
                INSERT INTO event_search (event_id, stream_id, event_type, payload_text)
                    SELECT event_id, stream_id, event_type,
                        CASE WHEN payload_kind = 0
                                  AND event_type NOT IN ('cognition.checkpoint', 'cognition.job-updated')
                             THEN substr(CAST(payload_json AS TEXT), 1, 8192)
                             ELSE '' END
                    FROM events;
                PRAGMA user_version=10; COMMIT;")).map_err(storage)?;
            }
            if version < 11 {
                migration::plugin_credentials(connection)?;
            }
            Ok(())
        };
        database_call(&connection, operation, storage).await?;
        Ok(Self {
            metadata: Arc::new(metadata),
            connection,
        })
    }
}

#[async_trait::async_trait]
impl<M: Send + Sync + 'static> StateStore for SqliteEventStore<M> {
    async fn get(
        &self,
        namespace: &StateNamespace,
        key: &str,
    ) -> Result<StateSnapshot, StateError> {
        let namespace_owned = namespace.to_owned();
        let key_owned = key.to_owned();
        let operation = move |connection: &mut Connection| {
            let namespace = &namespace_owned;
            let key = &key_owned;

            let transaction = connection.transaction().map_err(state_storage)?;
            let revision = state_revision(&transaction, namespace)?;
            let value = transaction
                .query_row(
                    "SELECT value FROM component_state WHERE namespace = ?1 AND key = ?2",
                    params![namespace.as_str(), key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(state_storage)?;
            transaction.commit().map_err(state_storage)?;
            Ok(StateSnapshot { revision, value })
        };
        database_call(&self.connection, operation, state_storage).await
    }

    async fn scan(
        &self,
        namespace: &StateNamespace,
        prefix: &str,
        after_key: Option<&str>,
        limit: usize,
    ) -> Result<StatePage, StateError> {
        let namespace_owned = namespace.to_owned();
        let prefix_owned = prefix.to_owned();
        let after_key_owned = after_key.map(str::to_owned);
        let operation = move |connection: &mut Connection| {
            let namespace = &namespace_owned;
            let prefix = &prefix_owned;
            let after_key = after_key_owned.as_deref();

            let transaction = connection.transaction().map_err(state_storage)?;
            let revision = state_revision(&transaction, namespace)?;
            let sql_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
            let entries = {
                let mut statement = transaction
                    .prepare(
                        "SELECT key, value FROM component_state
                     WHERE namespace = ?1
                       AND substr(key, 1, length(?2)) = ?2
                       AND (?3 IS NULL OR key > ?3)
                     ORDER BY key COLLATE BINARY ASC
                     LIMIT ?4",
                    )
                    .map_err(state_storage)?;
                let rows = statement
                    .query_map(
                        params![namespace.as_str(), prefix, after_key, sql_limit],
                        |row| {
                            Ok(StateEntry {
                                key: row.get(0)?,
                                value: row.get(1)?,
                            })
                        },
                    )
                    .map_err(state_storage)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(state_storage)?
            };
            transaction.commit().map_err(state_storage)?;
            let mut entries = entries;
            let has_more = entries.len() > limit;
            entries.truncate(limit);
            let next_key = has_more
                .then(|| entries.last().map(|entry| entry.key.clone()))
                .flatten();
            Ok(StatePage {
                entries,
                next_key,
                revision,
            })
        };
        database_call(&self.connection, operation, state_storage).await
    }

    async fn apply(
        &self,
        namespace: &StateNamespace,
        expected_revision: u64,
        mutations: &[StateMutation],
    ) -> Result<u64, StateError> {
        let namespace_owned = namespace.to_owned();
        let mutations_owned = mutations.to_owned();
        let operation = move |connection: &mut Connection| {
            let namespace = &namespace_owned;
            let mutations = &mutations_owned;

            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(state_storage)?;
            transaction
                .execute(
                    "INSERT INTO component_state_namespaces (namespace, revision)
                 VALUES (?1, '0') ON CONFLICT (namespace) DO NOTHING",
                    [namespace.as_str()],
                )
                .map_err(state_storage)?;
            let actual_revision = state_revision(&transaction, namespace)?;
            if actual_revision != expected_revision {
                return Err(StateError::Conflict {
                    expected: expected_revision,
                    actual: actual_revision,
                });
            }
            if mutations.is_empty() {
                transaction.commit().map_err(state_storage)?;
                return Ok(actual_revision);
            }
            for mutation in mutations {
                match mutation {
                    StateMutation::Set { key, value } => {
                        transaction
                            .execute(
                                "INSERT INTO component_state (namespace, key, value)
                             VALUES (?1, ?2, ?3)
                             ON CONFLICT (namespace, key) DO UPDATE SET value = excluded.value",
                                params![namespace.as_str(), key, value],
                            )
                            .map_err(state_storage)?;
                    }
                    StateMutation::Delete { key } => {
                        transaction
                            .execute(
                                "DELETE FROM component_state WHERE namespace = ?1 AND key = ?2",
                                params![namespace.as_str(), key],
                            )
                            .map_err(state_storage)?;
                    }
                }
            }
            let revision = actual_revision
                .checked_add(1)
                .ok_or_else(|| StateError::Storage("state revision exhausted".into()))?;
            transaction
                .execute(
                    "UPDATE component_state_namespaces SET revision = ?2 WHERE namespace = ?1",
                    params![namespace.as_str(), revision.to_string()],
                )
                .map_err(state_storage)?;
            transaction.commit().map_err(state_storage)?;
            Ok(revision)
        };
        database_call(&self.connection, operation, state_storage).await
    }
}

fn state_revision(
    transaction: &Transaction<'_>,
    namespace: &StateNamespace,
) -> Result<u64, StateError> {
    let revision = transaction
        .query_row(
            "SELECT revision FROM component_state_namespaces WHERE namespace = ?1",
            [namespace.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(state_storage)?
        .unwrap_or_else(|| "0".into());
    revision
        .parse()
        .map_err(|error| StateError::Storage(format!("invalid state revision: {error}")))
}

impl<M: EventMetadataSource + 'static> SqliteEventStore<M> {
    /// Appends one event inside a caller-owned transaction so that a delivery
    /// can commit several events, its state, and its cursor together.
    fn append_in(
        metadata: &M,
        transaction: &Transaction<'_>,
        request: &AppendRequest,
    ) -> Result<CommittedEvent, AppendError> {
        request.validate()?;

        if let Some(deduplication_key) = &request.deduplication_key {
            let sql = format!(
                "SELECT {EVENT_COLUMNS} FROM events
                 WHERE stream_id = ?1 AND deduplication_key = ?2"
            );
            let existing = transaction
                .query_row(
                    &sql,
                    params![request.stream_id.as_str(), deduplication_key],
                    decode_event,
                )
                .optional()
                .map_err(storage)?;
            if let Some(event) = existing {
                return Ok(event);
            }
        }

        let stream_kind = encode_stream_kind(request.stream_kind);
        transaction
            .execute(
                "INSERT INTO streams (stream_id, stream_kind, next_sequence)
                 VALUES (?1, ?2, 1)
                 ON CONFLICT (stream_id) DO NOTHING",
                params![request.stream_id.as_str(), stream_kind],
            )
            .map_err(storage)?;
        let (stored_kind, sequence): (i64, i64) = transaction
            .query_row(
                "SELECT stream_kind, next_sequence FROM streams WHERE stream_id = ?1",
                [request.stream_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(storage)?;
        if stored_kind != stream_kind {
            return Err(AppendError::InvalidEvent(format!(
                "stream kind changed for {}",
                request.stream_id.as_str()
            )));
        }
        if sequence <= 0 || sequence == i64::MAX {
            return Err(AppendError::Storage("stream sequence exhausted".into()));
        }

        let event_id = metadata.next_event_id();
        let collision: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE event_id = ?1)",
                [event_id.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if collision {
            return Err(AppendError::Storage(format!(
                "event ID collision: {}",
                event_id.as_str()
            )));
        }

        let recorded_at_ms = metadata.now_ms();
        insert_event(transaction, request, &event_id, sequence, recorded_at_ms)?;

        Ok(CommittedEvent {
            schema: CommittedEvent::SCHEMA.into(),
            event_id,
            sequence: u64::try_from(sequence)
                .map_err(|_| AppendError::Storage("negative stream sequence".into()))?,
            recorded_at_ms,
            request: request.clone(),
        })
    }
}

#[async_trait::async_trait]
impl<M: EventMetadataSource + 'static> EventStore for SqliteEventStore<M> {
    async fn append(&self, request: AppendRequest) -> Result<CommittedEvent, AppendError> {
        let metadata = self.metadata.clone();
        let operation = move |connection: &mut Connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let event = Self::append_in(&metadata, &transaction, &request)?;
            transaction.commit().map_err(storage)?;
            Ok(event)
        };
        database_call(&self.connection, operation, storage).await
    }

    async fn read(
        &self,
        stream: &StreamId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, AppendError> {
        let stream_owned = stream.to_owned();
        let operation = move |connection: &mut Connection| {
            let stream = &stream_owned;

            let Ok(after_sequence) = i64::try_from(after_sequence) else {
                return Ok(Vec::new());
            };
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);

            let sql = format!(
                "SELECT {EVENT_COLUMNS} FROM events
             WHERE stream_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC
             LIMIT ?3"
            );
            let mut statement = connection.prepare(&sql).map_err(storage)?;
            let rows = statement
                .query_map(
                    params![stream.as_str(), after_sequence, limit],
                    decode_event,
                )
                .map_err(storage)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
        };
        database_call(&self.connection, operation, storage).await
    }

    async fn get(&self, event_id: &EventId) -> Result<Option<CommittedEvent>, AppendError> {
        let event_id_owned = event_id.to_owned();
        let operation = move |connection: &mut Connection| {
            let event_id = &event_id_owned;

            let sql = format!("SELECT {EVENT_COLUMNS} FROM events WHERE event_id = ?1");
            connection
                .query_row(&sql, [event_id.as_str()], decode_event)
                .optional()
                .map_err(storage)
        };
        database_call(&self.connection, operation, storage).await
    }

    async fn query(
        &self,
        stream: &StreamId,
        query: &EventQuery,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, AppendError> {
        let stream_owned = stream.to_owned();
        let query_owned = query.to_owned();
        let operation = move |connection: &mut Connection| {
            let stream = &stream_owned;
            let query = &query_owned;

            let mut conditions = vec!["stream_id = ?".to_owned()];
            let mut binds = vec![Value::Text(stream.as_str().to_owned())];

            if let Some(after) = query.after_sequence {
                let Ok(after) = i64::try_from(after) else {
                    return Ok(Vec::new());
                };
                conditions.push("sequence > ?".to_owned());
                binds.push(Value::Integer(after));
            }
            if let Some(before) = query.before_sequence {
                let Ok(before) = i64::try_from(before) else {
                    return Ok(Vec::new());
                };
                conditions.push("sequence < ?".to_owned());
                binds.push(Value::Integer(before));
            }
            if !query.event_types.is_empty() {
                let placeholders = vec!["?"; query.event_types.len()].join(", ");
                conditions.push(format!("event_type IN ({placeholders})"));
                for event_type in &query.event_types {
                    binds.push(Value::Text(event_type.clone()));
                }
            }
            if let Some(correlation_id) = &query.correlation_id {
                conditions.push("correlation_id = ?".to_owned());
                binds.push(Value::Text(correlation_id.clone()));
            }
            if let Some(activity_id) = &query.activity_id {
                conditions.push("activity_id = ?".to_owned());
                binds.push(Value::Text(activity_id.clone()));
            }
            if let Some(from) = query.recorded_from_ms {
                conditions.push("recorded_at_ms >= ?".to_owned());
                binds.push(Value::Integer(from));
            }
            if let Some(to) = query.recorded_to_ms {
                conditions.push("recorded_at_ms <= ?".to_owned());
                binds.push(Value::Integer(to));
            }
            if let Some(text) = &query.text_query {
                let Some(text) = literal_search_query(text) else {
                    return Ok(Vec::new());
                };
                conditions.push(
                    "event_id IN (SELECT event_id FROM event_search WHERE event_search MATCH ?)"
                        .to_owned(),
                );
                binds.push(Value::Text(text));
            }
            binds.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));

            let order = if query.descending {
                "sequence DESC"
            } else {
                "sequence ASC"
            };
            let sql = format!(
                "SELECT {EVENT_COLUMNS} FROM events
             WHERE {}
             ORDER BY {order}
             LIMIT ?",
                conditions.join(" AND ")
            );

            let mut statement = connection.prepare(&sql).map_err(storage)?;
            let rows = statement
                .query_map(params_from_iter(binds), decode_event)
                .map_err(storage)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
        };
        database_call(&self.connection, operation, storage).await
    }
}

#[async_trait::async_trait]
impl<M: EventMetadataSource + 'static> DeliveryStore for SqliteEventStore<M> {
    async fn checkpoint(&self, cursor: &CursorKey) -> Result<u64, DeliveryError> {
        let cursor_owned = cursor.to_owned();
        let operation = move |connection: &mut Connection| {
            let cursor = &cursor_owned;

            read_checkpoint(connection, cursor)
        };
        database_call(&self.connection, operation, delivery_storage).await
    }

    async fn commit(&self, commit: DeliveryCommit) -> Result<DeliveryReceipt, DeliveryError> {
        let metadata = self.metadata.clone();
        let operation = move |connection: &mut Connection| {
            commit.validate()?;

            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(delivery_storage)?;

            let stored = read_checkpoint(&transaction, &commit.cursor)?;
            if stored != commit.expected_checkpoint {
                return Err(DeliveryError::Conflict {
                    expected: commit.expected_checkpoint,
                    actual: stored,
                });
            }

            let namespace = &commit.cursor.namespace;
            transaction
                .execute(
                    "INSERT INTO component_state_namespaces (namespace, revision)
                 VALUES (?1, '0') ON CONFLICT (namespace) DO NOTHING",
                    [namespace.as_str()],
                )
                .map_err(delivery_storage)?;
            let mut revision = state_revision(&transaction, namespace)?;
            if !commit.mutations.is_empty() {
                for mutation in &commit.mutations {
                    match mutation {
                        StateMutation::Set { key, value } => {
                            transaction
                                .execute(
                                    "INSERT INTO component_state (namespace, key, value)
                                 VALUES (?1, ?2, ?3)
                                 ON CONFLICT (namespace, key)
                                 DO UPDATE SET value = excluded.value",
                                    params![namespace.as_str(), key, value],
                                )
                                .map_err(delivery_storage)?;
                        }
                        StateMutation::Delete { key } => {
                            transaction
                                .execute(
                                    "DELETE FROM component_state WHERE namespace = ?1 AND key = ?2",
                                    params![namespace.as_str(), key],
                                )
                                .map_err(delivery_storage)?;
                        }
                    }
                }
                revision = revision
                    .checked_add(1)
                    .ok_or_else(|| DeliveryError::Storage("state revision exhausted".into()))?;
                transaction
                    .execute(
                        "UPDATE component_state_namespaces SET revision = ?2 WHERE namespace = ?1",
                        params![namespace.as_str(), revision.to_string()],
                    )
                    .map_err(delivery_storage)?;
            }

            let mut events = Vec::with_capacity(commit.events.len());
            for request in &commit.events {
                events.push(Self::append_in(&metadata, &transaction, request)?);
            }

            let checkpoint = commit.checkpoint.unwrap_or(stored);
            if commit.checkpoint.is_some() {
                transaction
                    .execute(
                        "INSERT INTO delivery_cursors (stream_id, namespace, checkpoint)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (stream_id, namespace)
                     DO UPDATE SET checkpoint = excluded.checkpoint",
                        params![
                            commit.cursor.stream_id.as_str(),
                            namespace.as_str(),
                            checkpoint.to_string()
                        ],
                    )
                    .map_err(delivery_storage)?;
            }

            transaction.commit().map_err(delivery_storage)?;
            Ok(DeliveryReceipt {
                events,
                checkpoint,
                revision,
            })
        };
        database_call(&self.connection, operation, delivery_storage).await
    }

    async fn discard(&self, cursor: &CursorKey) -> Result<(), DeliveryError> {
        let cursor_owned = cursor.to_owned();
        let operation = move |connection: &mut Connection| {
            let cursor = &cursor_owned;

            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(delivery_storage)?;
            transaction
                .execute(
                    "DELETE FROM component_state WHERE namespace = ?1",
                    [cursor.namespace.as_str()],
                )
                .map_err(delivery_storage)?;
            transaction
                .execute(
                    "DELETE FROM component_state_namespaces WHERE namespace = ?1",
                    [cursor.namespace.as_str()],
                )
                .map_err(delivery_storage)?;
            transaction
                .execute(
                    "DELETE FROM delivery_cursors WHERE stream_id = ?1 AND namespace = ?2",
                    params![cursor.stream_id.as_str(), cursor.namespace.as_str()],
                )
                .map_err(delivery_storage)?;
            transaction.commit().map_err(delivery_storage)
        };
        database_call(&self.connection, operation, delivery_storage).await
    }
}

fn read_checkpoint(connection: &Connection, cursor: &CursorKey) -> Result<u64, DeliveryError> {
    let checkpoint = connection
        .query_row(
            "SELECT checkpoint FROM delivery_cursors
             WHERE stream_id = ?1 AND namespace = ?2",
            params![cursor.stream_id.as_str(), cursor.namespace.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(delivery_storage)?
        .unwrap_or_else(|| "0".into());
    checkpoint
        .parse()
        .map_err(|error| DeliveryError::Storage(format!("invalid delivery checkpoint: {error}")))
}

fn insert_event(
    transaction: &Transaction<'_>,
    request: &AppendRequest,
    event_id: &EventId,
    sequence: i64,
    recorded_at_ms: i64,
) -> Result<(), AppendError> {
    let (payload_kind, payload_json, blob_digest, blob_size, blob_media_type) =
        encode_payload(&request.payload);
    transaction
        .execute(
            "INSERT INTO events (
                schema, event_id, stream_id, stream_kind, sequence,
                recorded_at_ms, observed_at_ms, event_type, payload_schema,
                payload_kind, payload_json, blob_digest, blob_size, blob_media_type,
                actor_kind, actor_id, authority_id, activity_id, correlation_id,
                causation_id, deduplication_key
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
             )",
            params![
                CommittedEvent::SCHEMA,
                event_id.as_str(),
                request.stream_id.as_str(),
                encode_stream_kind(request.stream_kind),
                sequence,
                recorded_at_ms,
                request.observed_at_ms,
                request.event_type,
                request.payload_schema,
                payload_kind,
                payload_json,
                blob_digest,
                blob_size,
                blob_media_type,
                encode_principal_kind(request.actor.kind),
                request.actor.id.as_str(),
                request.authority_id.as_ref().map(AuthorityId::as_str),
                request.activity_id,
                request.correlation_id,
                request.causation_id.as_ref().map(EventId::as_str),
                request.deduplication_key,
            ],
        )
        .map_err(storage)?;
    let payload_text = searchable_payload_text(request);
    transaction
        .execute(
            "INSERT INTO event_search (event_id, stream_id, event_type, payload_text)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                event_id.as_str(),
                request.stream_id.as_str(),
                request.event_type,
                payload_text
            ],
        )
        .map_err(storage)?;
    transaction
        .execute(
            "UPDATE streams SET next_sequence = ?2 WHERE stream_id = ?1",
            params![request.stream_id.as_str(), sequence + 1],
        )
        .map_err(storage)?;
    Ok(())
}

fn searchable_payload_text(request: &AppendRequest) -> String {
    if matches!(
        request.event_type.as_str(),
        "cognition.checkpoint" | "cognition.job-updated"
    ) {
        return String::new();
    }
    let EventPayload::CanonicalJson(bytes) = &request.payload else {
        return String::new();
    };
    String::from_utf8_lossy(bytes).chars().take(8192).collect()
}

type EncodedPayload<'a> = (
    i64,
    Option<&'a [u8]>,
    Option<&'a str>,
    Option<String>,
    Option<&'a str>,
);

fn encode_payload(payload: &EventPayload) -> EncodedPayload<'_> {
    match payload {
        EventPayload::CanonicalJson(bytes) => (0, Some(bytes), None, None, None),
        EventPayload::Blob(blob) => (
            1,
            None,
            Some(&blob.digest),
            Some(blob.size.to_string()),
            Some(&blob.media_type),
        ),
    }
}

fn literal_search_query(input: &str) -> Option<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in input.chars() {
        if character.is_alphanumeric() || character == '_' {
            token.push(character);
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    (!tokens.is_empty()).then(|| {
        tokens
            .into_iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ")
    })
}

fn decode_event(row: &Row<'_>) -> rusqlite::Result<CommittedEvent> {
    let stream_kind_value = row.get::<_, i64>(3)?;
    let sequence_value = row.get::<_, i64>(4)?;
    let payload_kind = row.get::<_, i64>(9)?;
    let actor_kind_value = row.get::<_, i64>(14)?;
    let payload = match payload_kind {
        0 => EventPayload::CanonicalJson(required(row.get(10)?, 10, "payload JSON")?),
        1 => {
            let size = required::<String>(row.get(12)?, 12, "blob size")?
                .parse::<u64>()
                .map_err(|error| corrupt(12, Type::Text, error))?;
            EventPayload::Blob(BlobRef {
                algorithm: pluribus_core::SHA256_ALGORITHM.into(),
                digest: required(row.get(11)?, 11, "blob digest")?,
                size,
                media_type: required(row.get(13)?, 13, "blob media type")?,
            })
        }
        value => {
            return Err(corrupt_message(
                9,
                Type::Integer,
                format!("payload kind {value}"),
            ));
        }
    };
    let authority_id = row.get::<_, Option<String>>(16)?.map(AuthorityId::new);
    let causation_id = row.get::<_, Option<String>>(19)?.map(EventId::new);

    Ok(CommittedEvent {
        schema: row.get(0)?,
        event_id: EventId::new(row.get::<_, String>(1)?),
        sequence: u64::try_from(sequence_value)
            .map_err(|error| corrupt(4, Type::Integer, error))?,
        recorded_at_ms: row.get(5)?,
        request: AppendRequest {
            stream_id: StreamId::new(row.get::<_, String>(2)?),
            stream_kind: decode_stream_kind(stream_kind_value)?,
            observed_at_ms: row.get(6)?,
            event_type: row.get(7)?,
            payload_schema: row.get(8)?,
            payload,
            actor: PrincipalRef::new(
                decode_principal_kind(actor_kind_value)?,
                row.get::<_, String>(15)?,
            ),
            authority_id,
            activity_id: row.get(17)?,
            correlation_id: row.get(18)?,
            causation_id,
            deduplication_key: row.get(20)?,
        },
    })
}

fn encode_stream_kind(kind: StreamKind) -> i64 {
    match kind {
        StreamKind::Agent => 0,
        StreamKind::Node => 1,
    }
}

fn decode_stream_kind(value: i64) -> rusqlite::Result<StreamKind> {
    match value {
        0 => Ok(StreamKind::Agent),
        1 => Ok(StreamKind::Node),
        value => Err(corrupt_message(
            3,
            Type::Integer,
            format!("stream kind {value}"),
        )),
    }
}

fn encode_principal_kind(kind: PrincipalKind) -> i64 {
    match kind {
        PrincipalKind::Human => 0,
        PrincipalKind::Agent => 1,
        PrincipalKind::Node => 2,
        PrincipalKind::Component => 3,
        PrincipalKind::External => 4,
    }
}

fn decode_principal_kind(value: i64) -> rusqlite::Result<PrincipalKind> {
    match value {
        0 => Ok(PrincipalKind::Human),
        1 => Ok(PrincipalKind::Agent),
        2 => Ok(PrincipalKind::Node),
        3 => Ok(PrincipalKind::Component),
        4 => Ok(PrincipalKind::External),
        value => Err(corrupt_message(
            14,
            Type::Integer,
            format!("principal kind {value}"),
        )),
    }
}

fn required<T>(value: Option<T>, column: usize, name: &str) -> rusqlite::Result<T> {
    value.ok_or_else(|| corrupt_message(column, Type::Null, format!("missing {name}")))
}

fn corrupt(
    column: usize,
    source_type: Type,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, source_type, Box::new(error))
}

fn corrupt_message(column: usize, source_type: Type, message: String) -> rusqlite::Error {
    corrupt(
        column,
        source_type,
        io::Error::new(io::ErrorKind::InvalidData, message),
    )
}

fn storage(error: impl std::fmt::Display) -> AppendError {
    AppendError::Storage(error.to_string())
}

fn delivery_storage(error: impl std::fmt::Display) -> DeliveryError {
    DeliveryError::Storage(error.to_string())
}

fn state_storage(error: impl std::fmt::Display) -> StateError {
    StateError::Storage(error.to_string())
}

fn secret_storage(error: impl std::fmt::Display) -> SecretError {
    SecretError::Storage(error.to_string())
}

#[cfg(unix)]
fn restrict_database_permissions(path: &Path) -> Result<(), AppendError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut permissions = std::fs::metadata(path).map_err(storage)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions).map_err(storage)
}

#[cfg(not(unix))]
fn restrict_database_permissions(_path: &Path) -> Result<(), AppendError> {
    Ok(())
}

async fn database_call<T: Send + 'static, E: Send + 'static>(
    client: &async_sqlite::Client,
    work: impl FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    map_error: impl FnOnce(async_sqlite::Error) -> E + Send,
) -> Result<T, E> {
    client
        .conn_mut(move |connection| Ok(work(connection)))
        .await
        .map_err(map_error)?
}

#[async_trait::async_trait]
impl<M: EventMetadataSource + 'static> pluribus_core::PluginCredentialStore
    for SqliteEventStore<M>
{
    async fn read_plugin_credential(
        &self,
        handle: &SecretHandle,
        provider: &str,
    ) -> Result<Option<Vec<u8>>, SecretError> {
        let handle = handle.as_str().to_owned();
        let provider = provider.to_owned();
        database_call(
            &self.connection,
            move |db| {
                db.query_row(
                    "SELECT value FROM plugin_credentials WHERE handle=?1 AND provider=?2",
                    params![handle, provider],
                    |r| r.get(0),
                )
                .optional()
                .map_err(secret_storage)
            },
            secret_storage,
        )
        .await
    }
    async fn replace_plugin_credential(
        &self,
        handle: &SecretHandle,
        provider: &str,
        expected: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<bool, SecretError> {
        let handle = handle.as_str().to_owned();
        let provider = provider.to_owned();
        database_call(&self.connection, move |db| {
            let changed = if let Some(previous) = expected {
                db.execute("UPDATE plugin_credentials SET value=?3 WHERE handle=?1 AND provider=?2 AND value=?4", params![handle,provider,value,previous])
            } else {
                db.execute("INSERT OR IGNORE INTO plugin_credentials(handle,provider,value) VALUES (?1,?2,?3)", params![handle,provider,value])
            }.map_err(secret_storage)?;
            Ok(changed == 1)
        }, secret_storage).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pluribus_core::{EventId, EventPayload, PrincipalKind, PrincipalRef, StreamKind};
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct Metadata {
        prefix: &'static str,
        next: AtomicU64,
    }

    impl Metadata {
        fn new(prefix: &'static str, next: u64) -> Self {
            Self {
                prefix,
                next: AtomicU64::new(next),
            }
        }
    }

    impl EventMetadataSource for Metadata {
        fn next_event_id(&self) -> EventId {
            EventId::new(format!(
                "{}-{}",
                self.prefix,
                self.next.fetch_add(1, Ordering::Relaxed)
            ))
        }

        fn now_ms(&self) -> i64 {
            123
        }
    }

    struct RepeatedMetadata;

    impl EventMetadataSource for RepeatedMetadata {
        fn next_event_id(&self) -> EventId {
            EventId::new("same-event")
        }

        fn now_ms(&self) -> i64 {
            123
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_contention_leaves_executor_responsive() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let connection = store.connection.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            connection
                .conn_blocking(move |_| {
                    ready.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(())
                })
                .unwrap();
        });
        started.recv().unwrap();
        let start = std::time::Instant::now();
        let (_, latency) = tokio::join!(
            async { store.read(&StreamId::new("test"), 0, 1).await.unwrap() },
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                start.elapsed()
            }
        );
        worker.join().unwrap();
        eprintln!("database contention heartbeat: {latency:?}");
        assert!(
            latency < Duration::from_millis(100),
            "executor stalled: {latency:?}"
        );
    }

    fn request(stream: &str, deduplication_key: Option<&str>) -> AppendRequest {
        AppendRequest {
            stream_id: StreamId::new(stream),
            stream_kind: StreamKind::Agent,
            observed_at_ms: Some(100),
            event_type: "observation.received".into(),
            payload_schema: "test.observation/1".into(),
            payload: EventPayload::CanonicalJson(br#"{"text":"hello"}"#.to_vec()),
            actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            authority_id: None,
            activity_id: Some("activity".into()),
            correlation_id: Some("correlation".into()),
            causation_id: Some(EventId::new("cause")),
            deduplication_key: deduplication_key.map(str::to_owned),
        }
    }

    fn cursor() -> CursorKey {
        CursorKey {
            stream_id: StreamId::new("personal"),
            namespace: StateNamespace::new("shell-1"),
        }
    }

    fn delivery(
        expected: u64,
        checkpoint: Option<u64>,
        mutations: Vec<StateMutation>,
        events: Vec<AppendRequest>,
    ) -> DeliveryCommit {
        DeliveryCommit {
            cursor: cursor(),
            expected_checkpoint: expected,
            checkpoint,
            mutations,
            events,
        }
    }

    fn set(key: &str, value: &[u8]) -> StateMutation {
        StateMutation::Set {
            key: key.to_owned(),
            value: value.to_vec(),
        }
    }

    #[tokio::test]
    async fn delivery_commits_events_state_and_cursor_together() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();

        let receipt = store
            .commit(delivery(
                0,
                Some(7),
                vec![set("offset", b"42")],
                vec![request("personal", None), request("personal", None)],
            ))
            .await
            .unwrap();

        assert_eq!(receipt.events.len(), 2);
        assert_eq!(
            receipt
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(receipt.checkpoint, 7);
        assert_eq!(store.checkpoint(&cursor()).await.unwrap(), 7);
        assert_eq!(
            StateStore::get(&store, &StateNamespace::new("shell-1"), "offset")
                .await
                .unwrap()
                .value,
            Some(b"42".to_vec())
        );
        assert_eq!(
            store
                .read(&StreamId::new("personal"), 0, 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_stale_checkpoint_commits_nothing() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        store
            .commit(delivery(0, Some(4), vec![set("offset", b"1")], Vec::new()))
            .await
            .unwrap();

        let error = store
            .commit(delivery(
                0,
                Some(9),
                vec![set("offset", b"2")],
                vec![request("personal", None)],
            ))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            DeliveryError::Conflict {
                expected: 0,
                actual: 4
            }
        );
        assert_eq!(store.checkpoint(&cursor()).await.unwrap(), 4);
        assert_eq!(
            StateStore::get(&store, &StateNamespace::new("shell-1"), "offset")
                .await
                .unwrap()
                .value,
            Some(b"1".to_vec()),
            "the rejected mutation must not have been applied"
        );
        assert!(
            store
                .read(&StreamId::new("personal"), 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "the rejected event must not have been appended"
        );
    }

    #[tokio::test]
    async fn a_failed_event_rolls_back_state_and_cursor() {
        let store = SqliteEventStore::open_in_memory(RepeatedMetadata)
            .await
            .unwrap();
        store.append(request("personal", None)).await.unwrap();

        let error = store
            .commit(delivery(
                0,
                Some(5),
                vec![set("offset", b"9")],
                vec![request("personal", None)],
            ))
            .await
            .unwrap_err();

        assert!(matches!(error, DeliveryError::Append(_)), "{error:?}");
        assert_eq!(store.checkpoint(&cursor()).await.unwrap(), 0);
        assert_eq!(
            StateStore::get(&store, &StateNamespace::new("shell-1"), "offset")
                .await
                .unwrap()
                .value,
            None
        );
    }

    #[tokio::test]
    async fn an_absent_checkpoint_leaves_the_cursor_for_redelivery() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        store
            .commit(delivery(0, Some(3), Vec::new(), Vec::new()))
            .await
            .unwrap();

        let receipt = store
            .commit(delivery(3, None, vec![set("k", b"v")], Vec::new()))
            .await
            .unwrap();

        assert_eq!(receipt.checkpoint, 3);
        assert_eq!(store.checkpoint(&cursor()).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn a_repeated_deduplication_key_returns_the_original_event() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let first = store
            .commit(delivery(
                0,
                Some(1),
                Vec::new(),
                vec![request("personal", Some("call-1"))],
            ))
            .await
            .unwrap();

        let second = store
            .commit(delivery(
                1,
                Some(2),
                Vec::new(),
                vec![request("personal", Some("call-1"))],
            ))
            .await
            .unwrap();

        assert_eq!(second.events, first.events);
        assert_eq!(
            store
                .read(&StreamId::new("personal"), 0, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn discard_clears_the_namespace_and_its_cursor() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        store
            .commit(delivery(0, Some(6), vec![set("k", b"v")], Vec::new()))
            .await
            .unwrap();

        store.discard(&cursor()).await.unwrap();

        assert_eq!(store.checkpoint(&cursor()).await.unwrap(), 0);
        assert_eq!(
            StateStore::get(&store, &StateNamespace::new("shell-1"), "k")
                .await
                .unwrap()
                .value,
            None
        );
    }

    #[tokio::test]
    async fn query_filters_by_type_correlation_and_time() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let mut wanted = request("personal", None);
        wanted.event_type = "capability.completed".into();
        wanted.correlation_id = Some("wanted".into());
        store.append(request("personal", None)).await.unwrap();
        store.append(wanted).await.unwrap();
        store.append(request("personal", None)).await.unwrap();

        let by_type = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    event_types: vec!["capability.completed".into()],
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();
        let by_correlation = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    correlation_id: Some("wanted".into()),
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();
        let out_of_range = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    recorded_from_ms: Some(200),
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();
        let after = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    after_sequence: Some(2),
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();

        assert_eq!(by_type.len(), 1);
        assert_eq!(by_type[0].sequence, 2);
        assert_eq!(by_correlation.len(), 1);
        assert!(out_of_range.is_empty());
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].sequence, 3);
    }

    #[tokio::test]
    async fn query_searches_payload_text_and_pages_newest_first() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let mut first = request("personal", None);
        first.payload = EventPayload::CanonicalJson(br#"{"message":"older match"}"#.to_vec());
        store.append(first).await.unwrap();
        let mut second = request("personal", None);
        second.payload = EventPayload::CanonicalJson(br#"{"message":"newer match"}"#.to_vec());
        store.append(second).await.unwrap();
        let mut unrelated = request("personal", None);
        unrelated.payload = EventPayload::CanonicalJson(br#"{"message":"other"}"#.to_vec());
        store.append(unrelated).await.unwrap();

        let newest = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    text_query: Some("match".into()),
                    descending: true,
                    ..EventQuery::default()
                },
                1,
            )
            .await
            .unwrap();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].sequence, 2);

        let older = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    text_query: Some("match".into()),
                    descending: true,
                    before_sequence: Some(newest[0].sequence),
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(older.iter().map(|e| e.sequence).collect::<Vec<_>>(), [1]);
    }

    #[tokio::test]
    async fn query_search_treats_fts_syntax_as_literal_text() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let mut event = request("personal", None);
        event.payload = EventPayload::CanonicalJson(br#"{"message":"a+b"}"#.to_vec());
        store.append(event).await.unwrap();
        let result = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    text_query: Some("a+b OR NOT (broken)".into()),
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn schema_nine_search_migration_matches_new_event_projection() {
        let path = temporary_database_path("history-search-migration");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "{EVENT_SCHEMA} {STATE_SCHEMA} {CREDENTIAL_SCHEMA} {DELIVERY_SCHEMA}
                 INSERT INTO streams VALUES ('personal', 0, 2);
                 INSERT INTO events VALUES
                 ('pluribus.event/1', 'old-event', 'personal', 0, 1, 123, NULL,
                  'observation.received', 'test/1', 0,
                  X'7B226D657373616765223A2276C3A46C6B6F6D6D656E227D', NULL, NULL, NULL,
                  0, 'telegram', NULL, NULL, NULL, NULL, NULL);
                 PRAGMA user_version=9;"
            ))
            .unwrap();
        drop(connection);

        let store = SqliteEventStore::open(&path, Metadata::new("event", 2))
            .await
            .unwrap();
        let mut fresh = request("personal", None);
        fresh.payload =
            EventPayload::CanonicalJson(r#"{"message":"välkommen"}"#.as_bytes().to_vec());
        store.append(fresh).await.unwrap();
        let matches = store
            .query(
                &StreamId::new("personal"),
                &EventQuery {
                    text_query: Some("välkommen".into()),
                    ..EventQuery::default()
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            matches
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        drop(store);
        remove_database(&path);
    }

    #[tokio::test]
    async fn query_uses_an_index_for_every_filter() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        store.connection.conn(|connection| {

        for (label, sql) in [
            (
                "type",
                "SELECT 1 FROM events WHERE stream_id = 'a' AND event_type = 'b' ORDER BY sequence",
            ),
            (
                "correlation",
                "SELECT 1 FROM events WHERE stream_id = 'a' AND correlation_id = 'b' ORDER BY sequence",
            ),
            (
                "activity",
                "SELECT 1 FROM events WHERE stream_id = 'a' AND activity_id = 'b' ORDER BY sequence",
            ),
            (
                "recorded",
                "SELECT 1 FROM events WHERE stream_id = 'a' AND recorded_at_ms >= 1 ORDER BY sequence",
            ),
        ] {
            let plan: String = connection
                .query_row(&format!("EXPLAIN QUERY PLAN {sql}"), [], |row| row.get(3))
                .unwrap();
            assert!(
                plan.starts_with("SEARCH") && plan.contains("INDEX"),
                "{label} filter must be an indexed search, got: {plan}"
            );
        }
        Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn state_apply_is_atomic_and_revisioned() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let namespace = StateNamespace::new("plugin-1");

        let revision = store
            .apply(&namespace, 0, &[set("a", b"one")])
            .await
            .unwrap();
        let conflict = store.apply(&namespace, 0, &[]).await.unwrap_err();

        assert_eq!(revision, 1);
        assert_eq!(
            StateStore::get(&store, &namespace, "a")
                .await
                .unwrap()
                .value,
            Some(b"one".to_vec())
        );
        assert_eq!(
            conflict,
            StateError::Conflict {
                expected: 0,
                actual: 1
            }
        );
    }

    #[tokio::test]
    async fn state_scan_is_ordered_and_paginated() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let namespace = StateNamespace::new("plugin-1");
        store
            .apply(
                &namespace,
                0,
                &[set("item/a", &[1]), set("item/b", &[2]), set("other", &[3])],
            )
            .await
            .unwrap();

        let first = store.scan(&namespace, "item/", None, 1).await.unwrap();
        let second = store
            .scan(&namespace, "item/", first.next_key.as_deref(), 1)
            .await
            .unwrap();

        assert_eq!(first.entries[0].key, "item/a");
        assert_eq!(first.next_key.as_deref(), Some("item/a"));
        assert_eq!(second.entries[0].key, "item/b");
        assert_eq!(second.next_key, None);
    }

    #[tokio::test]
    async fn events_survive_reopen() {
        let path = temporary_database_path("reopen");
        let first;
        {
            let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .unwrap();
            first = store
                .append(request("personal", Some("telegram:42")))
                .await
                .unwrap();
        }
        {
            let store = SqliteEventStore::open(&path, Metadata::new("event", 2))
                .await
                .unwrap();
            let events = store.read(&StreamId::new("personal"), 0, 10).await.unwrap();
            assert_eq!(events, [first.clone()][..]);

            let duplicate = store
                .append(request("personal", Some("telegram:42")))
                .await
                .unwrap();
            assert_eq!(duplicate, first);

            let second = store.append(request("personal", None)).await.unwrap();
            assert_eq!(second.sequence, 2);
        }
        remove_database(&path);
    }

    #[tokio::test]
    async fn streams_have_independent_gapless_sequences() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();

        assert_eq!(
            store
                .append(request("personal", None))
                .await
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(
            store
                .append(request("personal", None))
                .await
                .unwrap()
                .sequence,
            2
        );
        assert_eq!(
            store
                .append(request("family", None))
                .await
                .unwrap()
                .sequence,
            1
        );
    }

    #[tokio::test]
    async fn concurrent_connections_do_not_reuse_sequences() {
        let path = temporary_database_path("concurrent");
        let left = Arc::new(
            SqliteEventStore::open(&path, Metadata::new("left", 1))
                .await
                .unwrap(),
        );
        let right = Arc::new(
            SqliteEventStore::open(&path, Metadata::new("right", 1))
                .await
                .unwrap(),
        );
        let writers = (0..16)
            .map(|index| {
                let store = if index % 2 == 0 {
                    Arc::clone(&left)
                } else {
                    Arc::clone(&right)
                };
                tokio::spawn(async move { store.append(request("personal", None)).await.unwrap() })
            })
            .collect::<Vec<_>>();

        for writer in writers {
            writer.await.unwrap();
        }

        let events = left.read(&StreamId::new("personal"), 0, 32).await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=16).collect::<Vec<_>>()
        );
        drop(left);
        drop(right);
        remove_database(&path);
    }

    #[tokio::test]
    async fn id_collision_rolls_back_stream_creation() {
        let store = SqliteEventStore::open_in_memory(RepeatedMetadata)
            .await
            .unwrap();

        store.append(request("personal", None)).await.unwrap();
        let error = store.append(request("family", None)).await.unwrap_err();

        assert_eq!(
            error,
            AppendError::Storage("event ID collision: same-event".into())
        );
        assert!(
            store
                .read(&StreamId::new("family"), 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pagination_starts_after_sequence() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        for _ in 0..4 {
            store.append(request("personal", None)).await.unwrap();
        }

        let events = store.read(&StreamId::new("personal"), 1, 2).await.unwrap();

        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[tokio::test]
    async fn typed_fields_and_blob_references_round_trip() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let mut request = request("mac", None);
        request.stream_kind = StreamKind::Node;
        request.payload = EventPayload::Blob(BlobRef {
            algorithm: pluribus_core::SHA256_ALGORITHM.into(),
            digest: "a".repeat(64),
            size: u64::MAX,
            media_type: "application/octet-stream".into(),
        });
        request.actor = PrincipalRef::new(PrincipalKind::External, "telegram:bot:42");
        request.authority_id = Some(AuthorityId::new("authority"));

        let committed = store.append(request.clone()).await.unwrap();
        let read = store.read(&StreamId::new("mac"), 0, 1).await.unwrap();

        assert_eq!(committed.request, request);
        assert_eq!(read, [committed]);
    }

    #[tokio::test]
    async fn plugin_secrets_survive_reopen_and_compare_atomically() {
        use pluribus_core::PluginCredentialStore;
        let path = temporary_database_path("plugin-secrets");
        let handle = SecretHandle::new("app");
        {
            let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .unwrap();
            assert!(
                store
                    .replace_plugin_credential(&handle, "plugin", None, b"private".to_vec())
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .replace_plugin_credential(&handle, "plugin", None, b"overwrite".to_vec())
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .replace_plugin_credential(
                        &handle,
                        "plugin",
                        Some(b"wrong".to_vec()),
                        b"overwrite".to_vec()
                    )
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .read_plugin_credential(&handle, "other")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        {
            let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .unwrap();
            assert_eq!(
                store
                    .read_plugin_credential(&handle, "plugin")
                    .await
                    .unwrap(),
                Some(b"private".to_vec())
            );
            assert!(
                store
                    .replace_plugin_credential(
                        &handle,
                        "plugin",
                        Some(b"private".to_vec()),
                        b"updated".to_vec()
                    )
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .retention_payloads()
                    .await
                    .unwrap()
                    .iter()
                    .any(|v| v.windows(7).any(|b| b == b"updated"))
            );
        }
        remove_database(&path);
    }

    #[tokio::test]
    async fn file_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = temporary_database_path("permissions");
        let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
            .await
            .unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(store);
        remove_database(&path);
    }

    /// Legacy tables disappear; events and plugin records remain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_eleven_drops_host_injected_credentials() {
        use pluribus_core::PluginCredentialStore;
        let path = temporary_database_path("credential-migration");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "{EVENT_SCHEMA} {STATE_SCHEMA} PRAGMA user_version=2;"
            ))
            .unwrap();
        drop(connection);

        let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
            .await
            .unwrap();
        let handle = SecretHandle::new("telegram");
        store
            .replace_plugin_credential(&handle, "dev.pluribus.telegram", None, b"token".to_vec())
            .await
            .unwrap();
        drop(store);

        let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
            .await
            .unwrap();
        assert_eq!(
            store
                .read_plugin_credential(&handle, "dev.pluribus.telegram")
                .await
                .unwrap(),
            Some(b"token".to_vec())
        );
        let tables = store
            .connection
            .conn(|connection| {
                let mut statement = connection
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%credential%'")?;
                let names = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(names)
            })
            .await
            .unwrap();
        assert_eq!(tables, ["plugin_credentials"]);
        drop(store);
        remove_database(&path);
    }

    async fn legacy_credential_database(label: &str) -> std::path::PathBuf {
        let path = temporary_database_path(label);
        drop(
            SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .unwrap(),
        );
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "{CREDENTIAL_SCHEMA}
             ALTER TABLE oauth_credentials ADD COLUMN refresh_recipe BLOB;
             ALTER TABLE oauth_credentials ADD COLUMN access_token BLOB;
             PRAGMA user_version=10;"
            ))
            .unwrap();
        path
    }

    #[tokio::test]
    async fn legacy_provider_secrets_survive_schema_eleven() {
        use pluribus_core::PluginCredentialStore;
        let path = legacy_credential_database("legacy-provider-secrets").await;
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("
            INSERT INTO http_credentials VALUES ('router', 'authorization', CAST('Bearer router-key' AS BLOB), NULL);
            INSERT INTO http_credentials VALUES ('telegram', NULL, NULL, CAST('/bottelegram-token' AS BLOB));
            INSERT INTO http_credentials VALUES ('codex', 'authorization', CAST('Bearer access' AS BLOB), NULL);
            INSERT INTO http_credential_origins VALUES ('router', 'https://openrouter.ai');
            INSERT INTO http_credential_origins VALUES ('telegram', 'https://api.telegram.org');
            INSERT INTO http_credential_origins VALUES ('codex', 'https://chatgpt.com');
            INSERT INTO oauth_credentials VALUES ('codex', 'openai-codex', CAST('refresh' AS BLOB), 1, 'https://auth.openai.com/oauth/token', 'client', NULL, CAST('access' AS BLOB));
        ").unwrap();
        drop(connection);
        let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
            .await
            .unwrap();
        for (handle, provider, expected) in [
            (
                "router",
                "dev.pluribus.openrouter",
                r#"{"api_key":"router-key"}"#,
            ),
            (
                "telegram",
                "dev.pluribus.telegram",
                r#"{"token":"telegram-token"}"#,
            ),
            (
                "codex",
                "dev.pluribus.openai-codex",
                r#"{"access_token":"access","refresh_token":"refresh"}"#,
            ),
        ] {
            assert_eq!(
                store
                    .read_plugin_credential(&SecretHandle::new(handle), provider)
                    .await
                    .unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        drop(store);
        remove_database(&path);
    }

    #[tokio::test]
    async fn legacy_migration_preserves_newer_plugin_credentials() {
        use pluribus_core::PluginCredentialStore;
        let path = legacy_credential_database("legacy-existing-record").await;
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("
            INSERT INTO http_credentials VALUES ('router', 'authorization', CAST('Bearer old-key' AS BLOB), NULL);
            INSERT INTO http_credential_origins VALUES ('router', 'https://openrouter.ai');
            INSERT INTO plugin_credentials VALUES ('router', 'dev.pluribus.openrouter', CAST('{\"api_key\":\"new-key\"}' AS BLOB));
        ").unwrap();
        drop(connection);
        let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
            .await
            .unwrap();
        assert_eq!(
            store
                .read_plugin_credential(&SecretHandle::new("router"), "dev.pluribus.openrouter")
                .await
                .unwrap(),
            Some(br#"{"api_key":"new-key"}"#.to_vec())
        );
        drop(store);
        remove_database(&path);
    }

    #[tokio::test]
    async fn malformed_legacy_secret_bytes_are_not_discarded() {
        for secret in [b"Bearer \xff".as_slice(), b"Bearer key\0tail".as_slice()] {
            let path = legacy_credential_database("malformed-legacy-secret").await;
            let connection = Connection::open(&path).unwrap();
            connection
                .execute(
                    "INSERT INTO http_credentials VALUES ('router', 'authorization', ?1, NULL)",
                    params![secret],
                )
                .unwrap();
            connection.execute("INSERT INTO http_credential_origins VALUES ('router', 'https://openrouter.ai')", []).unwrap();
            drop(connection);
            assert!(
                SqliteEventStore::open(&path, Metadata::new("event", 1))
                    .await
                    .is_err()
            );
            let connection = Connection::open(&path).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT header_value FROM http_credentials WHERE handle='router'",
                        [],
                        |row| row.get::<_, Vec<u8>>(0)
                    )
                    .unwrap(),
                secret
            );
            drop(connection);
            remove_database(&path);
        }
    }

    #[tokio::test]
    async fn unsupported_legacy_credentials_prevent_destructive_migration() {
        let path = legacy_credential_database("unknown-legacy-secret").await;
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("
            INSERT INTO http_credentials VALUES ('router', 'authorization', CAST('Bearer router-key' AS BLOB), NULL);
            INSERT INTO http_credential_origins VALUES ('router', 'https://openrouter.ai');
            INSERT INTO http_credentials VALUES ('custom', 'authorization', CAST('private-key' AS BLOB), NULL);
            INSERT INTO http_credential_origins VALUES ('custom', 'https://custom.example');
        ").unwrap();
        drop(connection);
        assert!(
            SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .is_err()
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            10
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM plugin_credentials", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT header_value FROM http_credentials WHERE handle='custom'",
                    [],
                    |row| row.get::<_, Vec<u8>>(0)
                )
                .unwrap(),
            b"private-key"
        );
        drop(connection);
        remove_database(&path);
    }

    #[tokio::test]
    async fn a_schema_three_database_migrates_to_the_current_version() {
        let path = temporary_database_path("schema-three-migration");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "{EVENT_SCHEMA} {STATE_SCHEMA}
                 CREATE TABLE http_credentials (
                    handle TEXT PRIMARY KEY CHECK (handle <> ''),
                    header_name TEXT NOT NULL CHECK (header_name <> ''),
                    header_value BLOB NOT NULL CHECK (length(header_value) > 0)
                 ) STRICT;
                 CREATE TABLE http_credential_origins (
                    handle TEXT NOT NULL REFERENCES http_credentials(handle) ON DELETE CASCADE,
                    origin TEXT NOT NULL CHECK (origin <> ''),
                    PRIMARY KEY (handle, origin)
                 ) STRICT;
                 CREATE TABLE http_credential_components (
                    handle TEXT NOT NULL REFERENCES http_credentials(handle) ON DELETE CASCADE,
                    component_kind INTEGER NOT NULL CHECK (component_kind BETWEEN 0 AND 4),
                    component_id TEXT NOT NULL CHECK (component_id <> ''),
                    PRIMARY KEY (handle, component_kind, component_id)
                 ) STRICT;
                 PRAGMA user_version=3;"
            ))
            .unwrap();
        drop(connection);

        let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
            .await
            .unwrap();
        assert_eq!(
            store
                .connection
                .conn(|connection| connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0)))
                .await
                .unwrap(),
            11
        );
        drop(store);
        remove_database(&path);
    }

    #[tokio::test]
    async fn unknown_schema_version_is_rejected() {
        let path = temporary_database_path("future-schema");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 12).unwrap();
        drop(connection);

        let result = SqliteEventStore::open(&path, Metadata::new("event", 1)).await;

        assert!(matches!(
            result,
            Err(AppendError::Storage(message))
                if message == "unsupported SQLite schema version: 12"
        ));
        remove_database(&path);
    }

    #[tokio::test]
    async fn component_state_survives_reopen() {
        let path = temporary_database_path("component-state");
        let namespace = StateNamespace::new("echo-1");
        {
            let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .unwrap();
            let revision = store
                .apply(
                    &namespace,
                    0,
                    &[StateMutation::Set {
                        key: "counter".into(),
                        value: 7_u64.to_le_bytes().to_vec(),
                    }],
                )
                .await
                .unwrap();
            assert_eq!(revision, 1);
        }
        {
            let store = SqliteEventStore::open(&path, Metadata::new("event", 1))
                .await
                .unwrap();
            let snapshot = StateStore::get(&store, &namespace, "counter")
                .await
                .unwrap();
            assert_eq!(snapshot.revision, 1);
            assert_eq!(snapshot.value, Some(7_u64.to_le_bytes().to_vec()));
        }
        remove_database(&path);
    }

    #[tokio::test]
    async fn component_state_is_namespaced_and_conflict_safe() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let left = StateNamespace::new("left");
        let right = StateNamespace::new("right");
        store
            .apply(
                &left,
                0,
                &[StateMutation::Set {
                    key: "value".into(),
                    value: b"left".to_vec(),
                }],
            )
            .await
            .unwrap();

        let error = store
            .apply(
                &left,
                0,
                &[StateMutation::Set {
                    key: "value".into(),
                    value: b"wrong".to_vec(),
                }],
            )
            .await
            .unwrap_err();

        assert!(matches!(error, StateError::Conflict { actual: 1, .. }));
        assert_eq!(
            StateStore::get(&store, &left, "value").await.unwrap().value,
            Some(b"left".to_vec())
        );
        assert_eq!(
            StateStore::get(&store, &right, "value")
                .await
                .unwrap()
                .value,
            None
        );
    }

    #[tokio::test]
    async fn component_state_scan_uses_stable_cursors() {
        let store = SqliteEventStore::open_in_memory(Metadata::new("event", 1))
            .await
            .unwrap();
        let namespace = StateNamespace::new("echo-1");
        store
            .apply(
                &namespace,
                0,
                &[
                    StateMutation::Set {
                        key: "item/a".into(),
                        value: vec![1],
                    },
                    StateMutation::Set {
                        key: "item/b".into(),
                        value: vec![2],
                    },
                ],
            )
            .await
            .unwrap();

        let first = store.scan(&namespace, "item/", None, 1).await.unwrap();
        let second = store
            .scan(&namespace, "item/", first.next_key.as_deref(), 1)
            .await
            .unwrap();
        let empty = store.scan(&namespace, "item/", None, 0).await.unwrap();

        assert_eq!(first.entries[0].key, "item/a");
        assert_eq!(first.next_key.as_deref(), Some("item/a"));
        assert_eq!(second.entries[0].key, "item/b");
        assert!(empty.entries.is_empty());
        assert_eq!(empty.next_key, None);
    }

    fn temporary_database_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pluribus-{label}-{}-{}.sqlite3",
            std::process::id(),
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn snapshot_verification_rejects_unrelated_database() {
        let path = temporary_database_path("unrelated-snapshot");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE unrelated (value TEXT); PRAGMA user_version=8;")
            .unwrap();
        drop(connection);
        assert!(
            SqliteEventStore::<()>::verify_snapshot(&path)
                .await
                .is_err()
        );
        remove_database(&path);
    }

    fn remove_database(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
        let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
    }
}
