use pluribus_core::AppendError;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::storage;

// Legacy record formats are confined to this schema migration.
const RECORDS: &str = "
    WITH legacy AS (
        SELECT h.*, o.provider AS oauth_provider, o.refresh_token AS raw_refresh_token,
            CAST(o.refresh_token AS TEXT) AS refresh_token,
            CAST(substr(h.header_value, 8) AS TEXT) AS access_token,
            (SELECT group_concat(origin) FROM http_credential_origins WHERE handle=h.handle) AS origin,
            (SELECT count(*) FROM http_credential_extra_headers WHERE handle=h.handle) AS extra_headers
        FROM http_credentials h LEFT JOIN oauth_credentials o USING(handle)
    ), recognized AS (
        SELECT *, CASE
            WHEN origin='https://openrouter.ai' AND oauth_provider IS NULL
                AND lower(header_name)='authorization' AND path_prefix IS NULL
                AND substr(CAST(header_value AS TEXT),1,7)='Bearer '
                AND length(header_value)>7 AND extra_headers=0
                THEN 'dev.pluribus.openrouter'
            WHEN origin='https://api.telegram.org' AND oauth_provider IS NULL
                AND header_name IS NULL AND extra_headers=0
                AND substr(CAST(path_prefix AS TEXT),1,4)='/bot' AND length(path_prefix)>4
                THEN 'dev.pluribus.telegram'
            WHEN origin='https://chatgpt.com' AND oauth_provider='openai-codex'
                AND lower(header_name)='authorization' AND path_prefix IS NULL
                AND substr(CAST(header_value AS TEXT),1,7)='Bearer '
                AND length(access_token)>0 AND length(refresh_token)>0
                THEN 'dev.pluribus.openai-codex'
        END AS package FROM legacy
    )
    SELECT handle, package, CAST(CASE package
        WHEN 'dev.pluribus.openrouter' THEN json_object('api_key', substr(CAST(header_value AS TEXT),8))
        WHEN 'dev.pluribus.telegram' THEN json_object('token', substr(CAST(path_prefix AS TEXT),5))
        WHEN 'dev.pluribus.openai-codex' THEN json_object('access_token', access_token, 'refresh_token', refresh_token)
    END AS BLOB), header_value, path_prefix, raw_refresh_token FROM recognized;
";

pub(super) fn plugin_credentials(connection: &mut Connection) -> Result<(), AppendError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    {
        let mut statement = transaction.prepare(RECORDS).map_err(storage)?;
        let mut rows = statement.query([]).map_err(storage)?;
        while let Some(row) = rows.next().map_err(storage)? {
            for column in 3..=5 {
                if let Some(bytes) = row.get::<_, Option<Vec<u8>>>(column).map_err(storage)?
                    && (std::str::from_utf8(&bytes).is_err() || bytes.contains(&0))
                {
                    return Err(storage(
                        "malformed legacy credential; schema 11 migration cancelled without deleting credentials",
                    ));
                }
            }
            let handle: String = row.get(0).map_err(storage)?;
            let provider: Option<String> = row.get(1).map_err(storage)?;
            let Some(provider) = provider else {
                return Err(storage(
                    "unsupported legacy credential; schema 11 migration cancelled without deleting credentials",
                ));
            };
            let value: Vec<u8> = row.get(2).map_err(storage)?;
            transaction
                .execute(
                    "INSERT INTO plugin_credentials(handle,provider,value) VALUES (?1,?2,?3)
                     ON CONFLICT(handle,provider) DO NOTHING",
                    params![handle, provider, value],
                )
                .map_err(storage)?;
        }
    }
    transaction
        .execute_batch(
            "DROP TABLE oauth_credentials;
             DROP TABLE http_credential_extra_headers;
             DROP TABLE http_credential_components;
             DROP TABLE http_credential_origins;
             DROP TABLE http_credentials;
             DROP TABLE IF EXISTS credential_generations;
             DROP TABLE IF EXISTS credential_lifecycle;
             PRAGMA user_version=11;",
        )
        .map_err(storage)?;
    transaction.commit().map_err(storage)
}
