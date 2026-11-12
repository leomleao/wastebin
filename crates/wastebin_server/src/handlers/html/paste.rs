use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::SignedCookieJar;
use axum_extra::extract::cookie::Key as CookieKey;
use serde::Deserialize;

use crate::cache::{Key, Mode};
use crate::handlers::extract::{Theme, Uids, verify_owner_token};
use crate::handlers::html::{
    BurnConfirmation, ErrorResponse, FormAction, PasswordInput, make_error,
};
use crate::handlers::{NoStore, uid_cookie};
use crate::i18n::Lang;
use crate::{Cache, Database, Highlighter, Page};
use wastebin_core::crypto::Password;
use wastebin_core::db;
use wastebin_core::db::read::{Data, Entry, Metadata};
use wastebin_core::expiration::Expiration;

/// Magic-link handoff: when a paste was created via the JSON API, the response contains a signed
/// `owner` token. Opening `/<id>?owner=<token>` lets the browser claim ownership of the paste, the
/// server validates the token, appends the uid to the user's signed `uid` cookie, and redirects to
/// the clean paste URL so the token never leaks via Referer.
#[derive(Deserialize, Debug)]
pub(crate) struct OwnerHandoff {
    pub(crate) owner: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct PasteForm {
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default)]
    pub(crate) confirm_burn: Option<String>,
}

/// Paste view showing the formatted paste.
#[derive(Template, WebTemplate)]
#[template(path = "formatted.html")]
pub(crate) struct Paste {
    page: Page,
    key: Key,
    theme: Option<Theme>,
    lang: Lang,
    can_delete: bool,
    /// If the paste still in the database and can be fetched with another request.
    is_available: bool,
    /// Expiration in case it was set.
    expiration: Option<Expiration>,
    html: String,
    title: Option<String>,
    /// Whether the paste's extension identifies it as Markdown, enabling the rendered-view toggle.
    is_markdown: bool,
    is_image: bool,
    is_pdf: bool,
    raw_url: String,
    mime_type: String,
    file_size_formatted: String,
    can_open_raw: bool,
}

