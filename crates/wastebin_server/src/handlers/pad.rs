use std::convert::Infallible;
use std::num::NonZeroU32;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::cache::{Cache, valid_extension};
use crate::detect::{detect_language, highlight_code};
use crate::errors::{Error, JsonError, JsonErrorResponse};
use crate::{Highlighter, Page};
use wastebin_core::db::read::{Data, Metadata, PadSummary};
use wastebin_core::db::{self, Database, write};
use wastebin_core::id::Id;

/// Number of pads listed by `/api/pads`.
const RECENT_PADS: usize = 20;

/// Uploads expire after a day unless a saved pad shows them, which makes them expire with it.
const PENDING_ASSET_EXPIRY: u32 = 86_400;

/// Longest pad label kept for the pad list.
const MAX_PREVIEW_CHARS: usize = 80;

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
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AssetQuery {
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetUploadResponse {
    pub url: String,
    pub dl_url: String,
    pub filename: String,
    pub size_formatted: String,
    pub is_pdf: bool,
}

fn bad_request(status: StatusCode, message: &str) -> JsonErrorResponse {
    (
        status,
        Json(JsonError {
            message: message.to_string(),
        }),
    )
}

/// Expiration for a new pad: `0` never expires, `None` uses the configured default.
fn new_expiration(page: &Page, expires: Option<u32>) -> Option<NonZeroU32> {
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

fn parse_pad_id(raw: &str) -> Result<Id, Error> {
    let base = raw.split_once('.').map_or(raw, |(id, _)| id);
    base.parse::<Id>().map_err(Error::Id)
}

/// Extension to store a pad under: detected for `auto`, otherwise validated.
fn pad_extension(req: &PadWriteRequest) -> Result<String, JsonErrorResponse> {
    let extension = detect_language(&req.content, req.extension.as_deref());
    if valid_extension(&extension) {
        Ok(extension)
    } else {
        Err(bad_request(StatusCode::BAD_REQUEST, "invalid extension"))
    }
}

/// Ids of the uploads a pad's content shows through `/raw/{id}` links.
fn asset_refs(content: &str) -> Vec<Id> {
    let mut ids: Vec<Id> = content
        .match_indices("/raw/")
        .filter_map(|(start, prefix)| {
            let rest = &content[start + prefix.len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+')))
                .unwrap_or(rest.len());
            rest[..end].parse().ok()
        })
        .collect();
    ids.sort_by_key(|id| id.to_i64());
    ids.dedup();
    ids
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

/// A pad loaded into the editor, with its content and highlighting.
async fn loaded(highlighter: Highlighter, id: Id, data: Data) -> PadResponse {
    let extension = data
        .metadata
        .extension
        .clone()
        .unwrap_or_else(|| "txt".to_string());
    let highlighted_html = if extension == "rich" {
        None
    } else {
        highlight_code(highlighter, data.text.clone(), extension.clone()).await
    };

    PadResponse {
        auto_detected: detect_language(&data.text, None) == extension,
        content: Some(data.text),
        highlighted_html,
        ..saved(id, extension, &data.metadata)
    }
}

/// Tell all event stream listeners that pad `id` changed. Sending fails only when nobody listens.
fn broadcast(pad_tx: &broadcast::Sender<String>, kind: &str, id: Id, revision: Option<&str>) {
    let event = serde_json::json!({ "type": kind, "id": id.to_string(), "revision": revision });
    let _ = pad_tx.send(event.to_string());
}

/// GET handler for the list of recent pads.
pub async fn list_pads(
    State(db): State<Database>,
) -> Result<Json<Vec<PadSummary>>, JsonErrorResponse> {
    Ok(Json(
        db.list_recent_pads(RECENT_PADS)
            .await
            .map_err(Error::Database)?,
    ))
}

/// GET handler for the most recently modified pad, `null` if there is none.
pub async fn get_latest(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
) -> Result<Json<Option<PadResponse>>, JsonErrorResponse> {
    Ok(Json(
        match db.get_latest_pad().await.map_err(Error::Database)? {
            Some((id, data)) => Some(loaded(highlighter, id, data).await),
            None => None,
        },
    ))
}

/// GET handler for a pad. Entries that are not pads are reported as not found.
pub async fn get_pad(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    Path(id): Path<String>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let id = parse_pad_id(&id)?;
    let data = db.get_pad(id).await.map_err(Error::Database)?;
    Ok(Json(loaded(highlighter, id, data).await))
}

/// POST handler creating a new pad.
pub async fn create_pad(
    State(db): State<Database>,
    State(page): State<Page>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let extension = pad_extension(&req)?;

    let entry = write::Entry {
        extension: Some(extension.clone()),
        expires: new_expiration(&page, req.expires),
        title: req.title.as_deref().and_then(|t| trimmed(t, usize::MAX)),
        preview: req
            .preview
            .as_deref()
            .and_then(|p| trimmed(p, MAX_PREVIEW_CHARS)),
        is_pad: true,
        text: req.content,
        ..Default::default()
    };
    let assets = asset_refs(&entry.text);

    let (id, _) = db.insert(entry).await.map_err(Error::Database)?;
    db.link_assets(id, assets).await.map_err(Error::Database)?;
    let metadata = db.get_metadata(id).await.map_err(Error::Database)?;

    broadcast(&pad_tx, "created", id, metadata.updated_at.as_deref());
    Ok(Json(saved(id, extension, &metadata)))
}

/// PUT handler replacing the content of a pad. Only the title and expiration present in the
/// request are changed. Entries that are not pads are reported as not found and left alone. A
/// write based on an outdated revision is answered with `409 Conflict` and the current pad.
pub async fn update_pad(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Path(id): Path<String>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Response, JsonErrorResponse> {
    let id = parse_pad_id(&id)?;
    let extension = pad_extension(&req)?;
    let has_base = req.base_revision.is_some();
    let assets = asset_refs(&req.content);

    let update = write::PadUpdate {
        extension: Some(extension.clone()),
        preview: req
            .preview
            .as_deref()
            .and_then(|p| trimmed(p, MAX_PREVIEW_CHARS)),
        title: req.title.as_deref().map(|t| trimmed(t, usize::MAX)),
        expires: req.expires.map(NonZeroU32::new),
        base_revision: req.base_revision,
        text: req.content,
    };

    let metadata = match db.update_pad(id, update).await {
        Ok(metadata) => metadata,
        Err(db::Error::NotFound) if has_base => {
            // Either the pad is gone or someone else wrote first.
            let current = db.get_pad(id).await.map_err(Error::Database)?;
            let pad = loaded(highlighter, id, current).await;
            return Ok((StatusCode::CONFLICT, Json(pad)).into_response());
        }
        Err(err) => return Err(Error::Database(err).into()),
    };

    db.link_assets(id, assets).await.map_err(Error::Database)?;
    // The paste views never cache pads, but drop anything cached for this id before it became
    // mutable.
    cache.remove(id);

    broadcast(&pad_tx, "update", id, metadata.updated_at.as_deref());
    Ok(Json(saved(id, extension, &metadata)).into_response())
}

/// DELETE handler for a pad and the uploads it shows. Entries that are not pads are reported as
/// not found and left alone.
pub async fn delete_pad(
    State(db): State<Database>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Path(id): Path<String>,
) -> Result<StatusCode, JsonErrorResponse> {
    let id = parse_pad_id(&id)?;
    let data = db.get_pad(id).await.map_err(Error::Database)?;

    if !db
        .delete_pad(id, asset_refs(&data.text))
        .await
        .map_err(Error::Database)?
    {
        return Err(Error::Database(db::Error::NotFound).into());
    }

    cache.remove(id);
    broadcast(&pad_tx, "deleted", id, None);
    Ok(StatusCode::NO_CONTENT)
}

/// POST handler storing an image or PDF pasted or dropped into the pad editor. The body is the
/// file itself, typed by its `Content-Type`.
pub async fn upload_asset(
    State(db): State<Database>,
    Query(query): Query<AssetQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AssetUploadResponse>, JsonErrorResponse> {
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let Some(ext) = crate::media::ext_for_mime(mime) else {
        return Err(bad_request(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "only images and PDFs can be uploaded",
        ));
    };
    if body.is_empty() {
        return Err(bad_request(StatusCode::BAD_REQUEST, "empty upload"));
    }

    let title = query.filename.as_deref().and_then(|f| trimmed(f, 255));
    let id = db
        .insert_asset(
            &body,
            ext,
            title.clone(),
            NonZeroU32::new(PENDING_ASSET_EXPIRY),
        )
        .await
        .map_err(Error::Database)?;

    Ok(Json(AssetUploadResponse {
        url: format!("/raw/{id}.{ext}"),
        dl_url: format!("/dl/{id}.{ext}"),
        filename: title.unwrap_or_else(|| format!("{id}.{ext}")),
        size_formatted: crate::media::format_size(body.len()),
        is_pdf: ext == "pdf",
    }))
}

/// GET handler streaming `{type, id, revision}` for every pad change as server-sent events.
pub async fn sse_events(
    State(pad_tx): State<broadcast::Sender<String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = pad_tx.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    let event = Event::default().event("pad_update").data(payload);
                    return Some((Ok(event), rx));
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::insert::form::Entry;
    use crate::test_helpers::{Client, StoreCookies};
    use reqwest::StatusCode;

    fn write(content: &str) -> PadWriteRequest {
        PadWriteRequest {
            content: content.to_string(),
            extension: Some("auto".to_string()),
            ..Default::default()
        }
    }

    async fn create(client: &Client, req: &PadWriteRequest) -> Result<PadResponse, reqwest::Error> {
        client.post("/api/pad").json(req).send().await?.json().await
    }

    #[tokio::test]
    async fn live_pad_crud_and_asset_isolation() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let latest = client.get("/api/pad/latest").send().await?;
        assert!(latest.json::<Option<PadResponse>>().await?.is_none());

        // Create a pad, auto-detecting ABAP. Saves don't send the content back.
        let abap_code = "REPORT z_live_pad.\nDATA: lv_msg TYPE string.\nWRITE: / lv_msg.";
        let created = create(
            &client,
            &PadWriteRequest {
                title: Some("abap".to_string()),
                preview: Some("REPORT z_live_pad.".to_string()),
                expires: Some(3600),
                ..write(abap_code)
            },
        )
        .await?;
        let pad_id = created.id;
        assert_eq!(created.extension, "abap");
        assert_eq!(created.title.as_deref(), Some("abap"));
        assert!(created.expires_in.is_some_and(|secs| secs <= 3600));
        assert!(created.content.is_none() && created.highlighted_html.is_none());

        // An upload never becomes the latest pad.
        client
            .upload_asset(b"\x89PNG", "image/png", "screenshot.png")
            .await?;

        // Loading returns content and highlighting.
        let latest = client
            .get("/api/pad/latest")
            .send()
            .await?
            .json::<Option<PadResponse>>()
            .await?
            .expect("latest pad");
        assert_eq!(latest.id, pad_id);
        assert_eq!(latest.content.as_deref(), Some(abap_code));
        assert!(latest.highlighted_html.is_some());
        assert!(latest.auto_detected);

        let pads = client
            .get("/api/pads")
            .send()
            .await?
            .json::<Vec<PadSummary>>()
            .await?;
        assert_eq!(pads.len(), 1);
        assert_eq!(pads[0].title.as_deref(), Some("abap"));
        assert_eq!(pads[0].preview.as_deref(), Some("REPORT z_live_pad."));

        // Ordinary saves keep the title and the expiration.
        let ts_code = "export interface Config {\n  readonly port: number;\n  host: string;\n}";
        let updated = client
            .put(&format!("/api/pad/{pad_id}"))
            .json(&write(ts_code))
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(updated.extension, "ts");
        assert_eq!(updated.title.as_deref(), Some("abap"));
        assert!(updated.expires_in.is_some());

        // Explicit changes apply.
        let updated = client
            .put(&format!("/api/pad/{pad_id}"))
            .json(&PadWriteRequest {
                title: Some(String::new()),
                expires: Some(0),
                ..write(ts_code)
            })
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(updated.title, None);
        assert_eq!(updated.expires_in, None);

        let res = client.delete(&format!("/api/pad/{pad_id}")).send().await?;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let latest = client.get("/api/pad/latest").send().await?;
        assert!(latest.json::<Option<PadResponse>>().await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn pad_api_refuses_pastes() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let encrypted = Entry {
            text: String::from("secret"),
            password: String::from("hunter2"),
            ..Default::default()
        };
        let burn = Entry {
            text: String::from("burn"),
            burn_after_reading: Some(String::from("on")),
            ..Default::default()
        };
        let plain = Entry {
            text: String::from("plain"),
            ..Default::default()
        };

        let mut ids = Vec::new();
        for entry in [&encrypted, &burn, &plain] {
            let res = client.post_form().form(entry).send().await?;
            let location = res.headers().get("location").unwrap().to_str()?;
            ids.push(
                location
                    .trim_start_matches("/burn")
                    .trim_start_matches('/')
                    .to_string(),
            );
        }
        ids.push(Id::from(12345u32).to_string());

        for id in &ids {
            let url = format!("/api/pad/{id}");
            assert_eq!(
                client.get(&url).send().await?.status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                client
                    .put(&url)
                    .json(&write("clobbered"))
                    .send()
                    .await?
                    .status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                client.delete(&url).send().await?.status(),
                StatusCode::NOT_FOUND
            );
        }

        // The pastes are intact, the burn-after-reading one was not burned, and no pad was
        // created for the unknown id.
        let res = client
            .get(&format!("/raw/{}", ids[0]))
            .header(crate::handlers::extract::PASSWORD_HEADER_NAME, "hunter2")
            .send()
            .await?;
        assert_eq!(res.text().await?, "secret");
        let raw = |id: &str| client.get(&format!("/raw/{id}")).send();
        assert_eq!(raw(&ids[1]).await?.text().await?, "burn");
        assert_eq!(raw(&ids[2]).await?.text().await?, "plain");
        assert_eq!(raw(&ids[3]).await?.status(), StatusCode::NOT_FOUND);

        let pads = client
            .get("/api/pads")
            .send()
            .await?
            .json::<Vec<PadSummary>>()
            .await?;
        assert!(pads.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn invalid_extension_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let bad = PadWriteRequest {
            extension: Some("<script>".to_string()),
            ..write("text")
        };
        let res = client.post("/api/pad").json(&bad).send().await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let created = create(&client, &write("text")).await?;
        let res = client
            .put(&format!("/api/pad/{}", created.id))
            .json(&bad)
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn pad_assets_expire_and_go_with_it() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let url = client
            .upload_asset(b"\x89PNG", "image/png", "a.png")
            .await?;
        let paste = client
            .post_form()
            .form(&Entry {
                text: String::from("unrelated"),
                ..Default::default()
            })
            .send()
            .await?;
        let paste = paste
            .headers()
            .get("location")
            .unwrap()
            .to_str()?
            .to_owned();
        let view = url.replacen("/raw", "", 1);

        // A fresh upload expires unless a pad shows it.
        let html = client.get(&view).send().await?.text().await?;
        assert!(html.contains("expires in"), "html: {html}");

        // A document showing the upload, and (by mistake or malice) linking a regular paste.
        let content = format!("<figure><img src=\"{url}\"></figure><a href=\"/raw{paste}\">x</a>");
        let document = |expires| PadWriteRequest {
            extension: Some("rich".to_string()),
            expires: Some(expires),
            ..write(&content)
        };
        let created = create(&client, &document(0)).await?;

        // The upload now never expires, like its pad.
        let html = client.get(&view).send().await?.text().await?;
        assert!(!html.contains("expires in"), "html: {html}");

        // Changing the pad's expiration changes the upload's.
        client
            .put(&format!("/api/pad/{}", created.id))
            .json(&document(3600))
            .send()
            .await?;
        let html = client.get(&view).send().await?.text().await?;
        assert!(html.contains("expires in"), "html: {html}");

        // Deleting the pad deletes the upload, but not the regular paste.
        let res = client
            .delete(&format!("/api/pad/{}", created.id))
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            client.get(&url).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        let res = client.get(&format!("/raw{paste}")).send().await?;
        assert_eq!(res.text().await?, "unrelated");

        Ok(())
    }

    #[tokio::test]
    async fn events_carry_only_ids_and_revisions() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;
        let mut events = client.get("/api/pad/events").send().await?;

        let created = create(&client, &write("secret content")).await?;

        let mut received = String::new();
        let data = loop {
            let chunk = events.chunk().await?.expect("event stream open");
            received.push_str(std::str::from_utf8(&chunk)?);
            if let Some(data) = received
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
            {
                break data.to_string();
            }
        };

        let event: serde_json::Value = serde_json::from_str(&data)?;
        assert_eq!(event["type"], "created");
        assert_eq!(event["id"], created.id.as_str());
        assert_eq!(event["revision"].as_str(), created.revision.as_deref());
        assert!(!data.contains("secret content"), "event: {data}");

        Ok(())
    }

    #[tokio::test]
    async fn stale_write_conflicts() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let created = create(&client, &write("base")).await?;
        let url = format!("/api/pad/{}", created.id);

        let first = client
            .put(&url)
            .json(&PadWriteRequest {
                base_revision: created.revision.clone(),
                ..write("base + first")
            })
            .send()
            .await?;
        assert_eq!(first.status(), StatusCode::OK);
        let first = first.json::<PadResponse>().await?;
        assert!(first.revision > created.revision);

        // A second writer still based on the creation is refused and told the current state.
        let second = client
            .put(&url)
            .json(&PadWriteRequest {
                base_revision: created.revision.clone(),
                ..write("base + second")
            })
            .send()
            .await?;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        let current = second.json::<PadResponse>().await?;
        assert_eq!(current.content.as_deref(), Some("base + first"));
        assert_eq!(current.revision, first.revision);

        Ok(())
    }

    #[tokio::test]
    async fn asset_upload_rejects_other_types() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let res = client
            .post("/api/asset")
            .header(header::CONTENT_TYPE, "text/html")
            .body("<script>alert(1)</script>")
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        Ok(())
    }

    #[test]
    fn finds_asset_references() {
        let content =
            "<img src=\"/raw/kSuZw254mBk.png\"><a>/raw/+li+CV4q-Hl</a> /raw/kSuZw254mBk.png";
        let ids: Vec<String> = asset_refs(content)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"kSuZw254mBk".to_string()));
        assert!(ids.contains(&"+li+CV4q-Hl".to_string()));
    }
}
