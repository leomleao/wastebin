use std::convert::Infallible;
use std::num::NonZeroU32;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::cache::Cache;
use crate::detect::{detect_language, highlight_code};
use crate::errors::{Error, JsonError, JsonErrorResponse};
use crate::{Highlighter, Page};
use wastebin_core::db::read::{Metadata, PadSummary};
use wastebin_core::db::{Database, write};
use wastebin_core::id::Id;
use wastebin_highlight::markdown;

/// Number of pads listed by `/api/pads`.
const RECENT_PADS: usize = 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PadResponse {
    pub id: Option<String>,
    pub content: String,
    pub extension: String,
    pub detected_lang: String,
    pub is_code: bool,
    pub highlighted_html: Option<String>,
    pub title: Option<String>,
    /// Seconds until the pad expires, `None` if it never does.
    pub expires_in: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PadWriteRequest {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub extension: Option<String>,
    /// On update, `None` keeps the current title and an empty title clears it.
    #[serde(default)]
    pub title: Option<String>,
    /// Expiration in seconds, `0` for never. On update, `None` keeps the current expiration;
    /// on creation it selects the configured default.
    #[serde(default)]
    pub expires: Option<u32>,
    /// Identifies the writing browser tab, so that it can ignore the echo of its own writes.
    #[serde(default)]
    pub client_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AssetUploadRequest {
    pub data_url: String,
    #[serde(default)]
    pub filename: Option<String>,
    /// Expiration in seconds, `0` for never, `None` for the configured default.
    #[serde(default)]
    pub expires: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetUploadResponse {
    pub id: String,
    pub url: String,
    pub dl_url: String,
    pub ext: String,
    pub filename: String,
    pub size_formatted: String,
    pub is_image: bool,
    pub is_pdf: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenderedMarkdown {
    pub html: String,
}

/// Expiration for a new entry: `0` never expires, `None` uses the configured default.
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

fn normalize_title(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn parse_pad_id(raw: &str) -> Result<Id, Error> {
    let base = raw.split_once('.').map_or(raw, |(id, _)| id);
    base.parse::<Id>().map_err(Error::Id)
}

async fn pad_response(
    highlighter: Highlighter,
    id: Id,
    content: String,
    extension: Option<&str>,
    metadata: &Metadata,
) -> PadResponse {
    let detected = detect_language(&content, extension);
    let highlighted_html = if detected.is_code {
        highlight_code(highlighter, content.clone(), detected.extension.clone()).await
    } else {
        None
    };

    PadResponse {
        id: Some(id.to_string()),
        content,
        extension: detected.extension,
        detected_lang: detected.name,
        is_code: detected.is_code,
        highlighted_html,
        title: metadata.title.clone(),
        expires_in: metadata
            .expiration
            .as_ref()
            .map(|expiration| expiration.duration.as_secs()),
    }
}

/// Notify all event stream listeners. Sending fails only when nobody listens.
fn broadcast(pad_tx: &broadcast::Sender<String>, event: &serde_json::Value) {
    let _ = pad_tx.send(event.to_string());
}

fn update_event(kind: &str, pad: &PadResponse, client_id: Option<&str>) -> serde_json::Value {
    let mut event = serde_json::to_value(pad).unwrap_or_default();
    event["type"] = kind.into();
    event["client_id"] = client_id.into();
    event
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

/// GET handler for the most recently modified pad.
pub async fn get_latest(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let Some((id, data)) = db.get_latest_pad().await.map_err(Error::Database)? else {
        return Ok(Json(PadResponse {
            id: None,
            content: String::new(),
            extension: "auto".to_string(),
            detected_lang: "Auto".to_string(),
            is_code: false,
            highlighted_html: None,
            title: None,
            expires_in: None,
        }));
    };

    let extension = data.metadata.extension.clone();
    Ok(Json(
        pad_response(
            highlighter,
            id,
            data.text,
            extension.as_deref(),
            &data.metadata,
        )
        .await,
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
    let extension = data.metadata.extension.clone();

    Ok(Json(
        pad_response(
            highlighter,
            id,
            data.text,
            extension.as_deref(),
            &data.metadata,
        )
        .await,
    ))
}

/// GET handler for a pad's content rendered as Markdown.
pub async fn render_markdown(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    Path(id): Path<String>,
) -> Result<Json<RenderedMarkdown>, JsonErrorResponse> {
    let id = parse_pad_id(&id)?;
    let data = db.get_pad(id).await.map_err(Error::Database)?;
    let html = tokio::task::spawn_blocking(move || markdown::render(&data.text, &highlighter))
        .await
        .map_err(Error::Join)?
        .map_err(Error::SyntaxHighlighting)?;

    Ok(Json(RenderedMarkdown {
        html: html.into_inner(),
    }))
}

/// POST handler creating a new pad.
pub async fn create_pad(
    State(db): State<Database>,
    State(page): State<Page>,
    State(highlighter): State<Highlighter>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let detected = detect_language(&req.content, req.extension.as_deref());
    let expires = new_expiration(&page, req.expires);
    let title = req.title.as_deref().and_then(normalize_title);

    let entry = write::Entry {
        text: req.content.clone(),
        extension: Some(detected.extension.clone()),
        expires,
        title: title.clone(),
        is_pad: true,
        ..Default::default()
    };

    let (id, _) = db.insert(entry).await.map_err(Error::Database)?;
    let metadata = db.get_metadata(id).await.map_err(Error::Database)?;
    let pad = pad_response(
        highlighter,
        id,
        req.content,
        Some(&detected.extension),
        &metadata,
    )
    .await;

    broadcast(
        &pad_tx,
        &update_event("created", &pad, req.client_id.as_deref()),
    );

    Ok(Json(pad))
}

/// PUT handler replacing the content of a pad. Only the title and expiration present in the
/// request are changed. Entries that are not pads are reported as not found and left alone.
pub async fn update_pad(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Path(id): Path<String>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let id = parse_pad_id(&id)?;
    let detected = detect_language(&req.content, req.extension.as_deref());
    let title = req.title.as_deref().map(normalize_title);
    let expires = req.expires.map(NonZeroU32::new);

    let metadata = db
        .update_pad(
            id,
            &req.content,
            Some(detected.extension.clone()),
            title,
            expires,
        )
        .await
        .map_err(Error::Database)?;

    // The paste views never cache pads, but drop anything cached for this id before it became
    // mutable.
    cache.remove(id);

    let pad = pad_response(
        highlighter,
        id,
        req.content,
        Some(&detected.extension),
        &metadata,
    )
    .await;

    broadcast(
        &pad_tx,
        &update_event("update", &pad, req.client_id.as_deref()),
    );

    Ok(Json(pad))
}

/// DELETE handler for a pad. Entries that are not pads are reported as not found and left alone.
pub async fn delete_pad(
    State(db): State<Database>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Path(id): Path<String>,
) -> Result<StatusCode, JsonErrorResponse> {
    let id = parse_pad_id(&id)?;

    if !db.delete_pad(id).await.map_err(Error::Database)? {
        return Err(Error::Database(wastebin_core::db::Error::NotFound).into());
    }

    cache.remove(id);

    broadcast(
        &pad_tx,
        &serde_json::json!({ "type": "deleted", "id": id.to_string() }),
    );

    Ok(StatusCode::NO_CONTENT)
}

fn asset_error(status: StatusCode, message: &str) -> JsonErrorResponse {
    (
        status,
        Json(JsonError {
            message: message.to_string(),
        }),
    )
}

/// POST handler storing an image or PDF pasted or dropped into the pad editor.
pub async fn upload_asset(
    State(db): State<Database>,
    State(page): State<Page>,
    Json(req): Json<AssetUploadRequest>,
) -> Result<Json<AssetUploadResponse>, JsonErrorResponse> {
    let Some((header, b64)) = req
        .data_url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
    else {
        return Err(asset_error(
            StatusCode::BAD_REQUEST,
            "expected a base64 data URL",
        ));
    };

    if !(header.starts_with("image/") || header == "application/pdf") {
        return Err(asset_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "only images and PDFs can be uploaded",
        ));
    }

    let size = b64.trim().len() / 4 * 3;
    if size > page.max_body_size {
        return Err(asset_error(StatusCode::PAYLOAD_TOO_LARGE, "file too large"));
    }

    let entry = write::Entry {
        text: req.data_url,
        expires: new_expiration(&page, req.expires),
        is_asset: true,
        title: req.filename.as_deref().and_then(normalize_title),
        ..Default::default()
    };

    let (id, entry) = db.insert(entry).await.map_err(Error::Database)?;
    let ext = entry.extension.unwrap_or_else(|| "bin".to_string());
    let id = id.to_string();

    Ok(Json(AssetUploadResponse {
        url: format!("/raw/{id}.{ext}"),
        dl_url: format!("/dl/{id}.{ext}"),
        is_image: crate::media::is_image_ext(Some(&ext)) || ext == "svg",
        is_pdf: crate::media::is_pdf_ext(Some(&ext)),
        filename: entry.title.unwrap_or_else(|| format!("{id}.{ext}")),
        size_formatted: crate::media::format_size(size),
        id,
        ext,
    }))
}

/// GET handler streaming pad changes as server-sent events.
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
            client_id: Some("test-client".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn live_pad_crud_and_asset_isolation() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let latest = client
            .get("/api/pad/latest")
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert!(latest.id.is_none());
        assert_eq!(latest.content, "");

        // Create a pad, auto-detecting ABAP.
        let abap_code = "REPORT z_live_pad.\nDATA: lv_msg TYPE string.\nWRITE: / lv_msg.";
        let res = client
            .post("/api/pad")
            .json(&PadWriteRequest {
                title: Some("abap".to_string()),
                expires: Some(3600),
                ..write(abap_code)
            })
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let created = res.json::<PadResponse>().await?;
        let pad_id = created.id.expect("pad id returned");
        assert_eq!(created.extension, "abap");
        assert!(created.is_code);
        assert!(created.highlighted_html.is_some());
        assert_eq!(created.title.as_deref(), Some("abap"));
        assert!(created.expires_in.is_some_and(|secs| secs <= 3600));

        // An asset never becomes the latest pad.
        client
            .upload_asset("data:image/png;base64,AQIDBAU=", "screenshot.png")
            .await?;

        let latest = client
            .get("/api/pad/latest")
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(latest.id.as_deref(), Some(pad_id.as_str()));
        assert_eq!(latest.content, abap_code);

        let pads = client
            .get("/api/pads")
            .send()
            .await?
            .json::<Vec<PadSummary>>()
            .await?;
        assert_eq!(pads.len(), 1);
        assert_eq!(pads[0].title.as_deref(), Some("abap"));

        // Ordinary saves keep the title and the expiration.
        let ts_code = "export interface Config {\n  readonly port: number;\n  host: string;\n}";
        let updated = client
            .put(&format!("/api/pad/{pad_id}"))
            .json(&write(ts_code))
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(updated.id.as_deref(), Some(pad_id.as_str()));
        assert_eq!(updated.extension, "ts");
        assert_eq!(updated.content, ts_code);
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

        let latest = client
            .get("/api/pad/latest")
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert!(latest.id.is_none());

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
            assert_eq!(
                client.get(&format!("{url}/md")).send().await?.status(),
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
        assert_eq!(
            client
                .get(&format!("/raw/{}", ids[1]))
                .send()
                .await?
                .text()
                .await?,
            "burn"
        );
        assert_eq!(
            client
                .get(&format!("/raw/{}", ids[2]))
                .send()
                .await?
                .text()
                .await?,
            "plain"
        );
        assert_eq!(
            client
                .get(&format!("/raw/{}", ids[3]))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND
        );

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
    async fn document_pad_renders_sanitized() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let created = client
            .post("/api/pad")
            .json(&PadWriteRequest {
                extension: Some("rich".to_string()),
                ..write("<p>Hello &amp; bye</p><img src=\"/raw/x.png\" onerror=\"alert(1)\">")
            })
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(created.extension, "rich");
        assert!(!created.is_code);

        let id = created.id.unwrap();
        let html = client.get(&format!("/{id}")).send().await?.text().await?;
        assert!(html.contains("<p>Hello &amp; bye</p>"), "html: {html}");
        assert!(!html.contains("onerror"), "html: {html}");
        assert!(!html.contains("&lt;p&gt;"), "html: {html}");

        Ok(())
    }

    #[tokio::test]
    async fn pad_markdown_is_rendered() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let created = client
            .post("/api/pad")
            .json(&PadWriteRequest {
                extension: Some("md".to_string()),
                ..write("# Title\n\nSome *text*.\n")
            })
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        let id = created.id.unwrap();

        let rendered = client
            .get(&format!("/api/pad/{id}/md"))
            .send()
            .await?
            .json::<RenderedMarkdown>()
            .await?;
        assert!(
            rendered.html.contains("<h1>Title</h1>"),
            "{}",
            rendered.html
        );
        assert!(rendered.html.contains("<em>text</em>"), "{}", rendered.html);

        Ok(())
    }

    #[tokio::test]
    async fn asset_upload_rejects_text() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let res = client
            .post("/api/asset")
            .json(&AssetUploadRequest {
                data_url: "data:text/html;base64,PHNjcmlwdD4=".to_string(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        Ok(())
    }
}
