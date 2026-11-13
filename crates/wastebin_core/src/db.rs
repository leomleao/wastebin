use std::io::Cursor;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use chacha20poly1305::XNonce;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use rusqlite_migration::{HookError, M, Migrations};
use tokio::sync::oneshot;

use crate::crypto::{self, Password};
use crate::expiration::Expiration;
use crate::id::Id;
use read::{DatabaseEntry, ListEntry, Metadata, PadSummary};

/// Database related errors.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("not allowed to delete")]
    Delete,
    #[error("sqlite error: {0}")]
    Sqlite(rusqlite::Error),
    #[error("migrations error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("failed to compress: {0}")]
    Compression(String),
    #[error("password not given")]
    NoPassword,
    #[error("entry not found")]
    NotFound,
    #[error("join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::Error),
    #[error("failed to send command to channel")]
    SendError,
    #[error("failed to send result")]
    ResultSendError,
    #[error("failed to send result: {0}")]
    ResultRecvError(#[from] oneshot::error::RecvError),
}

/// The programmatic database interface. However, database calls are not translated directly to
/// sqlite but moved forward to a [`Handler`] which reads database commands from a queue and
/// processes them. This is done to avoid locking the underlying non-Send database per call which
/// cuts down performance in half on certain systems.
#[derive(Clone)]
pub struct Database {
    /// Sender for database commands.
    sender: kanal::AsyncSender<Command>,
}

/// Actual database handler that owns the connection to the underlying sqlite database.
struct Handler {
    conn: Connection,
    /// Receiver for database commands.
    receiver: kanal::Receiver<Command>,
    /// Strictly increasing write sequence for deterministic `updated_at` ordering.
    write_seq: u64,
}

/// Commands issued to the database handler and corresponding to [`Database`] calls.
enum Command {
    Insert {
        entry: write::DatabaseEntry,
        result: oneshot::Sender<Result<(Id, write::Entry), Error>>,
    },
    Get {
        id: Id,
        result: oneshot::Sender<Result<DatabaseEntry, Error>>,
    },
    GetMetadata {
        id: Id,
        result: oneshot::Sender<Result<Metadata, Error>>,
    },
    Delete {
        id: Id,
        result: oneshot::Sender<Result<bool, Error>>,
    },
    DeleteMany {
        ids: Vec<Id>,
        result: oneshot::Sender<Result<usize, Error>>,
    },
    DeleteFor {
        id: Id,
        uids: Vec<i64>,
        result: oneshot::Sender<Result<(), Error>>,
    },
    NextUid {
        result: oneshot::Sender<Result<i64, Error>>,
    },
    List {
        result: oneshot::Sender<Result<Vec<ListEntry>, Error>>,
    },
    Purge {
        result: oneshot::Sender<Result<Vec<Id>, Error>>,
    },
    UpdatePad {
        id: Id,
        update: write::PadUpdate,
        result: oneshot::Sender<Result<Metadata, Error>>,
    },
    DeletePad {
        id: Id,
        result: oneshot::Sender<Result<bool, Error>>,
    },
    GetLatestPadId {
        result: oneshot::Sender<Result<Option<Id>, Error>>,
    },
    ListRecentPads {
        limit: usize,
        result: oneshot::Sender<Result<Vec<PadSummary>, Error>>,
    },
}

/// Database opening modes
#[derive(Debug)]
pub enum Open {
    /// Open in-memory database that is wiped after reload
    Memory,
    /// Open database from given path
    Path(PathBuf),
}

/// Module with types for insertion.
pub mod write {
    use crate::crypto::{Encrypted, Password, Plaintext};
    use crate::db::Error;
    use async_compression::tokio::bufread::ZstdEncoder;
    use chacha20poly1305::XNonce;
    use serde::{Deserialize, Serialize};
    use std::io::Cursor;
    use std::num::NonZeroU32;
    use tokio::io::AsyncReadExt;

    /// An uncompressed entry to be inserted into the database.
    #[derive(Default, Debug, Serialize, Deserialize)]
    pub struct Entry {
        /// Content
        pub text: String,
        /// File extension
        pub extension: Option<String>,
        /// Expiration in seconds from now
        pub expires: Option<NonZeroU32>,
        /// Delete if read
        pub burn_after_reading: Option<bool>,
        /// User identifier that inserted the entry
        pub uid: Option<i64>,
        /// Optional password to encrypt the entry
        pub password: Option<String>,
        /// Title
        pub title: Option<String>,
        /// Whether this entry is a binary asset uploaded from the pad editor. Only assets have
        /// their `data:` URL text decoded into raw bytes.
        #[serde(default)]
        pub is_asset: bool,
        /// Whether this entry is a live pad, editable through the pad API.
        #[serde(default)]
        pub is_pad: bool,
    }

    /// New content and optional metadata changes for an existing pad.
    #[derive(Debug)]
    pub struct PadUpdate {
        /// Compressed content
        pub data: Vec<u8>,
        /// File extension
        pub extension: Option<String>,
        /// `Some` replaces the title (`Some(None)` clears it), `None` keeps it.
        pub title: Option<Option<String>>,
        /// `Some` resets the expiration (`Some(None)` never expires), `None` keeps it.
        pub expires: Option<Option<NonZeroU32>>,
        /// Only update if the pad is still at this revision (its `updated_at`).
        pub base_revision: Option<String>,
    }

    /// A compressed entry to be inserted.
    pub struct CompressedEntry {
        /// Original data
        entry: Entry,
        /// Compressed data
        data: Vec<u8>,
    }

    /// An entry that might be encrypted.
    pub struct DatabaseEntry {
        /// Original data
        pub entry: Entry,
        /// Compressed and potentially encrypted data
        pub data: Vec<u8>,
        /// Nonce for this entry
        pub nonce: Option<XNonce>,
    }

