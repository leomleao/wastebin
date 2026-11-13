//! Pads: live documents edited in the browser and by agents.
//!
//! Every read and write of a pad goes through [`Pads`], for the HTTP pad API and the MCP server
//! alike. Writes drop the cached rendering of the pad and announce the change on the event stream
//! themselves, so that no caller can forget to.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::extract::FromRef;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::cache::{Cache, valid_extension};
use crate::detect::{detect_language, highlight_code};
use crate::errors::Error;
use crate::{AppState, Highlighter, Page};
use wastebin_core::db::read::{Data, Metadata, PadSummary};
use wastebin_core::db::{self, Database, write};
use wastebin_core::id::Id;
use wastebin_highlight::markdown::{
    Uploads, figure_html, markdown_to_editor_html, text_to_editor_html,
};

/// Number of pads listed by [`Pads::list`].
const RECENT_PADS: usize = 20;

/// Uploads that belong to no pad expire after a day. Uploads into a pad are deleted with it.
const UNOWNED_ASSET_EXPIRY: u32 = 86_400;

/// How long the pad created for a `create_key` is remembered.
const CREATE_KEY_TTL: Duration = Duration::from_secs(600);

/// Longest accepted `create_key`.
const MAX_CREATE_KEY_LEN: usize = 64;

/// Longest pad label kept for the pad list.
const MAX_PREVIEW_CHARS: usize = 80;

/// Attempts [`Pads::rewrite`] makes before giving up on a pad that keeps changing under it.
const APPEND_TRIES: usize = 5;

/// Pads created recently, by the `create_key` of the request that created them. An editor that
/// lost the response to a create retries with the same key and gets the same pad back instead of
/// a second one. Memory is enough: losing a response right when the server restarts is rare, and
/// costs no more than a duplicate pad.
pub(crate) static CREATED: LazyLock<tokio::sync::Mutex<HashMap<String, (Id, Instant)>>> =
    LazyLock::new(Default::default);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PadResponse {
    pub id: String,
    pub extension: String,
    /// Whether `extension` is what auto-detection picks for the content, i.e. the editor can
    /// keep detecting the language instead of pinning `extension`. Only set when loading.
    #[serde(default)]
    pub auto_detected: bool,
    pub title: Option<String>,
    /// Seconds until the pad expires, `None` if it never does.
    pub expires_in: Option<u64>,
    /// Revision of the pad. Later revisions compare greater; send it back as `base_revision`.
    pub revision: Option<String>,
    /// Content, only returned when loading a pad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Syntax-highlighted content, only returned when loading a pad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlighted_html: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PadWriteRequest {
    #[serde(default)]
    pub content: String,
    /// `auto`, `rich` for a document, or a file extension.
    #[serde(default)]
    pub extension: Option<String>,
    /// Label in the pad list.
    #[serde(default)]
    pub preview: Option<String>,
    /// On update, `None` keeps the current title and an empty title clears it.
    #[serde(default)]
    pub title: Option<String>,
    /// Expiration in seconds, `0` for never. On update, `None` keeps the current expiration;
    /// on creation it selects the configured default.
    #[serde(default)]
    pub expires: Option<u32>,
    /// On update, the revision the content is based on. If the pad changed since, the update is
    /// refused with `409 Conflict` and the current pad.
    #[serde(default)]
    pub base_revision: Option<String>,
    /// On creation, a key the editor keeps for retries of the same create.
    #[serde(default)]
    pub create_key: Option<String>,
}

/// An image or PDF uploaded into a pad.
#[derive(Clone, Debug)]
pub(crate) struct PadFile {
    pub id: Id,
    /// Stored extension, e.g. `png` or `pdf`.
    pub extension: String,
    /// File name given on upload, else `{id}.{extension}`.
    pub name: String,
    /// Size in bytes.
    pub size: usize,
}

impl PadFile {
    /// Path of the file below `/raw/` and `/dl/`, i.e. `{id}.{extension}`.
    pub fn path(&self) -> String {
        format!("{}.{}", self.id, self.extension)
    }

