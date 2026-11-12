use std::convert::Infallible;
use std::num::NonZeroU32;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::cache::Cache;
use crate::detect::{detect_language, highlight_code};
use crate::errors::{Error, JsonErrorResponse};
use crate::Highlighter;
use wastebin_core::db::read::PadSummary;
use wastebin_core::db::{self, Database, write};
use wastebin_core::id::Id;

/// Default expiration for pads and inline assets: 30 days in seconds.
const DEFAULT_EXPIRY_SECS: u32 = 2_592_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PadResponse {
    pub id: Option<String>,
    pub content: String,
    pub extension: String,
    pub detected_lang: String,
    pub is_code: bool,
    pub highlighted_html: Option<String>,
    pub updated_at: Option<String>,
    pub recent: Vec<PadSummary>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PadWriteRequest {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub expires: Option<u32>,
    #[serde(default)]
    pub client_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AssetUploadRequest {
    pub data_url: String,
    #[serde(default)]
    pub filename: Option<String>,
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

fn resolve_expires(expires: Option<u32>) -> Option<NonZeroU32> {
    match expires {
        None => NonZeroU32::new(DEFAULT_EXPIRY_SECS),
        Some(0) => None,
        Some(secs) => NonZeroU32::new(secs),
    }
}

fn parse_pad_id(raw: &str) -> Result<Id, Error> {
    let base = raw.split_once('.').map_or(raw, |(id, _)| id);
    base.parse::<Id>().map_err(Error::Id)
}

pub async fn get_latest(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cache): State<Cache>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let latest = db.get_latest_pad().await.map_err(Error::Database)?;
    let recent = db.list_recent_pads(20).await.map_err(Error::Database)?;

    if let Some((id, data)) = latest {
        let detected = detect_language(&data.text, data.metadata.extension.as_deref());
        let highlighted_html = if detected.is_code {
            highlight_code(
                highlighter,
                cache,
                Some(id),
                data.text.clone(),
                detected.extension.clone(),
            )
            .await
        } else {
            None
        };
        let id_str = id.to_string();
        let updated_at = recent
            .iter()
            .find(|r| r.id == id_str)
            .and_then(|r| r.updated_at.clone());

        Ok(Json(PadResponse {
            id: Some(id_str),
            content: data.text,
            extension: detected.extension,
            detected_lang: detected.name,
            is_code: detected.is_code,
            highlighted_html,
            updated_at,
            recent,
        }))
    } else {
        Ok(Json(PadResponse {
            id: None,
            content: String::new(),
            extension: "auto".to_string(),
            detected_lang: "Auto".to_string(),
            is_code: false,
            highlighted_html: None,
            updated_at: None,
            recent,
        }))
    }
}

pub async fn get_pad(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cache): State<Cache>,
    Path(id_str): Path<String>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let id = parse_pad_id(&id_str)?;
    let entry = db.get(id, None).await.map_err(Error::Database)?;
    let data = match entry {
        db::read::Entry::Regular(d)
        | db::read::Entry::Burned(d)
        | db::read::Entry::Encrypted(d) => d,
    };

    let recent = db.list_recent_pads(20).await.map_err(Error::Database)?;
    let detected = detect_language(&data.text, data.metadata.extension.as_deref());
    let highlighted_html = if detected.is_code {
        highlight_code(
            highlighter,
            cache,
            Some(id),
            data.text.clone(),
            detected.extension.clone(),
        )
        .await
    } else {
        None
    };

    let clean_id = id.to_string();
    let updated_at = recent
        .iter()
        .find(|r| r.id == clean_id)
        .and_then(|r| r.updated_at.clone());

    Ok(Json(PadResponse {
        id: Some(clean_id),
        content: data.text,
        extension: detected.extension,
        detected_lang: detected.name,
        is_code: detected.is_code,
        highlighted_html,
        updated_at,
        recent,
    }))
}

pub async fn create_pad(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let detected = detect_language(&req.content, req.extension.as_deref());
    let expires = resolve_expires(req.expires);

    let entry = write::Entry {
        text: req.content.clone(),
        extension: Some(detected.extension.clone()),
        expires,
        is_asset: false,
        ..Default::default()
    };

    let (id, _) = db.insert(entry).await.map_err(Error::Database)?;
    let id_str = id.to_string();

    let highlighted_html = if detected.is_code {
        highlight_code(
            highlighter,
            cache,
            Some(id),
            req.content.clone(),
            detected.extension.clone(),
        )
        .await
    } else {
        None
    };

    let recent = db.list_recent_pads(20).await.map_err(Error::Database)?;
    let updated_at = recent
        .iter()
        .find(|r| r.id == id_str)
        .and_then(|r| r.updated_at.clone());

    let event_payload = serde_json::json!({
        "type": "update",
        "id": id_str,
        "content": req.content,
        "extension": detected.extension,
        "detected_lang": detected.name,
        "is_code": detected.is_code,
        "highlighted_html": highlighted_html,
        "client_id": req.client_id,
        "recent": recent,
    })
    .to_string();
    let _ = pad_tx.send(event_payload);

    Ok(Json(PadResponse {
        id: Some(id_str),
        content: req.content,
        extension: detected.extension,
        detected_lang: detected.name,
        is_code: detected.is_code,
        highlighted_html,
        updated_at,
        recent,
    }))
}

pub async fn update_pad(
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Path(id_str): Path<String>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, JsonErrorResponse> {
    let id = parse_pad_id(&id_str)?;
    let detected = detect_language(&req.content, req.extension.as_deref());
    let expires = resolve_expires(req.expires);

    let entry = write::Entry {
        text: req.content.clone(),
        extension: Some(detected.extension.clone()),
        expires,
        is_asset: false,
        ..Default::default()
    };

    db.update(id, entry).await.map_err(Error::Database)?;
    cache.remove(id);

    let clean_id = id.to_string();
    let highlighted_html = if detected.is_code {
        highlight_code(
            highlighter,
            cache,
            Some(id),
            req.content.clone(),
            detected.extension.clone(),
        )
        .await
    } else {
        None
    };

    let recent = db.list_recent_pads(20).await.map_err(Error::Database)?;
    let updated_at = recent
        .iter()
        .find(|r| r.id == clean_id)
        .and_then(|r| r.updated_at.clone());

    let event_payload = serde_json::json!({
        "type": "update",
        "id": clean_id,
        "content": req.content,
        "extension": detected.extension,
        "detected_lang": detected.name,
        "is_code": detected.is_code,
        "highlighted_html": highlighted_html,
        "client_id": req.client_id,
        "recent": recent,
    })
    .to_string();
    let _ = pad_tx.send(event_payload);

    Ok(Json(PadResponse {
        id: Some(clean_id),
        content: req.content,
        extension: detected.extension,
        detected_lang: detected.name,
        is_code: detected.is_code,
        highlighted_html,
        updated_at,
        recent,
    }))
}

pub async fn delete_pad(
    State(db): State<Database>,
    State(cache): State<Cache>,
    State(pad_tx): State<broadcast::Sender<String>>,
    Path(id_str): Path<String>,
) -> Result<Json<serde_json::Value>, JsonErrorResponse> {
    let id = parse_pad_id(&id_str)?;
    db.delete(id).await.map_err(Error::Database)?;
    cache.remove(id);

    let clean_id = id.to_string();
    let recent = db.list_recent_pads(20).await.map_err(Error::Database)?;

    let event_payload = serde_json::json!({
        "type": "deleted",
        "id": clean_id,
        "recent": recent,
    })
    .to_string();
    let _ = pad_tx.send(event_payload);

    Ok(Json(serde_json::json!({
        "deleted": true,
        "recent": recent,
    })))
}

pub async fn upload_asset(
    State(db): State<Database>,
    Json(req): Json<AssetUploadRequest>,
) -> Result<Json<AssetUploadResponse>, JsonErrorResponse> {
    let expires = resolve_expires(req.expires);
    let approx_bytes = req
        .data_url
        .split_once(";base64,")
        .map_or(req.data_url.len(), |(_, b64)| b64.len() * 3 / 4);

    let entry = write::Entry {
        text: req.data_url,
        expires,
        is_asset: true,
        title: req.filename.clone(),
        ..Default::default()
    };

    let (id, inserted_entry) = db.insert(entry).await.map_err(Error::Database)?;
    let ext = inserted_entry
        .extension
        .unwrap_or_else(|| "bin".to_string());
    let id_str = id.to_string();
    let is_image = crate::media::is_image_ext(Some(&ext));
    let is_pdf = crate::media::is_pdf_ext(Some(&ext));
    let size_formatted = crate::media::format_size(approx_bytes);
    let filename = req
        .filename
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{id_str}.{ext}"));

    Ok(Json(AssetUploadResponse {
        id: id_str.clone(),
        url: format!("/raw/{id_str}.{ext}"),
        dl_url: format!("/dl/{id_str}.{ext}"),
        ext,
        filename,
        size_formatted,
        is_image,
        is_pdf,
    }))
}

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
                Err(RecvError::Lagged(_)) => continue,
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
    use crate::test_helpers::{Client, StoreCookies};
    use reqwest::StatusCode;

    #[tokio::test]
    async fn live_pad_crud_and_asset_isolation() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        // 1. Initially latest pad is empty
        let res = client.get("/api/pad/latest").send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        let latest = res.json::<PadResponse>().await?;
        assert!(latest.id.is_none());
        assert_eq!(latest.content, "");

        // 2. Create a pad via POST /api/pad (auto-detects ABAP)
        let abap_code = "REPORT z_live_pad.\nDATA: lv_msg TYPE string.\nWRITE: / lv_msg.";
        let res = client
            .post("/api/pad")
            .json(&PadWriteRequest {
                content: abap_code.to_string(),
                extension: Some("auto".to_string()),
                expires: None,
                client_id: Some("test-client".to_string()),
            })
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let created = res.json::<PadResponse>().await?;
        let pad_id = created.id.expect("pad id returned");
        assert_eq!(created.extension, "abap");
        assert_eq!(created.detected_lang, "ABAP");
        assert!(created.is_code);
        assert!(created.highlighted_html.is_some());

        // 3. Upload an inline asset via POST /api/asset — must NOT replace the latest pad
        let asset_res = client
            .post("/api/asset")
            .json(&AssetUploadRequest {
                data_url: "data:image/png;base64,AQIDBAU=".to_string(),
                filename: Some("screenshot.png".to_string()),
                expires: None,
            })
            .send()
            .await?;
        assert_eq!(asset_res.status(), StatusCode::OK);
        let asset = asset_res.json::<AssetUploadResponse>().await?;
        assert_eq!(asset.ext, "png");
        assert!(asset.is_image);
        assert!(!asset.is_pdf);
        assert!(asset.url.ends_with(".png"));

        // 4. GET /api/pad/latest still returns our ABAP pad, not the asset
        let latest = client
            .get("/api/pad/latest")
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(latest.id.as_deref(), Some(pad_id.as_str()));
        assert_eq!(latest.content, abap_code);
        assert_eq!(latest.recent.len(), 1);

        // 5. PUT /api/pad/{id} updates the pad in-place (auto-detects TypeScript)
        let ts_code = "export interface Config {\n  readonly port: number;\n  host: string;\n}";
        let updated = client
            .put(&format!("/api/pad/{pad_id}"))
            .json(&PadWriteRequest {
                content: ts_code.to_string(),
                extension: Some("auto".to_string()),
                expires: None,
                client_id: Some("test-client".to_string()),
            })
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(updated.id.as_deref(), Some(pad_id.as_str()));
        assert_eq!(updated.extension, "ts");
        assert_eq!(updated.detected_lang, "TypeScript");
        assert_eq!(updated.content, ts_code);

        // 6. DELETE /api/pad/{id} deletes the pad
        let del_res = client.delete(&format!("/api/pad/{pad_id}")).send().await?;
        assert_eq!(del_res.status(), StatusCode::OK);

        let latest_after = client
            .get("/api/pad/latest")
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert!(latest_after.id.is_none());

        Ok(())
    }
}