    impl Entry {
        /// Compress the entry for insertion. The `data:…;base64,` text of an asset upload is
        /// decoded and stored as raw bytes; any other text is stored as is.
        pub async fn compress(mut self) -> Result<CompressedEntry, Error> {
            let decoded = if self.is_asset {
                decode_data_url(&self.text)
            } else {
                None
            };

            let bytes = match decoded {
                Some((ext, bytes)) => {
                    if self.extension.is_none() {
                        self.extension = ext.map(str::to_string);
                    }
                    bytes
                }
                None => self.text.as_bytes().to_vec(),
            };

            let mut encoder = ZstdEncoder::new(Cursor::new(bytes));
            let mut data = Vec::new();

            encoder
                .read_to_end(&mut data)
                .await
                .map_err(|e| Error::Compression(e.to_string()))?;

            Ok(CompressedEntry { entry: self, data })
        }
    }

    /// Decode a base64 `data:` URL into the file extension of its media type and its bytes.
    fn decode_data_url(text: &str) -> Option<(Option<&'static str>, Vec<u8>)> {
        use base64::Engine;

        let (header, b64) = text.strip_prefix("data:")?.split_once(";base64,")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(b64.trim()))
            .ok()?;

        let ext = match header.trim() {
            "image/png" => Some("png"),
            "image/jpeg" | "image/jpg" => Some("jpg"),
            "image/gif" => Some("gif"),
            "image/webp" => Some("webp"),
            "image/svg+xml" => Some("svg"),
            "image/avif" => Some("avif"),
            "image/bmp" => Some("bmp"),
            "image/x-icon" | "image/vnd.microsoft.icon" => Some("ico"),
            "image/tiff" => Some("tiff"),
            "application/pdf" => Some("pdf"),
            _ => None,
        };

        Some((ext, bytes))
    }

    /// Compress `text` for storing it as pad content.
    pub async fn compress_text(text: &str) -> Result<Vec<u8>, Error> {
        let mut encoder = ZstdEncoder::new(Cursor::new(text.as_bytes()));
        let mut data = Vec::new();

        encoder
            .read_to_end(&mut data)
            .await
            .map_err(|e| Error::Compression(e.to_string()))?;

        Ok(data)
    }

    impl CompressedEntry {
        /// Encrypt if password is set.
        pub async fn encrypt(self) -> Result<DatabaseEntry, Error> {
            let (data, nonce) = if let Some(password) = &self.entry.password {
                let password = Password::from(password.as_bytes().to_vec());
                let plaintext = Plaintext::from(self.data);
                let Encrypted { ciphertext, nonce } = plaintext.encrypt(password).await?;
                (ciphertext, Some(nonce))
            } else {
                (self.data, None)
            };

            Ok(DatabaseEntry {
                entry: self.entry,
                data,
                nonce,
            })
        }
    }
}

/// Module with types for reading from the database.
pub mod read {
    use crate::crypto::{Encrypted, Password};
    use crate::db::Error;
    use crate::expiration::Expiration;
    use crate::id::Id;
    use async_compression::tokio::bufread::ZstdDecoder;
    use chacha20poly1305::XNonce;
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    /// A raw entry as read from the database.
    #[derive(Debug)]
    pub struct DatabaseEntry {
        /// Compressed and potentially encrypted data
        pub data: Vec<u8>,
        /// Metadata
        pub metadata: Metadata,
        /// Entry is expired
        pub expired: bool,
        /// Entry must be deleted
        pub must_be_deleted: bool,
        /// Nonce for this entry
        pub nonce: Option<XNonce>,
    }

    /// Potentially decrypted but still compressed entry
    #[derive(Debug)]
    pub struct CompressedReadEntry {
        /// Compressed data
        data: Vec<u8>,
        /// Metadata
        metadata: Metadata,
        /// Entry must be deleted
        must_be_deleted: bool,
    }

    /// Uncompressed entry
    #[derive(Debug)]
    pub struct UncompressedEntry {
        /// Content as string (empty if binary/not valid UTF-8)
        pub text: String,
        /// Raw content bytes
        pub bytes: Vec<u8>,
        /// Metadata
        pub metadata: Metadata,
        /// Entry must be deleted
        pub must_be_deleted: bool,
    }

    /// Uncompressed, decrypted data read from the database.
    #[derive(Debug)]
    pub struct Data {
        /// Content as string (empty if binary/not valid UTF-8)
        pub text: String,
        /// Raw content bytes
        pub bytes: Vec<u8>,
        /// Metadata
        pub metadata: Metadata,
    }

    /// Paste metadata, i.e. anything but actual content.
    #[derive(Debug)]
    pub struct Metadata {
        /// User identifier that inserted the entry
        pub uid: Option<i64>,
        /// Title
        pub title: Option<String>,
        /// File extension
        pub extension: Option<String>,
        /// Entry is a binary asset uploaded from the pad editor
        pub is_asset: bool,
        /// Entry is a live pad
        pub is_pad: bool,
        /// Last modification, identifies the revision of a pad
        pub updated_at: Option<String>,
        /// Entry expiration datetime
        pub expiration: Option<Expiration>,
        /// Entry will be deleted the next time it is fetched via [`Database::get`].
        pub must_be_deleted: bool,
    }

    /// Potentially deleted or non-existent expired entry.
    #[derive(Debug)]
    pub enum Entry {
        /// Entry found and still available.
        Regular(Data),
        /// Entry burned.
        Burned(Data),
        /// Entry decrypted with the supplied password. Takes precedence over [`Entry::Burned`]. Whether the paste was also burned is recorded in [`Metadata::must_be_deleted`].
        Encrypted(Data),
    }

    /// A simple entry as read from the database for listing purposes.
    #[derive(Debug)]
    pub struct ListEntry {
        /// Identifier
        pub id: Id,
        /// Optional title
        pub title: Option<String>,
        /// If entry is encrypted
        pub is_encrypted: bool,
        /// Expiration if set
        pub expiration: Option<String>,
        /// If entry is expired
        pub is_expired: bool,
    }