    /// MIME type the file is served with.
    pub fn mime(&self) -> &'static str {
        crate::media::asset_mime(Some(&self.extension))
    }
}

/// Why a pad operation failed.
#[derive(Debug)]
pub(crate) enum PadError {
    /// The request is malformed; the message says how.
    Invalid(&'static str),
    /// The upload is neither an image nor a PDF.
    UnsupportedMedia,
    /// The write is based on an outdated revision. Carries the current pad, loaded.
    Conflict(Box<PadResponse>),
    /// The pad kept changing while a write retried: try again later.
    Busy,
    /// The content would be larger than the configured maximum body size, carried here, which
    /// the editor could then no longer save.
    TooLarge(usize),
    /// Anything else. An unknown pad, or an entry that is not a pad, is
    /// `Error::Database(db::Error::NotFound)`.
    Other(Error),
}

impl From<Error> for PadError {
    fn from(err: Error) -> Self {
        Self::Other(err)
    }
}

impl From<db::Error> for PadError {
    fn from(err: db::Error) -> Self {
        Self::Other(Error::Database(err))
    }
}

/// Access to pads. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Pads {
    db: Database,
    highlighter: Highlighter,
    cache: Cache,
    page: Page,
    events: broadcast::Sender<String>,
}

impl FromRef<AppState> for Pads {
    fn from_ref(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            highlighter: state.highlighter.clone(),
            cache: state.cache.clone(),
            page: state.page.clone(),
            events: state.pad_tx.clone(),
        }
    }
}

impl Pads {
    /// Recently modified pads, most recent first.
    pub async fn list(&self) -> Result<Vec<PadSummary>, PadError> {
        Ok(self.db.list_recent_pads(RECENT_PADS).await?)
    }

    /// The most recently modified pad, loaded with highlighting, `None` if there is none.
    pub async fn latest(&self) -> Result<Option<PadResponse>, PadError> {
        Ok(match self.db.get_latest_pad().await? {
            Some((id, data)) => Some(self.loaded(id, data, true).await),
            None => None,
        })
    }

    /// Pad `id` with its content but without highlighting. `id` may carry an extension
    /// (`abc.rs`). Entries that are not pads are reported as not found and left untouched.
    pub async fn get(&self, id: &str) -> Result<PadResponse, PadError> {
        let id = parse_pad_id(id)?;
        let data = self.db.get_pad(id).await?;
        Ok(self.loaded(id, data, false).await)
    }

    /// Pad `id` with its content and highlighting, as loaded into the editor. See [`Pads::get`].
    pub async fn load(&self, id: &str) -> Result<PadResponse, PadError> {
        let id = parse_pad_id(id)?;
        let data = self.db.get_pad(id).await?;
        Ok(self.loaded(id, data, true).await)
    }

    /// Create a pad. A retry with the same `create_key` updates the pad the key created instead
    /// of creating another one. Returns the pad without its content.
    pub async fn create(&self, req: PadWriteRequest) -> Result<PadResponse, PadError> {
        let extension = pad_extension(&req)?;
        if req
            .create_key
            .as_ref()
            .is_some_and(|key| key.len() > MAX_CREATE_KEY_LEN)
        {
            return Err(PadError::Invalid("create_key too long"));
        }
        self.check_size(&req.content)?;

        // Finish even when the caller is dropped (timeout, disconnect), so that a pad that was
        // inserted always has its key recorded and a retry cannot duplicate it.
        tokio::spawn(self.clone().insert(req, extension))
            .await
            .map_err(Error::Join)?
    }