/// Return `true` if `ext` identifies a Markdown paste.
pub(crate) fn is_markdown_ext(ext: Option<&str>) -> bool {
    ext.is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

#[expect(clippy::too_many_arguments)]
pub async fn get<E>(
    State(cache): State<Cache>,
    State(page): State<Page>,
    State(db): State<Database>,
    State(highlighter): State<Highlighter>,
    State(cookie_key): State<CookieKey>,
    Path(id): Path<String>,
    Query(handoff): Query<OwnerHandoff>,
    jar: SignedCookieJar,
    uids: Option<Uids>,
    theme: Option<Theme>,
    lang: Lang,
    form: Result<Form<PasteForm>, E>,
) -> Result<Response, ErrorResponse> {
    if let Some(token) = handoff.owner.as_deref()
        && let Some(claimed_uid) = verify_owner_token(&cookie_key, token)
    {
        let mut new_uids = uids
            .as_ref()
            .map(|Uids(list)| list.clone())
            .unwrap_or_default();
        if !new_uids.contains(&claimed_uid) {
            new_uids.push(claimed_uid);
        }
        let cookie = uid_cookie(&new_uids);
        return Ok((jar.add(cookie), Redirect::to(&format!("/{id}"))).into_response());
    }

    async {
        let form = form.ok().map(|Form(form)| form);
        let password = form
            .as_ref()
            .and_then(|form| form.password.as_ref())
            .filter(|password| !password.is_empty())
            .map(|password| Password::from(password.as_bytes().to_vec()));
        let confirmed = form.as_ref().and_then(|form| form.confirm_burn.as_deref()) == Some("1");
        let no_password = password.is_none();
        let key: Key = id.parse()?;
        let metadata = db.get_metadata(key.id).await?;

        if metadata.must_be_deleted && !confirmed {
            return Ok(BurnConfirmation {
                page: page.clone(),
                theme: theme.clone(),
                lang,
                title: metadata.title.clone(),
                action: FormAction::source(&key),
            }
            .into_response());
        }

        let (data, is_available, encrypted) = match db.get(key.id, password).await {
            Ok(Entry::Regular(data)) => (data, true, false),
            Ok(Entry::Burned(data)) => (data, false, false),
            Ok(Entry::Encrypted(data)) => {
                let is_available = !data.metadata.must_be_deleted;
                (data, is_available, true)
            }
            Err(db::Error::NoPassword) => {
                return Ok(PasswordInput {
                    page: page.clone(),
                    theme: theme.clone(),
                    lang,
                    action: FormAction::source(&key),
                }
                .into_response()
                .no_store());
            }
            Err(err) => return Err(err.into()),
        };

        let Data {
            text,
            bytes,
            metadata,
        } = data;
        let Metadata {
            uid: owner_uid,
            title,
            expiration,
            ..
        } = metadata;

        let can_delete = match (uids, owner_uid) {
            (Some(Uids(uids)), Some(owner_uid)) => uids.contains(&owner_uid),
            _ => false,
        };

        let ext = key.ext.as_deref().or(metadata.extension.as_deref());
        let is_image = crate::media::is_image(&bytes, ext);
        let is_pdf = crate::media::is_pdf(&bytes, ext);
        let mime_type = crate::media::detect_mime(&bytes, ext).to_string();
        let file_size_formatted = crate::media::format_size(bytes.len());
        let raw_url = if (is_image || is_pdf) && (!is_available || encrypted) {
            use base64::Engine;
            format!(
                "data:{mime_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            )
        } else {
            format!("/raw/{key}")
        };
        let can_open_raw = is_available && !encrypted && !raw_url.starts_with("data:");

        let html = if is_image || is_pdf {
            String::new()
        } else if let Some(html) = cache.get(&key, Mode::Source) {
            tracing::trace!(?key, "found cached item");
            html.into_inner()
        } else {
            let ext = key.ext.clone().or_else(|| metadata.extension.clone());
            let highlighter = highlighter.clone();
            let html =
                tokio::task::spawn_blocking(move || highlighter.highlight(text, ext)).await??;

            if is_available && no_password {
                tracing::trace!(?key, "cache item");
                cache.put(&key, Mode::Source, html.clone());
            }

            html.into_inner()
        };

        let is_markdown = is_markdown_ext(ext);
        let paste = Paste {
            page: page.clone(),
            key,
            theme: theme.clone(),
            lang,
            can_delete,
            is_available,
            expiration,
            html,
            title,
            is_markdown,
            is_image,
            is_pdf,
            raw_url,
            mime_type,
            file_size_formatted,
            can_open_raw,
        };

        let response = paste.into_response();
        Ok(if encrypted {
            response.no_store()
        } else {
            response
        })
    }
    .await
    .map_err(|err| make_error(err, page, theme, lang))
}

#[cfg(test)]
mod tests {
    use crate::test_helpers::{Client, StoreCookies};
    use reqwest::StatusCode;

    #[tokio::test]
    async fn unknown_paste() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let res = client.get("/000000").send().await?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn password_prompt_keeps_extension() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;
        let data = crate::handlers::insert::form::Entry {
            text: String::from("fn main() {}"),
            extension: Some(String::from("rs")),
            password: String::from("hunter2"),
            ..Default::default()
        };

        let res = client.post_form().form(&data).send().await?;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers().get("location").unwrap().to_str()?.to_owned();
        assert!(location.ends_with(".rs"), "location: {location}");

        // The prompt submits back to the very same paste, extension included.
        let body = client.get(&location).send().await?.text().await?;
        assert!(
            body.contains(&format!("action=\"{location}\"")),
            "body: {body}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn encrypted_paste_is_not_stored() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;
        let password = "hunter2";
        let data = crate::handlers::insert::form::Entry {
            text: String::from("secret-body-xyz"),
            password: password.to_string(),
            ..Default::default()
        };

        let res = client.post_form().form(&data).send().await?;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers().get("location").unwrap().to_str()?.to_owned();

        // The password prompt is not cacheable.
        let res = client.get(&location).send().await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(reqwest::header::CACHE_CONTROL).unwrap(),
            "no-store"
        );

        // The decrypted paste is not cacheable either.
        let res = client
            .post(&location)
            .form(&[("password", password)])
            .header(reqwest::header::ACCEPT, "text/html; charset=utf-8")
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let cache_control = res.headers().get(reqwest::header::CACHE_CONTROL).cloned();
        let body = res.text().await?;
        assert!(body.contains("secret-body-xyz"), "body: {body}");
        assert_eq!(cache_control.unwrap(), "no-store");

        Ok(())
    }

    #[tokio::test]
    async fn paste_renders_image_and_pdf() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let img_entry = crate::handlers::insert::form::Entry {
            text: String::from("data:image/png;base64,iVBORw0KGgo="),
            title: String::from("My Screenshot"),
            ..Default::default()
        };
        let res = client.post_form().form(&img_entry).send().await?;
        let loc = res.headers().get("location").unwrap().to_str()?.to_owned();

        let html = client.get(&loc).send().await?.text().await?;
        assert!(html.contains("paste-image"), "Expected paste-image in html: {html}");
        assert!(html.contains("image-container"), "Expected image-container in html: {html}");
        assert!(html.contains("Open in new tab"), "Expected Open in new tab for available media");

        let pdf_entry = crate::handlers::insert::form::Entry {
            text: String::from("data:application/pdf;base64,JVBERi0xLjQ="),
            title: String::from("My Document"),
            ..Default::default()
        };
        let res = client.post_form().form(&pdf_entry).send().await?;
        let loc = res.headers().get("location").unwrap().to_str()?.to_owned();

        let html = client.get(&loc).send().await?.text().await?;
        assert!(html.contains("pdf-iframe"), "Expected pdf-iframe in html: {html}");
        assert!(html.contains("pdf-container"), "Expected pdf-container in html: {html}");
        assert!(html.contains("Open in new tab"), "Expected Open in new tab for available media");

        Ok(())
    }

    #[tokio::test]
    async fn burned_and_encrypted_media_renders_inline() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        // Burn-after-reading image
        let burn_img = crate::handlers::insert::form::Entry {
            text: String::from("data:image/png;base64,iVBORw0KGgo="),
            burn_after_reading: Some(String::from("on")),
            ..Default::default()
        };
        let res = client.post_form().form(&burn_img).send().await?;
        let loc = res.headers().get("location").unwrap().to_str()?.to_owned();
        let paste_path = loc.strip_prefix("/burn").unwrap();

        // Confirm burn
        let res = client
            .post(paste_path)
            .form(&[("confirm_burn", "1")])
            .header(reqwest::header::ACCEPT, "text/html; charset=utf-8")
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let html = res.text().await?;
        assert!(html.contains("paste-image"), "Expected paste-image in html: {html}");
        assert!(
            html.contains("src=\"data:image/png;base64,iVBORw0KGgo=\""),
            "Expected data URI for burned image in html: {html}"
        );
        assert!(
            !html.contains("Open in new tab"),
            "Expected no Open in new tab link when can_open_raw is false"
        );

        // Burn-after-reading PDF
        let burn_pdf = crate::handlers::insert::form::Entry {
            text: String::from("data:application/pdf;base64,JVBERi0xLjQ="),
            burn_after_reading: Some(String::from("on")),
            ..Default::default()
        };
        let res = client.post_form().form(&burn_pdf).send().await?;
        let loc = res.headers().get("location").unwrap().to_str()?.to_owned();
        let paste_path = loc.strip_prefix("/burn").unwrap();

        let res = client
            .post(paste_path)
            .form(&[("confirm_burn", "1")])
            .header(reqwest::header::ACCEPT, "text/html; charset=utf-8")
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let html = res.text().await?;
        assert!(html.contains("pdf-iframe"), "Expected pdf-iframe in html: {html}");
        assert!(
            html.contains("src=\"data:application/pdf;base64,JVBERi0xLjQ=#toolbar=1\""),
            "Expected data URI for burned pdf in html: {html}"
        );
        assert!(
            !html.contains("Open in new tab"),
            "Expected no Open in new tab link when can_open_raw is false"
        );

        // Encrypted image
        let enc_img = crate::handlers::insert::form::Entry {
            text: String::from("data:image/png;base64,iVBORw0KGgo="),
            password: String::from("hunter2"),
            ..Default::default()
        };
        let res = client.post_form().form(&enc_img).send().await?;
        let loc = res.headers().get("location").unwrap().to_str()?.to_owned();

        let res = client
            .post(&loc)
            .form(&[("password", "hunter2")])
            .header(reqwest::header::ACCEPT, "text/html; charset=utf-8")
            .send()
            .await?;
        let html = res.text().await?;
        assert!(html.contains("paste-image"), "Expected paste-image in html: {html}");
        assert!(
            html.contains("src=\"data:image/png;base64,iVBORw0KGgo=\""),
            "Expected data URI for encrypted image in html: {html}"
        );
        assert!(
            !html.contains("Open in new tab"),
            "Expected no Open in new tab link when can_open_raw is false"
        );

        Ok(())
    }
}