    /// Summary of a pad for the recent pads list.
    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct PadSummary {
        /// Identifier
        pub id: String,
        /// Title
        pub title: Option<String>,
        /// First line of text, ignoring inline asset captions
        pub preview: Option<String>,
        /// Last modification as RFC 3339 UTC timestamp
        pub updated_at: Option<String>,
    }

    impl DatabaseEntry {
        pub async fn decrypt(
            self,
            password: Option<Password>,
        ) -> Result<CompressedReadEntry, Error> {
            match (self.nonce, password) {
                (Some(_), None) => Err(Error::NoPassword),
                (None, None | Some(_)) => Ok(CompressedReadEntry {
                    data: self.data,
                    metadata: self.metadata,
                    must_be_deleted: self.must_be_deleted,
                }),
                (Some(nonce), Some(password)) => {
                    let encrypted = Encrypted::new(self.data, nonce);
                    let decrypted = encrypted.decrypt(password).await?;
                    Ok(CompressedReadEntry {
                        data: decrypted,
                        metadata: self.metadata,
                        must_be_deleted: self.must_be_deleted,
                    })
                }
            }
        }
    }

    impl CompressedReadEntry {
        pub async fn decompress(self) -> Result<UncompressedEntry, Error> {
            let mut decoder = ZstdDecoder::new(Cursor::new(self.data));
            let mut bytes = Vec::new();

            decoder
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| Error::Compression(e.to_string()))?;

            let text = std::str::from_utf8(&bytes)
                .map(str::to_owned)
                .unwrap_or_default();

            Ok(UncompressedEntry {
                bytes,
                text,
                metadata: self.metadata,
                must_be_deleted: self.must_be_deleted,
            })
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound,
            _ => Error::Sqlite(err),
        }
    }
}

impl Handler {
    /// Create new database with the given `method`.
    fn new(method: Open, receiver: kanal::Receiver<Command>) -> Result<Self, Error> {
        tracing::debug!("opening {method:?}");

        let mut conn = match method {
            Open::Memory => Connection::open_in_memory()?,
            Open::Path(path) => Connection::open(path)?,
        };

        let migrations = Migrations::new(vec![
            M::up(include_str!("migrations/0001-initial.sql")),
            M::up(include_str!("migrations/0002-add-created-column.sql")),
            M::up(include_str!(
                "migrations/0003-drop-created-add-uid-column.sql"
            )),
            M::up_with_hook(
                include_str!("migrations/0004-add-compressed-column.sql"),
                |tx: &Transaction| {
                    let mut stmt = tx.prepare("SELECT id, text FROM entries")?;

                    let rows = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<Result<Vec<(u32, String)>, _>>()?;

                    tracing::debug!("compressing {} rows", rows.len());

                    for (id, text) in rows {
                        let cursor = Cursor::new(text);
                        let data =
                            zstd::stream::encode_all(cursor, zstd::DEFAULT_COMPRESSION_LEVEL)
                                .map_err(|e| HookError::Hook(e.to_string()))?;

                        tx.execute(
                            "UPDATE entries SET data = ?1 WHERE id = ?2",
                            params![data, id],
                        )?;
                    }

                    Ok(())
                },
            ),
            M::up(include_str!("migrations/0005-drop-text-column.sql")),
            M::up(include_str!("migrations/0006-add-nonce-column.sql")),
            M::up(include_str!("migrations/0007-add-title-column.sql")),
            M::up(include_str!("migrations/0008-add-extension-column.sql")),
            M::up(include_str!(
                "migrations/0009-add-updated-at-and-is-asset.sql"
            )),
            M::up(include_str!("migrations/0010-add-is-pad-column.sql")),
        ]);

        migrations.to_latest(&mut conn)?;

        Ok(Self {
            conn,
            receiver,
            write_seq: 0,
        })
    }

    fn next_updated_at(&mut self) -> Result<String, rusqlite::Error> {
        self.write_seq = self.write_seq.wrapping_add(1);
        let seq = self.write_seq;
        let ts: String =
            self.conn
                .query_row("SELECT strftime('%Y-%m-%d %H:%M:%f', 'now')", [], |row| {
                    row.get(0)
                })?;
        Ok(format!("{ts}.{seq:010}"))
    }

    /// Run database command loop.
    fn run(mut self) -> Result<(), Error> {
        loop {
            let command = match self.receiver.recv() {
                Ok(command) => command,
                Err(kanal::ReceiveError::Closed | kanal::ReceiveError::SendClosed) => return Ok(()), // sender closed, application is shutting down..
            };

            let result = match command {
                Command::Insert { entry, result } => result
                    .send(self.insert(entry))
                    .map_err(|_| Error::ResultSendError),
                Command::Get { id, result } => result
                    .send(self.get(id))
                    .map_err(|_| Error::ResultSendError),
                Command::GetMetadata { id, result } => result
                    .send(self.get_metadata(id))
                    .map_err(|_| Error::ResultSendError),
                Command::Delete { id, result } => result
                    .send(self.delete(id))
                    .map_err(|_| Error::ResultSendError),
                Command::DeleteMany { ids, result } => result
                    .send(self.delete_many(ids))
                    .map_err(|_| Error::ResultSendError),
                Command::DeleteFor { id, uids, result } => result
                    .send(self.delete_for(id, &uids))
                    .map_err(|_| Error::ResultSendError),
                Command::NextUid { result } => result
                    .send(self.next_uid())
                    .map_err(|_| Error::ResultSendError),
                Command::List { result } => {
                    result.send(self.list()).map_err(|_| Error::ResultSendError)
                }
                Command::Purge { result } => result
                    .send(self.purge())
                    .map_err(|_| Error::ResultSendError),
                Command::UpdatePad { id, update, result } => result
                    .send(self.update_pad(id, update))
                    .map_err(|_| Error::ResultSendError),
                Command::DeletePad { id, result } => result
                    .send(self.delete_pad(id))
                    .map_err(|_| Error::ResultSendError),
                Command::GetLatestPadId { result } => result
                    .send(self.get_latest_pad_id())
                    .map_err(|_| Error::ResultSendError),
                Command::ListRecentPads { limit, result } => result
                    .send(self.list_recent_pads(limit))
                    .map_err(|_| Error::ResultSendError),
            };

            if let Err(err) = result {
                tracing::error!(?err, "failed to send result");
            }
        }
    }