    async fn insert(
        self,
        mut req: PadWriteRequest,
        extension: String,
    ) -> Result<PadResponse, PadError> {
        // Held until the pad is recorded, so that a retry racing the original waits for it.
        let mut created = match &req.create_key {
            Some(_) => Some(CREATED.lock().await),
            None => None,
        };

        if let Some(created) = created.as_mut() {
            created.retain(|_, (_, at)| at.elapsed() < CREATE_KEY_TTL);
            let known = req.create_key.as_ref().and_then(|key| created.get(key));
            if let Some(&(id, _)) = known {
                // The client did not see the first response: apply what it sends now.
                req.base_revision = None;
                match self.write(id, pad_update(req.clone(), &extension)).await {
                    Ok(metadata) => return Ok(saved(id, extension, &metadata)),
                    // Deleted since: create it anew.
                    Err(db::Error::NotFound) => {}
                    Err(err) => return Err(err.into()),
                }
            }
        }

        let entry = write::Entry {
            extension: Some(extension.clone()),
            expires: new_expiration(&self.page, req.expires),
            title: req.title.as_deref().and_then(|t| trimmed(t, usize::MAX)),
            preview: req
                .preview
                .as_deref()
                .and_then(|p| trimmed(p, MAX_PREVIEW_CHARS)),
            is_pad: true,
            text: req.content,
            ..Default::default()
        };

        let (id, _) = self.db.insert(entry).await?;
        if let (Some(created), Some(key)) = (created.as_mut(), req.create_key) {
            created.insert(key, (id, Instant::now()));
        }
        drop(created);

        let metadata = self.db.get_metadata(id).await?;
        self.announce("created", id, metadata.updated_at.as_deref());
        Ok(saved(id, extension, &metadata))
    }

    /// Replace the content of pad `id`. Only the title, expiration and label present in `req`
    /// are changed. Entries that are not pads are reported as not found and left alone. A write
    /// based on an outdated `base_revision` fails with [`PadError::Conflict`]. Returns the pad
    /// without its content.
    pub async fn update(&self, id: &str, req: PadWriteRequest) -> Result<PadResponse, PadError> {
        let id = parse_pad_id(id)?;
        let extension = pad_extension(&req)?;
        self.check_size(&req.content)?;
        let has_base = req.base_revision.is_some();

        match self.write(id, pad_update(req, &extension)).await {
            Ok(metadata) => Ok(saved(id, extension, &metadata)),
            Err(db::Error::NotFound) if has_base => Err(self.conflict(id).await),
            Err(err) => Err(err.into()),
        }
    }

    /// Append `text` to pad `id` on a new line, keeping its language, title, expiration and
    /// label. To a document pad, `text` is Markdown and is appended as HTML blocks, converted
    /// with `uploads`. A concurrent write is never lost: see [`Pads::rewrite`]. Returns the pad
    /// without its content.
    pub async fn append(
        &self,
        id: &str,
        text: &str,
        uploads: &Uploads,
    ) -> Result<PadResponse, PadError> {
        self.rewrite(id, |current, extension| {
            let text = if extension == "rich" {
                // The editor shows every newline: blocks need none between them.
                format!("{current}{}", markdown_to_editor_html(text, uploads))
            } else {
                appended(current, text)
            };
            (text, extension.to_string())
        })
        .await
    }

    /// Upload a file into pad `id`, as [`Pads::upload`], and add it at the end of the pad with
    /// the markup the editor gives a pasted file. Like the editor, this turns a code pad into a
    /// document pad. If the pad cannot take it, the file is not kept.
    pub async fn attach(
        &self,
        id: &str,
        mime: &str,
        filename: Option<&str>,
        bytes: &[u8],
    ) -> Result<PadFile, PadError> {
        let file = self.upload(Some(id), mime, filename, bytes).await?;
        let size = crate::media::format_size(file.size);
        let figure = figure_html(&file.path(), &file.name, Some(&size));

        let attached = self
            .rewrite(id, |current, extension| {
                let mut html = if extension == "rich" {
                    current.to_string()
                } else {
                    text_to_editor_html(current)
                };
                html.push_str(&figure);
                html.push_str("<div><br></div>");
                (html, "rich".to_string())
            })
            .await;
        if let Err(err) = attached {
            self.db.delete_many(vec![file.id]).await?;
            return Err(err);
        }

        Ok(file)
    }

