use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::errors::{JsonError, JsonErrorResponse};
use crate::pads::{PadError, Pads};
use wastebin_core::db::read::PadSummary;

pub use crate::pads::{PadResponse, PadWriteRequest};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AssetQuery {
    #[serde(default)]
    pub filename: Option<String>,
    /// Pad the upload belongs to.
    #[serde(default)]
    pub pad: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetUploadResponse {
    pub url: String,
    pub dl_url: String,
    pub filename: String,
    pub size_formatted: String,
    pub is_pdf: bool,
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(JsonError {
            message: message.to_string(),
        }),
    )
        .into_response()
}

impl IntoResponse for PadError {
    fn into_response(self) -> Response {
        match self {
            Self::Invalid(message) => json_error(StatusCode::BAD_REQUEST, message),
            Self::UnsupportedMedia => json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "only images and PDFs can be uploaded",
            ),
            Self::Conflict(pad) => (StatusCode::CONFLICT, Json(pad)).into_response(),
            Self::Busy => json_error(StatusCode::SERVICE_UNAVAILABLE, "pad is busy, retry"),
            Self::Other(err) => JsonErrorResponse::from(err).into_response(),
        }
    }
}

/// GET handler for the list of recent pads.
pub async fn list_pads(State(pads): State<Pads>) -> Result<Json<Vec<PadSummary>>, PadError> {
    Ok(Json(pads.list().await?))
}

/// GET handler for the most recently modified pad, `null` if there is none.
pub async fn get_latest(State(pads): State<Pads>) -> Result<Json<Option<PadResponse>>, PadError> {
    Ok(Json(pads.latest().await?))
}

/// GET handler for a pad. Entries that are not pads are reported as not found.
pub async fn get_pad(
    State(pads): State<Pads>,
    Path(id): Path<String>,
) -> Result<Json<PadResponse>, PadError> {
    Ok(Json(pads.load(&id).await?))
}