    fn insert(
        &mut self,
        write::DatabaseEntry { entry, data, nonce }: write::DatabaseEntry,
    ) -> Result<(Id, write::Entry), Error> {
        let mut counter = 0;
        let title = entry.title.clone();
        let nonce = nonce.as_ref().map(|n| n.as_slice());
        let updated_at = self.next_updated_at()?;

        loop {
            let id = Id::rand();

            let result = match entry.expires {
                None => self.conn.execute(
                    "INSERT INTO entries (id, uid, data, burn_after_reading, nonce, title, extension, is_asset, is_pad, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        id.to_i64(),
                        entry.uid,
                        data,
                        entry.burn_after_reading,
                        nonce,
                        title,
                        entry.extension,
                        entry.is_asset,
                        entry.is_pad,
                        &updated_at,
                    ],
                ),
                Some(expires) => self.conn.execute(
                    "INSERT INTO entries (id, uid, data, burn_after_reading, nonce, expires, title, extension, is_asset, is_pad, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, datetime('now', ?6), ?7, ?8, ?9, ?10, ?11)",
                    params![
                        id.to_i64(),
                        entry.uid,
                        data,
                        entry.burn_after_reading,
                        nonce,
                        format!("{expires} seconds"),
                        title,
                        entry.extension,
                        entry.is_asset,
                        entry.is_pad,
                        &updated_at,
                    ],
                ),
            };

            match result {
                Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code,
                        extended_code,
                    },
                    Some(ref _message),
                )) if code == rusqlite::ErrorCode::ConstraintViolation
                    && extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                    && counter < 10 =>
                {
                    // Retry if ID is already existent
                    counter += 1;
                    continue;
                }
                Err(err) => break Err(err)?,
                Ok(rows) => {
                    debug_assert!(rows == 1);
                    return Ok((id, entry));
                }
            }
        }
    }

    fn get_metadata(&self, id: Id) -> Result<Metadata, Error> {
        let metadata = self.conn.query_row(
            "SELECT uid, title, CAST(ROUND((julianday(expires) - julianday('now')) * 86400) AS INTEGER), burn_after_reading, extension, is_asset, is_pad, updated_at FROM entries WHERE id=?1",
            params![id.to_i64()],
            |row| {
                let expiration = row.get::<_, Option<i64>>(2)?
                    .filter(|secs| *secs > 0)
                    .and_then(|secs| u64::try_from(secs).ok())
                    .map(|secs| Expiration { duration: Duration::from_secs(secs), default: false });

                Ok(read::Metadata {
                    uid: row.get(0)?,
                    title: row.get::<_, Option<String>>(1)?,
                    extension: row.get::<_, Option<String>>(4)?,
                    is_asset: row.get::<_, Option<bool>>(5)?.unwrap_or(false),
                    is_pad: row.get::<_, Option<bool>>(6)?.unwrap_or(false),
                    updated_at: row.get::<_, Option<String>>(7)?,
                    expiration,
                    must_be_deleted: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                })
            }
        )?;

        Ok(metadata)
    }

    fn get(&self, id: Id) -> Result<DatabaseEntry, Error> {
        let metadata = self.get_metadata(id)?;
        let entry = self.conn.query_row(
            "SELECT data, burn_after_reading, nonce, expires < datetime('now') FROM entries WHERE id=?1",
            params![id.to_i64()],
            |row| {
                let nonce = row
                    .get::<_, Option<Vec<_>>>(2)?
                    .map(|v| XNonce::try_from(v.as_slice()))
                    .transpose()
                    .map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Blob,
                            Box::new(err),
                        )
                    })?;

                Ok(read::DatabaseEntry {
                    data: row.get(0)?,
                    metadata,
                    must_be_deleted: row.get::<_, Option<bool>>(1)?.unwrap_or(false),
                    nonce,
                    expired: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                })
            },
        )?;

        Ok(entry)
    }

    fn delete(&self, id: Id) -> Result<bool, Error> {
        Ok(self
            .conn
            .execute("DELETE FROM entries WHERE id=?1", params![id.to_i64()])?
            != 0)
    }

    fn delete_many(&mut self, ids: Vec<Id>) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;

        let mut affected = 0;

        for id in ids {
            affected += tx.execute("DELETE FROM entries WHERE id=?1", params![id.to_i64()])?;
        }

        tx.commit()?;
        Ok(affected)
    }

    fn delete_for(&mut self, id: Id, uids: &[i64]) -> Result<(), Error> {
        if uids.is_empty() {
            return Err(Error::Delete);
        }

        let placeholders = vec!["?"; uids.len()].join(",");
        let delete_sql = format!("DELETE FROM entries WHERE id=? AND uid IN ({placeholders})");

        let mut params = Vec::with_capacity(uids.len() + 1);
        params.push(id.to_i64());
        params.extend_from_slice(uids);

        let affected = self.conn.execute(&delete_sql, params_from_iter(&params))?;

        if affected == 0 {
            return Err(Error::Delete);
        }

        Ok(())
    }

    fn next_uid(&self) -> Result<i64, Error> {
        let uid = self.conn.query_row(
            "UPDATE uids SET n = n + 1 WHERE id = 0 RETURNING n",
            [],
            |row| row.get(0),
        )?;

        Ok(uid)
    }

    fn list(&self) -> Result<Vec<ListEntry>, Error> {
        let entries = self
            .conn
            .prepare("SELECT id, title, nonce, expires, expires < datetime('now') FROM entries")?
            .query_map([], |row| {
                Ok(ListEntry {
                    id: Id::from(row.get::<_, i64>(0)?),
                    title: row.get(1)?,
                    is_encrypted: row.get::<_, Option<Vec<u8>>>(2)?.is_some(),
                    expiration: row.get(3)?,
                    is_expired: row.get::<_, Option<bool>>(4)?.unwrap_or_default(),
                })
            })?
            .collect::<Result<_, _>>()?;

        Ok(entries)
    }

    fn purge(&self) -> Result<Vec<Id>, Error> {
        let ids = self
            .conn
            .prepare("DELETE FROM entries WHERE expires < datetime('now') RETURNING id")?
            .query_map([], |row| Ok(Id::from(row.get::<_, i64>(0)?)))?
            .collect::<Result<_, _>>()?;

        Ok(ids)
    }

    fn update_pad(&mut self, id: Id, update: write::PadUpdate) -> Result<Metadata, Error> {
        let updated_at = self.next_updated_at()?;

        let mut sql = String::from("UPDATE entries SET data = ?2, extension = ?3, updated_at = ?4");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(id.to_i64()),
            Box::new(update.data),
            Box::new(update.extension),
            Box::new(updated_at),
        ];

        if let Some(title) = update.title {
            values.push(Box::new(title));
            sql.push_str(&format!(", title = ?{}", values.len()));
        }

        match update.expires {
            None => {}
            Some(None) => sql.push_str(", expires = NULL"),
            Some(Some(expires)) => {
                values.push(Box::new(format!("{expires} seconds")));
                sql.push_str(&format!(", expires = datetime('now', ?{})", values.len()));
            }
        }

        // Only live pads may be modified: this never touches encrypted, burn-after-reading or
        // asset entries, and never creates a row for an unknown id.
        sql.push_str(
            " WHERE id = ?1 AND is_pad = 1 AND (expires IS NULL OR expires > datetime('now'))",
        );

        if let Some(base_revision) = update.base_revision {
            values.push(Box::new(base_revision));
            sql.push_str(&format!(" AND updated_at = ?{}", values.len()));
        }

        let affected = self
            .conn
            .execute(&sql, params_from_iter(values.iter().map(AsRef::as_ref)))?;

        if affected == 0 {
            return Err(Error::NotFound);
        }

        self.get_metadata(id)
    }

    fn delete_pad(&self, id: Id) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "DELETE FROM entries WHERE id = ?1 AND is_pad = 1",
            params![id.to_i64()],
        )? != 0)
    }

    fn get_latest_pad_id(&self) -> Result<Option<Id>, Error> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM entries WHERE is_pad = 1 AND (expires IS NULL OR expires > datetime('now')) ORDER BY updated_at DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        Ok(id.map(Id::from))
    }

    fn list_recent_pads(&self, limit: usize) -> Result<Vec<PadSummary>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, data, updated_at, title FROM entries WHERE is_pad = 1 AND (expires IS NULL OR expires > datetime('now')) ORDER BY updated_at DESC, rowid DESC LIMIT ?1",
        )?;

        let pads = stmt
            .query_map(params![limit], |row| {
                let data: Vec<u8> = row.get(1)?;
                let updated_at: Option<String> = row.get(2)?;

                Ok(PadSummary {
                    id: Id::from(row.get::<_, i64>(0)?).to_string(),
                    title: row
                        .get::<_, Option<String>>(3)?
                        .filter(|title| !title.trim().is_empty()),
                    preview: Self::extract_preview(&data),
                    updated_at: updated_at.as_deref().and_then(to_rfc3339),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(pads)
    }

    /// First non-empty line of a pad's text. Markup is stripped and the content of `<figure>`
    /// elements (inline images and PDFs with their captions) is skipped.
    fn extract_preview(data: &[u8]) -> Option<String> {
        const MAX_CHARS: usize = 48;

        let decoded = zstd::stream::decode_all(Cursor::new(data)).ok()?;
        let text = std::str::from_utf8(&decoded).ok()?;

        let mut line = String::new();
        let mut rest = text;
        let mut figure_depth = 0usize;

        while !rest.is_empty() {
            let (chunk, tag) = match rest.find('<') {
                Some(start) => match rest[start..].find('>') {
                    Some(len) => (&rest[..start], Some(&rest[start + 1..start + len])),
                    None => (rest, None),
                },
                None => (rest, None),
            };

            if figure_depth == 0 {
                for ch in chunk.chars() {
                    if ch == '\n' {
                        if !line.trim().is_empty() {
                            return Some(Self::finish_preview(&line, MAX_CHARS));
                        }
                        line.clear();
                    } else {
                        line.push(ch);
                    }
                }
            }

            let Some(tag) = tag else { break };
            rest = &rest[chunk.len() + tag.len() + 2..];

            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();

            if name == "figure" {
                if tag.starts_with('/') {
                    figure_depth = figure_depth.saturating_sub(1);
                } else {
                    figure_depth += 1;
                }
            } else if figure_depth == 0
                && matches!(
                    name.as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "tr"
                        | "pre"
                        | "blockquote"
                        | "hr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                )
            {
                if !line.trim().is_empty() {
                    return Some(Self::finish_preview(&line, MAX_CHARS));
                }
                line.clear();
            } else if figure_depth == 0 {
                line.push(' ');
            }
        }

        (!line.trim().is_empty()).then(|| Self::finish_preview(&line, MAX_CHARS))
    }

    fn finish_preview(line: &str, max_chars: usize) -> String {
        let line = line
            .replace("&nbsp;", " ")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&");
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");

        let mut preview: String = line.chars().take(max_chars).collect();
        if line.chars().count() > max_chars {
            preview.push('…');
        }
        preview
    }
}

/// Convert an `updated_at` value (`YYYY-MM-DD HH:MM:SS.fff` in UTC, optionally followed by a
/// write sequence suffix) into an RFC 3339 timestamp.
fn to_rfc3339(value: &str) -> Option<String> {
    let datetime = value.get(..23).or_else(|| value.get(..19))?;
    Some(format!("{}Z", datetime.replacen(' ', "T", 1)))
}

impl Database {
    /// Create new database with the given `method` as well as a [`Handler`] future that makes the
    /// actual calls.
    pub fn new(method: Open) -> Result<(Self, impl Future<Output = Result<(), Error>>), Error> {
        let (sender, receiver) = kanal::bounded(256);
        let sender = sender.to_async();
        let handler = Handler::new(method, receiver)?;
        let fut = async move { tokio::task::spawn_blocking(|| handler.run()).await? };
        Ok((Self { sender }, fut))
    }

    /// Insert `entry` under a new random id into the database and optionally set owner to `uid`.
    /// Returns the id of the new entry on success.
    pub async fn insert(&self, entry: write::Entry) -> Result<(Id, write::Entry), Error> {
        let entry = entry.compress().await?.encrypt().await?;

        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::Insert { entry, result })
            .await
            .map_err(|_| Error::SendError)?;

        command_result.await?
    }

    /// Get entire entry for `id`.
    pub async fn get(&self, id: Id, password: Option<Password>) -> Result<read::Entry, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::Get { id, result })
            .await
            .map_err(|_| Error::SendError)?;

        let entry = command_result.await??;

        if entry.expired {
            self.delete(id).await?;
            return Err(Error::NotFound);
        }

        let is_encrypted = entry.nonce.is_some();

        let read::UncompressedEntry {
            bytes,
            text,
            metadata,
            must_be_deleted,
        } = entry.decrypt(password).await?.decompress().await?;

        let data = read::Data {
            bytes,
            text,
            metadata,
        };

        // Whether this fetch burned the paste.
        let burned = if must_be_deleted {
            if !self.delete(id).await? {
                return Err(Error::NotFound);
            }

            true
        } else {
            false
        };

        // Password-protected pastes take precedence over burned ones so that callers
        // can tell that the content must not be cached.
        Ok(match (is_encrypted, burned) {
            (true, _) => read::Entry::Encrypted(data),
            (false, true) => read::Entry::Burned(data),
            (false, false) => read::Entry::Regular(data),
        })
    }

    /// Get metadata of a paste.
    pub async fn get_metadata(&self, id: Id) -> Result<Metadata, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::GetMetadata { id, result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Delete paste with `id`. Returns whether the entry was still present.
    async fn delete(&self, id: Id) -> Result<bool, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::Delete { id, result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Delete pastes with `ids`.
    pub async fn delete_many(&self, ids: Vec<Id>) -> Result<usize, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::DeleteMany { ids, result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Delete paste with `id` if any of `uids` owns it.
    pub async fn delete_for(&self, id: Id, uids: &[i64]) -> Result<(), Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::DeleteFor {
                id,
                uids: uids.to_vec(),
                result,
            })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Retrieve next monotonically increasing uid.
    pub async fn next_uid(&self) -> Result<i64, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::NextUid { result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// List all entries.
    pub async fn list(&self) -> Result<Vec<ListEntry>, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::List { result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Purge all expired entries and return their [`Id`]s
    pub async fn purge(&self) -> Result<Vec<Id>, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::Purge { result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Get the content of the live pad `id`. Entries that are not pads (regular, encrypted,
    /// burn-after-reading pastes or assets) are reported as [`Error::NotFound`] without being
    /// read, so that a burn-after-reading paste is not burned.
    pub async fn get_pad(&self, id: Id) -> Result<read::Data, Error> {
        if !self.get_metadata(id).await?.is_pad {
            return Err(Error::NotFound);
        }

        match self.get(id, None).await? {
            read::Entry::Regular(data) => Ok(data),
            read::Entry::Burned(_) | read::Entry::Encrypted(_) => Err(Error::NotFound),
        }
    }

    /// Replace the content of the live pad `id` and return its updated metadata. Fails with
    /// [`Error::NotFound`] if `id` is not a live pad or is no longer at `base_revision`.
    pub async fn update_pad(
        &self,
        id: Id,
        text: &str,
        extension: Option<String>,
        title: Option<Option<String>>,
        expires: Option<Option<NonZeroU32>>,
        base_revision: Option<String>,
    ) -> Result<Metadata, Error> {
        let update = write::PadUpdate {
            data: write::compress_text(text).await?,
            extension,
            title,
            expires,
            base_revision,
        };

        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::UpdatePad { id, update, result })
            .await
            .map_err(|_| Error::SendError)?;

        command_result.await?
    }

    /// Delete the live pad `id`. Returns `false` if `id` is not a pad.
    pub async fn delete_pad(&self, id: Id) -> Result<bool, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::DeletePad { id, result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }

    /// Retrieve the most recently updated live pad.
    pub async fn get_latest_pad(&self) -> Result<Option<(Id, read::Data)>, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::GetLatestPadId { result })
            .await
            .map_err(|_| Error::SendError)?;

        let Some(id) = command_result.await?? else {
            return Ok(None);
        };

        match self.get_pad(id).await {
            Ok(data) => Ok(Some((id, data))),
            Err(Error::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// List live pads, most recently updated first.
    pub async fn list_recent_pads(&self, limit: usize) -> Result<Vec<read::PadSummary>, Error> {
        let (result, command_result) = oneshot::channel();
        self.sender
            .send(Command::ListRecentPads { limit, result })
            .await
            .map_err(|_| Error::SendError)?;
        command_result.await?
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    impl read::Entry {
        /// Unwrap inner data or panic.
        #[must_use]
        pub fn unwrap_inner(self) -> read::Data {
            match self {
                read::Entry::Regular(data)
                | read::Entry::Burned(data)
                | read::Entry::Encrypted(data) => data,
            }
        }
    }

    fn new_db() -> Result<Database, Box<dyn std::error::Error>> {
        let (db, handler) = Database::new(Open::Memory)?;
        tokio::spawn(handler);
        Ok(db)
    }

    #[tokio::test]
    async fn insert() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            text: "hello world".to_string(),
            uid: Some(10),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        let entry = db.get(id, None).await?.unwrap_inner();
        assert_eq!(entry.text, "hello world");
        assert!(entry.metadata.uid.is_some());
        assert_eq!(entry.metadata.uid.unwrap(), 10);

        let result = db.get(Id::from(5678u32), None).await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn next_uid() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let uid1 = db.next_uid().await?;
        let uid2 = db.next_uid().await?;
        let uid3 = db.next_uid().await?;

        assert!(uid1 < uid2);
        assert!(uid2 < uid3);

        Ok(())
    }

    #[tokio::test]
    async fn expired_does_not_exist() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            expires: Some(NonZeroU32::new(1).unwrap()),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        let result = db.get(id, None).await;
        assert!(matches!(result, Err(Error::NotFound)));

        Ok(())
    }

    #[tokio::test]
    async fn delete() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let (id, _entry) = db.insert(write::Entry::default()).await?;

        assert!(db.get(id, None).await.is_ok());
        assert!(db.delete(id).await.is_ok());
        assert!(db.get(id, None).await.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn burn_after_reading() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            text: "burn me".to_string(),
            burn_after_reading: Some(true),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        assert!(matches!(db.get(id, None).await?, read::Entry::Burned(_)));
        assert!(matches!(db.get(id, None).await, Err(Error::NotFound)));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn burn_after_reading_single_view() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            text: "burn me".repeat(10_000),
            burn_after_reading: Some(true),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let db = db.clone();
            set.spawn(async move { db.get(id, None).await });
        }

        let results: Vec<_> = set.join_all().await;

        let (burned, not_found, regular, other) = results.into_iter().fold(
            (0, 0, 0, 0),
            |(burned, not_found, regular, other), result| match result {
                Ok(read::Entry::Burned(_)) => (burned + 1, not_found, regular, other),
                Err(Error::NotFound) => (burned, not_found + 1, regular, other),
                Ok(read::Entry::Regular(_) | read::Entry::Encrypted(_)) => {
                    (burned, not_found, regular + 1, other)
                }
                Err(_) => (burned, not_found, regular, other + 1),
            },
        );
        assert_eq!(burned, 1);
        assert_eq!(not_found, 19);
        assert_eq!(regular, 0);
        assert_eq!(other, 0);

        Ok(())
    }

    #[tokio::test]
    async fn delete_for() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let uid = 42;

        let entry = write::Entry {
            uid: Some(uid),
            ..Default::default()
        };
        let (id, _entry) = db.insert(entry).await?;

        assert!(db.get(id, None).await.is_ok());
        assert!(db.delete_for(id, &[uid]).await.is_ok());
        assert!(db.get(id, None).await.is_err());

        let entry = write::Entry {
            uid: Some(uid),
            ..Default::default()
        };
        let (id, _entry) = db.insert(entry).await?;

        let incorrect_uid = 99;
        assert!(matches!(
            db.delete_for(id, &[incorrect_uid]).await,
            Err(Error::Delete)
        ));

        // Multi-uid: passes if any uid in the list matches.
        let entry = write::Entry {
            uid: Some(uid),
            ..Default::default()
        };
        let (id, _entry) = db.insert(entry).await?;

        assert!(db.delete_for(id, &[99, uid, 7]).await.is_ok());
        assert!(db.get(id, None).await.is_err());

        // Empty slice: nothing to authorize against.
        let entry = write::Entry {
            uid: Some(uid),
            ..Default::default()
        };
        let (id, _entry) = db.insert(entry).await?;

        assert!(matches!(db.delete_for(id, &[]).await, Err(Error::Delete)));

        assert!(db.get(id, None).await.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn purge() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            expires: Some(NonZeroU32::new(1).unwrap()),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        let ids = db.purge().await?;
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], id);

        Ok(())
    }

    #[tokio::test]
    async fn get_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            text: "test content".to_string(),
            uid: Some(42),
            title: Some("Test Title".to_string()),
            expires: Some(NonZeroU32::new(3600).unwrap()),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        let metadata = db.get_metadata(id).await?;
        assert_eq!(metadata.uid, Some(42));
        assert_eq!(metadata.title, Some("Test Title".to_string()));

        let expiration = metadata.expiration.unwrap().duration.as_secs();
        assert!(expiration <= 3600);
        // We have a problem if the test takes more than 10 seconds.
        assert!(expiration >= 3590);

        Ok(())
    }

    #[tokio::test]
    async fn read_encrypted() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = write::Entry {
            text: "secret".to_string(),
            password: Some("pw".to_string()),
            burn_after_reading: Some(true),
            ..Default::default()
        };

        let (id, _entry) = db.insert(entry).await?;

        // Without a password the content stays locked.
        assert!(matches!(db.get(id, None).await, Err(Error::NoPassword)));

        // Encrypted takes precedence over burned, but the burn is still reported.
        let entry = db
            .get(id, Some(Password::from("pw".as_bytes().to_vec())))
            .await?;
        assert!(matches!(entry, read::Entry::Encrypted(_)));
        assert!(entry.unwrap_inner().metadata.must_be_deleted);

        Ok(())
    }

    #[tokio::test]
    async fn asset_data_url_is_decoded() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        // Base64 of [1, 2, 3, 4, 5] is AQIDBAU=
        let (id, entry) = db
            .insert(write::Entry {
                text: "data:image/png;base64,AQIDBAU=".to_string(),
                title: Some("test.png".to_string()),
                is_asset: true,
                ..Default::default()
            })
            .await?;
        assert_eq!(entry.extension.as_deref(), Some("png"));

        let metadata = db.get_metadata(id).await?;
        assert_eq!(metadata.extension.as_deref(), Some("png"));
        assert!(metadata.is_asset);

        let data = db.get(id, None).await?.unwrap_inner();
        assert_eq!(data.bytes, vec![1, 2, 3, 4, 5]);

        Ok(())
    }

    #[tokio::test]
    async fn paste_data_url_is_kept_as_text() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let text = "data:image/png;base64,AQIDBAU=";
        let (id, entry) = db
            .insert(write::Entry {
                text: text.to_string(),
                ..Default::default()
            })
            .await?;
        assert_eq!(entry.extension, None);

        let data = db.get(id, None).await?.unwrap_inner();
        assert_eq!(data.text, text);
        assert!(!data.metadata.is_asset);

        Ok(())
    }

    fn pad(text: &str) -> write::Entry {
        write::Entry {
            text: text.to_string(),
            extension: Some("txt".to_string()),
            is_pad: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn update_pad_keeps_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let (id, _) = db
            .insert(write::Entry {
                title: Some("notes".to_string()),
                expires: NonZeroU32::new(3600),
                ..pad("initial")
            })
            .await?;

        let metadata = db
            .update_pad(id, "updated", Some("md".to_string()), None, None, None)
            .await?;
        assert_eq!(metadata.title.as_deref(), Some("notes"));
        assert!(metadata.expiration.is_some());

        let data = db.get_pad(id).await?;
        assert_eq!(data.text, "updated");
        assert_eq!(data.metadata.extension.as_deref(), Some("md"));

        let metadata = db
            .update_pad(id, "updated", None, Some(None), Some(None), None)
            .await?;
        assert_eq!(metadata.title, None);
        assert!(metadata.expiration.is_none());

        // A write based on an outdated revision is refused.
        let stale = Some("2000-01-01 00:00:00.000".to_string());
        assert!(matches!(
            db.update_pad(id, "stale", None, None, None, stale).await,
            Err(Error::NotFound)
        ));
        let current = metadata.updated_at.clone();
        let metadata = db
            .update_pad(id, "fresh", None, None, None, current.clone())
            .await?;
        assert_ne!(metadata.updated_at, current);
        assert_eq!(db.get_pad(id).await?.text, "fresh");

        Ok(())
    }

    #[tokio::test]
    async fn pad_api_refuses_other_entries() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let (encrypted, _) = db
            .insert(write::Entry {
                text: "secret".to_string(),
                password: Some("hunter2".to_string()),
                ..Default::default()
            })
            .await?;
        let (burn, _) = db
            .insert(write::Entry {
                text: "burn".to_string(),
                burn_after_reading: Some(true),
                ..Default::default()
            })
            .await?;
        let (paste, _) = db
            .insert(write::Entry {
                text: "paste".to_string(),
                ..Default::default()
            })
            .await?;

        for id in [encrypted, burn, paste, Id::from(12345u32)] {
            assert!(matches!(db.get_pad(id).await, Err(Error::NotFound)));
            assert!(matches!(
                db.update_pad(id, "clobbered", None, None, None, None).await,
                Err(Error::NotFound)
            ));
            assert!(!db.delete_pad(id).await?);
        }

        // Nothing was burned, modified or inserted.
        let password = Password::from("hunter2".as_bytes().to_vec());
        assert_eq!(
            db.get(encrypted, Some(password)).await?.unwrap_inner().text,
            "secret"
        );
        assert_eq!(db.get(burn, None).await?.unwrap_inner().text, "burn");
        assert_eq!(db.get(paste, None).await?.unwrap_inner().text, "paste");
        assert!(matches!(
            db.get(Id::from(12345u32), None).await,
            Err(Error::NotFound)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn latest_and_recent_pads_list_only_pads() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        assert!(db.get_latest_pad().await?.is_none());

        let (pad1_id, _) = db
            .insert(write::Entry {
                extension: Some("rich".to_string()),
                ..pad("<figure class=\"inline-asset\"><img src=\"/raw/x.png\"><figcaption>image.png · 100 KB</figcaption></figure><p>Hello <b>rich</b> &amp; world</p><p>second</p>")
            })
            .await?;

        let (pad2_id, _) = db
            .insert(write::Entry {
                title: Some("todo".to_string()),
                ..pad("\n\n# Second Pad\nSome markdown here")
            })
            .await?;

        // Neither a regular paste nor an asset is a pad.
        db.insert(write::Entry {
            text: "regular paste".to_string(),
            ..Default::default()
        })
        .await?;
        db.insert(write::Entry {
            text: "data:image/png;base64,AQIDBAU=".to_string(),
            is_asset: true,
            ..Default::default()
        })
        .await?;

        let (latest_id, latest_data) = db.get_latest_pad().await?.expect("latest pad exists");
        assert_eq!(latest_id, pad2_id);
        assert_eq!(latest_data.text, "\n\n# Second Pad\nSome markdown here");

        let recents = db.list_recent_pads(10).await?;
        assert_eq!(recents.len(), 2);
        assert_eq!(recents[0].id, pad2_id.to_string());
        assert_eq!(recents[0].title.as_deref(), Some("todo"));
        assert_eq!(recents[0].preview.as_deref(), Some("# Second Pad"));
        assert!(
            recents[0]
                .updated_at
                .as_deref()
                .is_some_and(|t| t.ends_with('Z'))
        );
        assert_eq!(recents[1].id, pad1_id.to_string());
        assert_eq!(recents[1].title, None);
        assert_eq!(recents[1].preview.as_deref(), Some("Hello rich & world"));

        Ok(())
    }
}