    /// Replace the content and extension of pad `id` with `change(content, extension)`, keeping
    /// its title, expiration and label. The write is conditional on the content `change` saw, and
    /// retried on newer content up to a few times before failing with [`PadError::Busy`], so that
    /// a concurrent write is never lost.
    async fn rewrite(
        &self,
        id: &str,
        change: impl Fn(&str, &str) -> (String, String),
    ) -> Result<PadResponse, PadError> {
        let id = parse_pad_id(id)?;

        for _ in 0..APPEND_TRIES {
            let current = self.db.get_pad(id).await?;
            let extension = current.metadata.extension.as_deref().unwrap_or("txt");
            let (text, extension) = change(&current.text, extension);
            self.check_size(&text)?;
            let update = write::PadUpdate {
                extension: Some(extension.clone()),
                preview: None,
                title: None,
                expires: None,
                base_revision: current.metadata.updated_at,
                text,
            };

            match self.write(id, update).await {
                Ok(metadata) => return Ok(saved(id, extension, &metadata)),
                // Changed since it was read, or deleted: the next read tells which.
                Err(db::Error::NotFound) => {}
                Err(err) => return Err(err.into()),
            }
        }

        self.db.get_pad(id).await?;
        Err(PadError::Busy)
    }

    /// Delete pad `id` and the uploads it owns. Entries that are not pads are reported as not
    /// found and left alone.
    pub async fn delete(&self, id: &str) -> Result<(), PadError> {
        let id = parse_pad_id(id)?;

        if !self.db.delete_pad(id).await? {
            return Err(db::Error::NotFound.into());
        }

        self.cache.remove(id);
        self.announce("deleted", id, None);
        Ok(())
    }

    /// Store `bytes`, an image or PDF of MIME type `mime`, owned by pad `owner`: it is deleted
    /// with the pad. Without an owner the upload expires after a day.
    pub async fn upload(
        &self,
        owner: Option<&str>,
        mime: &str,
        filename: Option<&str>,
        bytes: &[u8],
    ) -> Result<PadFile, PadError> {
        let Some(ext) = crate::media::ext_for_mime(mime) else {
            return Err(PadError::UnsupportedMedia);
        };
        if bytes.is_empty() {
            return Err(PadError::Invalid("empty upload"));
        }

        let owner = match owner {
            Some(pad) => {
                let pad = parse_pad_id(pad)?;
                self.db.get_pad(pad).await?;
                Some(pad)
            }
            None => None,
        };
        let expires = owner.map_or(NonZeroU32::new(UNOWNED_ASSET_EXPIRY), |_| None);

        let title = filename.and_then(|f| trimmed(f, 255));
        let id = self
            .db
            .insert_asset(bytes, ext, title.clone(), expires, owner)
            .await?;

        Ok(PadFile {
            id,
            extension: ext.to_string(),
            name: title.unwrap_or_else(|| format!("{id}.{ext}")),
            size: bytes.len(),
        })
    }

    /// The images and PDFs uploaded into pad `id`, oldest first. Empty for an unknown pad.
    pub async fn files(&self, id: &str) -> Result<Vec<PadFile>, PadError> {
        let id = parse_pad_id(id)?;
        let mut files = Vec::new();

        for asset in self.db.list_pad_assets(id).await? {
            // Sizes are not stored: the data is compressed.
            let size = match self.db.get(asset.id, None).await {
                Ok(entry) => match entry {
                    db::read::Entry::Regular(data)
                    | db::read::Entry::Burned(data)
                    | db::read::Entry::Encrypted(data) => data.bytes.len(),
                },
                // Deleted or expired meanwhile.
                Err(db::Error::NotFound) => continue,
                Err(err) => return Err(err.into()),
            };
            let extension = asset.extension.unwrap_or_default();
            files.push(PadFile {
                name: asset
                    .title
                    .unwrap_or_else(|| format!("{}.{extension}", asset.id)),
                id: asset.id,
                extension,
                size,
            });
        }

        Ok(files)
    }