/// POST handler creating a new pad. A retry with the same `create_key` updates the pad the key
/// created instead of creating another one.
pub async fn create_pad(
    State(pads): State<Pads>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, PadError> {
    Ok(Json(pads.create(req).await?))
}

/// PUT handler replacing the content of a pad. Only the title and expiration present in the
/// request are changed. Entries that are not pads are reported as not found and left alone. A
/// write based on an outdated revision is answered with `409 Conflict` and the current pad.
pub async fn update_pad(
    State(pads): State<Pads>,
    Path(id): Path<String>,
    Json(req): Json<PadWriteRequest>,
) -> Result<Json<PadResponse>, PadError> {
    Ok(Json(pads.update(&id, req).await?))
}

/// DELETE handler for a pad and the uploads it owns. Entries that are not pads are reported as
/// not found and left alone.
pub async fn delete_pad(
    State(pads): State<Pads>,
    Path(id): Path<String>,
) -> Result<StatusCode, PadError> {
    pads.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST handler storing an image or PDF pasted or dropped into the pad editor. The body is the
/// file itself, typed by its `Content-Type`; `?pad=` names the pad it belongs to.
pub async fn upload_asset(
    State(pads): State<Pads>,
    Query(query): Query<AssetQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AssetUploadResponse>, PadError> {
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let file = pads
        .upload(query.pad.as_deref(), mime, query.filename.as_deref(), &body)
        .await?;

    Ok(Json(AssetUploadResponse {
        url: format!("/raw/{}", file.path()),
        dl_url: format!("/dl/{}", file.path()),
        size_formatted: crate::media::format_size(file.size),
        is_pdf: file.extension == "pdf",
        filename: file.name,
    }))
}

/// GET handler streaming `{type, id, revision}` for every pad change as server-sent events.
pub async fn sse_events(
    State(pads): State<Pads>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = pads.subscribe();
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
    use crate::pads::CREATED;
    use crate::test_helpers::{Client, StoreCookies};
    use reqwest::StatusCode;
    use wastebin_core::id::Id;

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
    async fn assets_belong_to_the_pad_they_were_uploaded_into()
    -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;
        let document = |content: &str| PadWriteRequest {
            extension: Some("rich".to_string()),
            ..write(content)
        };

        let pad_a = create(&client, &document("a")).await?;
        let url = client
            .upload_asset_into(b"\x89PNG", "image/png", "a.png", Some(&pad_a.id))
            .await?;
        let view = url.replacen("/raw", "", 1);

        // Owned uploads don't expire on their own; unowned ones do.
        let html = client.get(&view).send().await?.text().await?;
        assert!(!html.contains("expires in"), "html: {html}");
        let unowned = client
            .upload_asset(b"\x89PNG", "image/png", "b.png")
            .await?;
        let html = client
            .get(&unowned.replacen("/raw", "", 1))
            .send()
            .await?
            .text()
            .await?;
        assert!(html.contains("expires in"), "html: {html}");

        // Pad B showing A's upload can neither extend nor delete it.
        let content = format!("<figure><img src=\"{url}\"></figure>");
        let pad_b = create(
            &client,
            &PadWriteRequest {
                expires: Some(3600),
                ..document(&content)
            },
        )
        .await?;
        let res = client
            .delete(&format!("/api/pad/{}", pad_b.id))
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let html = client.get(&view).send().await?.text().await?;
        assert!(!html.contains("expires in"), "html: {html}");

        // Deleting pad A deletes its upload.
        let res = client
            .delete(&format!("/api/pad/{}", pad_a.id))
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            client.get(&url).send().await?.status(),
            StatusCode::NOT_FOUND
        );

        // Uploading into something that is not a pad is refused.
        let res = client
            .post("/api/asset?pad=aaaaaaaaaaa")
            .header(header::CONTENT_TYPE, "image/png")
            .body(&b"\x89PNG"[..])
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn pad_urls_serve_the_editor() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;
        let created = create(&client, &write("text")).await?;

        let res = client.get(&format!("/pad/{}", created.id)).send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.text().await?.contains("id=\"doc-editor\""));

        Ok(())
    }

    #[tokio::test]
    async fn create_retry_applies_to_the_same_pad() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let key = Some("retry-me".to_string());
        let first = create(
            &client,
            &PadWriteRequest {
                create_key: key.clone(),
                ..write("abc")
            },
        )
        .await?;

        // The retry carries what was typed since, and a title and expiration.
        let again = create(
            &client,
            &PadWriteRequest {
                create_key: key.clone(),
                title: Some("typed meanwhile".to_string()),
                expires: Some(3600),
                ..write("abcdef")
            },
        )
        .await?;
        assert_eq!(again.id, first.id);
        assert!(again.revision > first.revision);
        assert_eq!(again.title.as_deref(), Some("typed meanwhile"));
        assert!(again.expires_in.is_some());

        let pad = client
            .get(&format!("/api/pad/{}", first.id))
            .send()
            .await?
            .json::<PadResponse>()
            .await?;
        assert_eq!(pad.content.as_deref(), Some("abcdef"));

        let other = create(
            &client,
            &PadWriteRequest {
                create_key: Some("another".to_string()),
                ..write("second pad")
            },
        )
        .await?;
        assert_ne!(other.id, first.id);

        let too_long = PadWriteRequest {
            create_key: Some("k".repeat(65)),
            ..write("x")
        };
        let res = client.post("/api/pad").json(&too_long).send().await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let pads = client
            .get("/api/pads")
            .send()
            .await?
            .json::<Vec<PadSummary>>()
            .await?;
        assert_eq!(pads.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn create_cut_off_by_the_timeout_still_records_its_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::with_timeout(StoreCookies(false), Duration::from_millis(200)).await;
        let req = PadWriteRequest {
            create_key: Some("slow-create".to_string()),
            ..write("text")
        };

        // Stall creates until the server has answered 408 and dropped the request.
        let lock = CREATED.lock().await;
        let res = client.post("/api/pad").json(&req).send().await?;
        assert_eq!(res.status(), StatusCode::REQUEST_TIMEOUT);
        drop(lock);
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The create completed anyway, and the retry finds it instead of duplicating it.
        let pads = client
            .get("/api/pads")
            .send()
            .await?
            .json::<Vec<PadSummary>>()
            .await?;
        assert_eq!(pads.len(), 1);
        let retried = create(&client, &req).await?;
        assert_eq!(retried.id, pads[0].id);

        Ok(())
    }

    #[tokio::test]
    async fn missing_preview_keeps_the_label() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let created = create(
            &client,
            &PadWriteRequest {
                preview: Some("label".to_string()),
                title: Some("   ".to_string()),
                ..write("label")
            },
        )
        .await?;
        client
            .put(&format!("/api/pad/{}", created.id))
            .json(&write("label, edited by a client that sends no preview"))
            .send()
            .await?;

        let pads = client
            .get("/api/pads")
            .send()
            .await?
            .json::<Vec<PadSummary>>()
            .await?;
        assert_eq!(pads[0].preview.as_deref(), Some("label"));
        assert_eq!(pads[0].title, None);

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
}
