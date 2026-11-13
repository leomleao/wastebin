use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum_extra::headers::HeaderValue;

use crate::cache::Key;
use crate::handlers::NoStore;
use crate::handlers::extract::{Password, Theme};
use crate::handlers::html::{ErrorResponse, FormAction, PasswordInput, make_error};
use crate::i18n::Lang;
use crate::media::asset_mime;
use crate::{Database, Page};
use wastebin_core::db;
use wastebin_core::db::read::{Data, Entry};

/// Content policy for uploaded images: nothing in them may run.
const CSP_IMAGE: HeaderValue = HeaderValue::from_static(
    "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox",
);

/// Content policy for uploaded PDFs. Browsers refuse to run their PDF viewer in a sandboxed
/// document, and the pad editor embeds PDFs in a same-origin frame.
const CSP_PDF: HeaderValue = HeaderValue::from_static("default-src 'none'; frame-ancestors 'self'");

fn raw_response(key: &Key, data: Data) -> Response {
    if !data.metadata.is_asset {
        return data.text.into_response();
    }

    let ext = key.ext.as_deref().or(data.metadata.extension.as_deref());
    let mime = asset_mime(ext);
    let csp = if mime == "application/pdf" {
        CSP_PDF
    } else {
        CSP_IMAGE
    };

    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (header::CONTENT_SECURITY_POLICY, csp),
        ],
        data.bytes,
    )
        .into_response()
}

/// GET handler for raw content of a paste.
pub async fn get(
    Path(id): Path<String>,
    State(db): State<Database>,
    State(page): State<Page>,
    theme: Option<Theme>,
    lang: Lang,
    password: Option<Password>,
) -> Result<Response, ErrorResponse> {
    async {
        let password = password.map(|Password(password)| password);
        let key: Key = id.parse()?;

        match db.get(key.id, password).await {
            Ok(Entry::Regular(data) | Entry::Burned(data)) => Ok(raw_response(&key, data)),
            Ok(Entry::Encrypted(data)) => Ok(raw_response(&key, data).no_store()),
            Err(db::Error::NoPassword) => Ok(PasswordInput {
                page: page.clone(),
                theme: theme.clone(),
                lang,
                action: FormAction::source(&key),
            }
            .into_response()
            .no_store()),
            Err(err) => Err(err.into()),
        }
    }
    .await
    .map_err(|err| make_error(err, page, theme, lang))
}

#[cfg(test)]
mod tests {
    use crate::handlers::extract::PASSWORD_HEADER_NAME;
    use crate::handlers::insert::form::Entry;
    use crate::test_helpers::{Client, StoreCookies};
    use reqwest::StatusCode;
    use reqwest::header;

    #[tokio::test]
    async fn encrypted_raw_is_not_stored() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;
        let password = "hunter2";
        let data = Entry {
            text: String::from("secret-body-xyz"),
            password: password.to_string(),
            ..Default::default()
        };

        let res = client.post_form().form(&data).send().await?;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers().get("location").unwrap().to_str()?.to_owned();

        // Password prompt for the encrypted paste must not be cached either.
        let res = client.get(&format!("/raw{location}")).send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );

        // Decrypted content must not be stored.
        let res = client
            .get(&format!("/raw{location}"))
            .header(PASSWORD_HEADER_NAME, password)
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let cache_control = res.headers().get(header::CACHE_CONTROL).cloned();
        assert_eq!(res.text().await?, "secret-body-xyz");
        assert_eq!(cache_control.unwrap(), "no-store");

        // Plain pastes stay cacheable.
        let data = Entry {
            text: String::from("plain"),
            ..Default::default()
        };

        let res = client.post_form().form(&data).send().await?;
        let location = res.headers().get("location").unwrap().to_str()?.to_owned();

        let res = client.get(&format!("/raw{location}")).send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get(header::CACHE_CONTROL).is_none());
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );

        Ok(())
    }

    #[tokio::test]
    async fn raw_image_and_pdf() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let url = client
            .upload_asset(b"\x89PNG\r\n\x1a\n", "image/png", "photo.png")
            .await?;
        let res = client.get(&url).send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(
            res.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox"
        );
        assert!(!res.bytes().await?.is_empty());

        let url = client
            .upload_asset(b"%PDF-1.4", "application/pdf", "doc.pdf")
            .await?;
        let res = client.get(&url).send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        // No sandbox, so that the browser's PDF viewer runs inside the editor's frame.
        assert_eq!(
            res.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            "default-src 'none'; frame-ancestors 'self'"
        );
        assert!(res.bytes().await?.starts_with(b"%PDF-"));

        Ok(())
    }

    #[tokio::test]
    async fn raw_text_is_never_sniffed() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        for text in [
            "BM looks like a bitmap",
            "<svg onload=alert(1)>",
            "data:image/png;base64,iVBORw0KGgo=",
        ] {
            let data = Entry {
                text: String::from(text),
                ..Default::default()
            };
            let res = client.post_form().form(&data).send().await?;
            let location = res.headers().get("location").unwrap().to_str()?.to_owned();

            let res = client.get(&format!("/raw{location}")).send().await?;
            assert_eq!(
                res.headers().get(header::CONTENT_TYPE).unwrap(),
                "text/plain; charset=utf-8"
            );
            assert_eq!(res.text().await?, text);
        }

        Ok(())
    }
}