    /// The formatted sizes of the files of pad `id` by path below `/raw/`, as figures show them.
    pub async fn sizes(&self, id: &str) -> Result<HashMap<String, String>, PadError> {
        Ok(self
            .files(id)
            .await?
            .into_iter()
            .map(|file| (file.path(), crate::media::format_size(file.size)))
            .collect())
    }

    /// Refuse `content` the editor could not save: larger than the maximum body size.
    fn check_size(&self, content: &str) -> Result<(), PadError> {
        let max = self.page.max_body_size;
        if content.len() > max {
            return Err(PadError::TooLarge(max));
        }
        Ok(())
    }

    /// Receiver of `{type, id, revision}` JSON for every pad change.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// Write `update` to pad `id`, then drop its cached rendering and announce the change.
    async fn write(&self, id: Id, update: write::PadUpdate) -> Result<Metadata, db::Error> {
        let metadata = self.db.update_pad(id, update).await?;
        // The paste views never cache pads, but drop anything cached for this id before it
        // became mutable.
        self.cache.remove(id);
        self.announce("update", id, metadata.updated_at.as_deref());
        Ok(metadata)
    }

    /// The error for a write to pad `id` that found it changed: the current pad, or not found
    /// if it is gone.
    async fn conflict(&self, id: Id) -> PadError {
        match self.db.get_pad(id).await {
            Ok(current) => PadError::Conflict(Box::new(self.loaded(id, current, true).await)),
            Err(err) => err.into(),
        }
    }

    /// Tell all event stream listeners that pad `id` changed. Sending fails only when nobody
    /// listens.
    fn announce(&self, kind: &str, id: Id, revision: Option<&str>) {
        let event = serde_json::json!({ "type": kind, "id": id.to_string(), "revision": revision });
        let _ = self.events.send(event.to_string());
    }

    /// A pad with its content, and its highlighting if `highlight` is set.
    async fn loaded(&self, id: Id, data: Data, highlight: bool) -> PadResponse {
        let extension = data
            .metadata
            .extension
            .clone()
            .unwrap_or_else(|| "txt".to_string());
        let highlighted_html = if !highlight || extension == "rich" {
            None
        } else {
            highlight_code(
                self.highlighter.clone(),
                data.text.clone(),
                extension.clone(),
            )
            .await
        };

        PadResponse {
            auto_detected: detect_language(&data.text, None) == extension,
            content: Some(data.text),
            highlighted_html,
            ..saved(id, extension, &data.metadata)
        }
    }
}

/// `text` appended to `current` on a new line.
fn appended(current: &str, text: &str) -> String {
    if current.is_empty() || current.ends_with('\n') {
        format!("{current}{text}")
    } else {
        format!("{current}\n{text}")
    }
}

/// Expiration for a new pad: `0` never expires, `None` uses the configured default.
pub(crate) fn new_expiration(page: &Page, expires: Option<u32>) -> Option<NonZeroU32> {
    match expires {
        Some(secs) => NonZeroU32::new(secs),
        None => page
            .expirations
            .iter()
            .find(|expiration| expiration.default)
            .and_then(|expiration| u32::try_from(expiration.duration.as_secs()).ok())
            .and_then(NonZeroU32::new),
    }
}

fn trimmed(text: &str, max_chars: usize) -> Option<String> {
    let text: String = text.trim().chars().take(max_chars).collect();
    (!text.is_empty()).then_some(text)
}

/// Id of an entry from `raw`, which may carry an extension (`abc.rs`).
pub(crate) fn parse_pad_id(raw: &str) -> Result<Id, Error> {
    let base = raw.split_once('.').map_or(raw, |(id, _)| id);
    base.parse::<Id>().map_err(Error::Id)
}

/// Extension to store a pad under: detected for `auto`, otherwise validated.
fn pad_extension(req: &PadWriteRequest) -> Result<String, PadError> {
    let extension = detect_language(&req.content, req.extension.as_deref());
    if valid_extension(&extension) {
        Ok(extension)
    } else {
        Err(PadError::Invalid("invalid extension"))
    }
}

/// A saved pad, without its content.
fn saved(id: Id, extension: String, metadata: &Metadata) -> PadResponse {
    PadResponse {
        id: id.to_string(),
        extension,
        auto_detected: false,
        title: metadata.title.clone(),
        expires_in: metadata
            .expiration
            .as_ref()
            .map(|expiration| expiration.duration.as_secs()),
        revision: metadata.updated_at.clone(),
        content: None,
        highlighted_html: None,
    }
}

/// The changes a write request makes to an existing pad: the content, and the title,
/// expiration and label when present.
fn pad_update(req: PadWriteRequest, extension: &str) -> write::PadUpdate {
    write::PadUpdate {
        extension: Some(extension.to_string()),
        preview: req
            .preview
            .as_deref()
            .map(|p| trimmed(p, MAX_PREVIEW_CHARS)),
        title: req.title.as_deref().map(|t| trimmed(t, usize::MAX)),
        expires: req.expires.map(NonZeroU32::new),
        base_revision: req.base_revision,
        text: req.content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str) -> PadWriteRequest {
        PadWriteRequest {
            content: content.to_string(),
            extension: Some("auto".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn appends_on_a_new_line() {
        assert_eq!(appended("", "a"), "a");
        assert_eq!(appended("a", "b"), "a\nb");
        assert_eq!(appended("a\n", "b"), "a\nb");
        assert_eq!(appended("a", "b\n"), "a\nb\n");
    }

    #[tokio::test]
    async fn append_keeps_the_pad_and_announces_it() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let mut events = pads.subscribe();

        let created = pads
            .create(PadWriteRequest {
                title: Some("notes".to_string()),
                preview: Some("first".to_string()),
                extension: Some("md".to_string()),
                expires: Some(3600),
                ..write("first")
            })
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert!(events.recv().await?.contains("\"created\""));

        let appended = pads
            .append(&created.id, "second", &Uploads::default())
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert!(appended.revision > created.revision);
        assert!(events.recv().await?.contains("\"update\""));

        let pad = pads
            .get(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert_eq!(pad.content.as_deref(), Some("first\nsecond"));
        assert_eq!(pad.extension, "md");
        assert_eq!(pad.title.as_deref(), Some("notes"));
        assert!(pad.expires_in.is_some());
        assert!(pad.highlighted_html.is_none());
        assert_eq!(
            pads.list().await.map_err(|err| format!("{err:?}"))?[0]
                .preview
                .as_deref(),
            Some("first")
        );

        let missing = pads
            .append(&Id::from(12345u32).to_string(), "x", &Uploads::default())
            .await;
        assert!(matches!(
            missing,
            Err(PadError::Other(Error::Database(db::Error::NotFound)))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn append_to_a_document_adds_no_blank_line() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(PadWriteRequest {
                extension: Some("rich".to_string()),
                ..write("<p>first</p>")
            })
            .await
            .map_err(|err| format!("{err:?}"))?;

        for text in ["second", "**third**"] {
            pads.append(&created.id, text, &Uploads::default())
                .await
                .map_err(|err| format!("{err:?}"))?;
        }

        let pad = pads
            .get(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert_eq!(
            pad.content.as_deref(),
            Some("<p>first</p><p>second</p><p><strong>third</strong></p>")
        );

        Ok(())
    }

    #[tokio::test]
    async fn concurrent_appends_and_edits_lose_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(write("start"))
            .await
            .map_err(|err| format!("{err:?}"))?;
        let id = created.id.clone();

        // A person editing from the revision they loaded races agents appending. Every append
        // lands, on top of the edit if the edit went first; otherwise the edit is refused. With
        // fewer writers than tries, no append can run out of tries.
        let edit = {
            let pads = pads.clone();
            let id = id.clone();
            let base = created.revision.clone();
            tokio::spawn(async move {
                pads.update(
                    &id,
                    PadWriteRequest {
                        base_revision: base,
                        ..write("start, edited")
                    },
                )
                .await
                .is_ok()
            })
        };
        let appends = (0..APPEND_TRIES - 1).map(|n| {
            let pads = pads.clone();
            let id = id.clone();
            tokio::spawn(async move {
                pads.append(&id, &format!("line {n}"), &Uploads::default())
                    .await
                    .is_ok()
            })
        });
        let appends = futures_util::future::join_all(appends).await;
        let edited = edit.await?;

        for append in appends {
            assert!(append?, "an append gave up");
        }

        let text = pads
            .get(&id)
            .await
            .map_err(|err| format!("{err:?}"))?
            .content
            .unwrap_or_default();
        for n in 0..APPEND_TRIES - 1 {
            assert_eq!(
                text.matches(&format!("line {n}")).count(),
                1,
                "text: {text}"
            );
        }
        if edited {
            assert!(text.starts_with("start, edited\n"), "text: {text}");
        } else {
            assert!(text.starts_with("start\n"), "text: {text}");
        }

        Ok(())
    }

    #[tokio::test]
    async fn append_conflict_reports_the_current_pad() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(write("base"))
            .await
            .map_err(|err| format!("{err:?}"))?;
        pads.append(&created.id, "more", &Uploads::default())
            .await
            .map_err(|err| format!("{err:?}"))?;

        let stale = pads
            .update(
                &created.id,
                PadWriteRequest {
                    base_revision: created.revision.clone(),
                    ..write("base, rewritten")
                },
            )
            .await;
        let Err(PadError::Conflict(current)) = stale else {
            return Err(format!("expected a conflict, got {stale:?}").into());
        };
        assert_eq!(current.content.as_deref(), Some("base\nmore"));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rewrite_gives_up_on_a_busy_pad() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(write("start"))
            .await
            .map_err(|err| format!("{err:?}"))?;
        let id = parse_pad_id(&created.id)?;

        // Someone else saves between every read and write.
        let result = pads
            .rewrite(&created.id, |current, extension| {
                let save = pads.db.update_pad(
                    id,
                    write::PadUpdate {
                        text: format!("{current}."),
                        ..Default::default()
                    },
                );
                tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(save))
                    .expect("save");
                (format!("{current} lost"), extension.to_string())
            })
            .await;
        assert!(matches!(result, Err(PadError::Busy)), "{result:?}");

        let pad = pads
            .get(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert_eq!(pad.content.as_deref(), Some("start....."));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn attach_keeps_no_file_it_could_not_add() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(write("start"))
            .await
            .map_err(|err| format!("{err:?}"))?;
        let id = parse_pad_id(&created.id)?;

        // Saves in a tight loop make attaching give up.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let savers: Vec<_> = (0..3)
            .map(|_| {
                let (db, stop) = (pads.db.clone(), stop.clone());
                tokio::spawn(async move {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        // Like an editor: based on what it read, so that it loses nothing.
                        let current = db.get_pad(id).await.expect("pad");
                        let update = write::PadUpdate {
                            text: current.text,
                            base_revision: current.metadata.updated_at,
                            ..Default::default()
                        };
                        let saved = db.update_pad(id, update).await;
                        assert!(
                            matches!(saved, Ok(_) | Err(db::Error::NotFound)),
                            "save: {saved:?}"
                        );
                    }
                })
            })
            .collect();
        let attaches = (0..8).map(|n| {
            let pads = pads.clone();
            let id = created.id.clone();
            tokio::spawn(async move {
                let name = format!("{n}.png");
                pads.attach(&id, "image/png", Some(&name), b"\x89PNG").await
            })
        });
        let attaches = futures_util::future::join_all(attaches).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for saver in savers {
            saver.await?;
        }

        let mut added = Vec::new();
        for attach in attaches {
            match attach? {
                Ok(file) => added.push(file.id),
                Err(PadError::Busy) => {}
                Err(err) => return Err(format!("{err:?}").into()),
            }
        }
        assert!(
            added.len() < 8,
            "no attach gave up: the test proves nothing"
        );

        // Exactly the added files are kept, and are in the pad.
        let files = pads
            .files(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        let mut kept: Vec<Id> = files.iter().map(|file| file.id).collect();
        kept.sort_by_key(|id| id.to_i64());
        added.sort_by_key(|id| id.to_i64());
        assert_eq!(kept, added);
        let content = pads
            .get(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?
            .content
            .unwrap_or_default();
        for file in &files {
            assert!(content.contains(&file.path()), "{content}");
        }

        Ok(())
    }

    #[tokio::test]
    async fn appends_and_uploads_stop_at_the_size_limit() -> Result<(), Box<dyn std::error::Error>>
    {
        let pads = Pads::from_ref(&crate::test_helpers::state_with_limit(2048));
        let created = pads
            .create(write("start"))
            .await
            .map_err(|err| format!("{err:?}"))?;
        let line = "a".repeat(1000);

        for _ in 0..2 {
            pads.append(&created.id, &line, &Uploads::default())
                .await
                .map_err(|err| format!("{err:?}"))?;
        }
        let full = pads.append(&created.id, &line, &Uploads::default()).await;
        assert!(matches!(full, Err(PadError::TooLarge(2048))), "{full:?}");
        let attached = pads
            .attach(&created.id, "image/png", Some("a.png"), b"\x89PNG")
            .await;
        assert!(
            matches!(attached, Err(PadError::TooLarge(2048))),
            "{attached:?}"
        );

        // Unchanged, and the file is not kept.
        let pad = pads
            .get(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert_eq!(pad.content.map(|c| c.len()), Some(5 + 2 * 1001));
        assert_eq!(pad.extension, "txt");
        let files = pads
            .files(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert!(files.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn files_list_the_uploads_of_a_pad() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(write("pad"))
            .await
            .map_err(|err| format!("{err:?}"))?;

        let png = pads
            .upload(Some(&created.id), "image/png", Some("shot.png"), b"\x89PNG")
            .await
            .map_err(|err| format!("{err:?}"))?;
        pads.upload(None, "application/pdf", None, b"%PDF")
            .await
            .map_err(|err| format!("{err:?}"))?;

        let files = pads
            .files(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, png.id);
        assert_eq!(files[0].name, "shot.png");
        assert_eq!(files[0].size, 4);
        assert_eq!(files[0].mime(), "image/png");
        assert_eq!(files[0].path(), format!("{}.png", png.id));

        assert!(matches!(
            pads.upload(Some(&created.id), "text/html", None, b"<p>")
                .await,
            Err(PadError::UnsupportedMedia)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn attach_adds_the_file_like_the_editor() -> Result<(), Box<dyn std::error::Error>> {
        let pads = Pads::from_ref(&crate::test_helpers::state());
        let created = pads
            .create(write("a\n\nb<"))
            .await
            .map_err(|err| format!("{err:?}"))?;
        let file = pads
            .attach(&created.id, "image/png", Some("a\"b.png"), b"\x89PNG")
            .await
            .map_err(|err| format!("{err:?}"))?;

        let pad = pads
            .get(&created.id)
            .await
            .map_err(|err| format!("{err:?}"))?;
        assert_eq!(pad.extension, "rich");
        let path = file.path();
        assert_eq!(
            pad.content.as_deref(),
            Some(
                format!(
                    "<div>a</div><div><br></div><div>b&lt;</div>\
                     <figure class=\"inline-asset\" data-kind=\"image\">\
                     <img src=\"/raw/{path}\" alt=\"a&quot;b.png\">\
                     <figcaption>a&quot;b.png · 4 B · <a href=\"/dl/{path}\">download</a>\
                     </figcaption></figure><div><br></div>"
                )
                .as_str()
            )
        );

        Ok(())
    }
}
