//! MCP server for agents, mounted at `/mcp` unless disabled with `WASTEBIN_MCP=false`.
//!
//! Stateless Streamable HTTP with JSON responses. The tools read and write pads through
//! [`Pads`], so agent writes reach open editors live, and pastes through the [`Database`].

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::http::StatusCode;
use axum::routing::any;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::cache::valid_extension;
use crate::errors::Error;
use crate::pads::{PadError, PadResponse, PadWriteRequest, Pads, new_expiration, parse_pad_id};
use crate::{AppState, Page};
use wastebin_core::db::{self, Database, read::Entry, write};
use wastebin_core::id::Id;
use wastebin_highlight::markdown::{editor_html_to_markdown, markdown_to_editor_html};

/// Routes of the MCP server, merged into the app outside the timeout and compression layers.
/// Disabled, `/mcp` is not found instead of being taken for a paste id.
pub(crate) fn router(state: AppState) -> Router {
    if !state.page.mcp {
        return Router::new().route("/mcp", any(|| async { StatusCode::NOT_FOUND }));
    }

    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        // Open like the rest of the API: checking the Host header would only lock out LAN clients.
        .disable_allowed_hosts()
        .with_max_request_body_bytes(state.page.max_body_size);

    let base_url = std::env::var(wastebin_core::env::vars::BASE_URL)
        .is_ok()
        .then(|| state.page.base_url.clone());
    let mcp = Mcp {
        pads: Pads::from_ref(&state),
        db: state.db,
        page: state.page,
        base_url,
        tool_router: Mcp::tool_router(),
    };
    let service = StreamableHttpService::new(
        move || Ok(mcp.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );

    Router::new().nest_service("/mcp", service)
}

/// Outcome of a tool call: the error branch is reported to the agent as a tool error.
type ToolResult = Result<CallToolResult, CallToolResult>;

#[derive(Clone)]
struct Mcp {
    pads: Pads,
    db: Database,
    page: Page,
    /// `WASTEBIN_BASE_URL` if set; otherwise URLs are built from the request.
    base_url: Option<Url>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct IdArgs {
    /// Id of the pad, paste or file, e.g. `aBc12x`; an extension (`aBc12x.rs`) is ignored.
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreatePadArgs {
    /// Content of the pad. Markdown when `language` is `document`.
    text: String,
    /// Title shown in the pad list.
    title: Option<String>,
    /// `auto` (default), a file extension (`rs`, `md`, …), or `document` for Markdown rich text.
    language: Option<String>,
    /// Seconds until it expires, one of the server's options (`0` = never). Omit for the default.
    expires: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct EditPadArgs {
    /// Id of the pad.
    id: String,
    /// New content, replacing the old. Markdown for document pads.
    text: String,
    /// Revision the new content is based on, from `read_pad` or the last write.
    revision: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppendArgs {
    /// Id of the pad.
    id: String,
    /// Text to add on a new line at the end. Markdown for document pads.
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UploadArgs {
    /// Id of the pad the file belongs to.
    pad: String,
    /// File name. Its extension sets the type: an image (png, jpg, gif, webp, svg, …) or pdf.
    filename: String,
    /// File content, base64 encoded.
    data_base64: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreatePasteArgs {
    /// Content of the paste.
    text: String,
    /// File extension for highlighting (`rs`, `md`, `py`, …). Omit for plain text.
    language: Option<String>,
    /// Seconds until it expires, one of the server's options (`0` = never). Omit for the default.
    expires: Option<u32>,
    /// Delete the paste after it is read once.
    burn: Option<bool>,
    /// Encrypt the paste; readers need this password.
    password: Option<String>,
}

#[tool_router]
impl Mcp {
    /// List the most recently changed pads, newest first.
    #[tool]
    async fn list_pads(&self, Extension(parts): Extension<Parts>) -> ToolResult {
        let base = self.base(&parts);
        let pads: Vec<Value> = self
            .pads
            .list()
            .await
            .map_err(failure)?
            .into_iter()
            .map(|pad| {
                json!({
                    "url": format!("{base}/pad/{}", pad.id),
                    "id": pad.id,
                    "title": pad.title,
                    "preview": pad.preview,
                    "updated_at": pad.updated_at,
                })
            })
            .collect();

        Ok(CallToolResult::structured(json!({ "pads": pads })))
    }

    /// Read a pad: text (Markdown for documents), language, title, revision and files.
    #[tool]
    async fn read_pad(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(IdArgs { id }): Parameters<IdArgs>,
    ) -> ToolResult {
        let base = self.base(&parts);
        let pad = self.pads.get(&id).await.map_err(failure)?;
        let files: Vec<Value> = self
            .pads
            .files(&pad.id)
            .await
            .map_err(failure)?
            .into_iter()
            .map(|file| {
                json!({
                    "id": file.id.to_string(),
                    "name": file.name,
                    "size": file.size,
                    "mime": file.mime(),
                    "url": format!("{base}/raw/{}", file.path()),
                })
            })
            .collect();

        Ok(CallToolResult::structured(json!({
            "url": format!("{base}/pad/{}", pad.id),
            "id": pad.id,
            "title": pad.title,
            "language": language(&pad.extension),
            "revision": pad.revision,
            "text": text_of(&pad),
            "files": files,
        })))
    }

    /// Create a pad, a live document people can open and edit in the browser.
    #[tool]
    async fn create_pad(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<CreatePadArgs>,
    ) -> ToolResult {
        let expires = self.expiration(args.expires)?;
        // `rich` is what documents are stored as: never take it as editor HTML.
        let document = matches!(args.language.as_deref(), Some("document" | "rich"));
        let req = PadWriteRequest {
            preview: Some(preview(&args.text)),
            content: if document {
                markdown_to_editor_html(&args.text, &HashMap::new())
            } else {
                args.text
            },
            extension: if document {
                Some("rich".to_string())
            } else {
                args.language
            },
            title: args.title,
            expires,
            ..Default::default()
        };
        let pad = self.pads.create(req).await.map_err(failure)?;

        Ok(CallToolResult::structured(json!({
            "url": format!("{}/pad/{}", self.base(&parts), pad.id),
            "id": pad.id,
            "revision": pad.revision,
        })))
    }

    /// Replace a pad's text. Fails with its current text and revision if changed since `revision`.
    #[tool]
    async fn edit_pad(&self, Parameters(args): Parameters<EditPadArgs>) -> ToolResult {
        let pad = self.pads.get(&args.id).await.map_err(failure)?;
        let (content, extension) = match pad.extension.as_str() {
            "rich" => {
                let sizes = self.pads.sizes(&pad.id).await.map_err(failure)?;
                (markdown_to_editor_html(&args.text, &sizes), "rich")
            }
            // Keep detecting the language if it was detected, as the editor does.
            _ if pad.auto_detected => (args.text.clone(), "auto"),
            extension => (args.text.clone(), extension),
        };
        let req = PadWriteRequest {
            preview: Some(preview(&args.text)),
            content,
            extension: Some(extension.to_string()),
            base_revision: Some(args.revision),
            ..Default::default()
        };
        let pad = self.pads.update(&pad.id, req).await.map_err(failure)?;

        Ok(CallToolResult::structured(
            json!({ "revision": pad.revision }),
        ))
    }

    /// Append text to a pad on a new line. Needs no revision: edits made meanwhile are kept.
    #[tool]
    async fn append_to_pad(&self, Parameters(args): Parameters<AppendArgs>) -> ToolResult {
        let sizes = self.pads.sizes(&args.id).await.map_err(failure)?;
        let pad = self
            .pads
            .append(&args.id, &args.text, &sizes)
            .await
            .map_err(failure)?;

        Ok(CallToolResult::structured(
            json!({ "revision": pad.revision }),
        ))
    }

    /// Upload an image or PDF and show it at the end of the pad. A code pad becomes a document.
    #[tool]
    async fn upload_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<UploadArgs>,
    ) -> ToolResult {
        let data: String = args.data_base64.split_whitespace().collect();
        let bytes = BASE64
            .decode(data)
            .map_err(|_| message("data_base64 is not valid base64"))?;
        let extension = args.filename.rsplit_once('.').map(|(_, ext)| ext);
        let mime = crate::media::asset_mime(extension);

        let file = self
            .pads
            .upload(Some(&args.pad), mime, Some(&args.filename), &bytes)
            .await
            .map_err(failure)?;
        self.pads.attach(&args.pad, &file).await.map_err(failure)?;

        Ok(CallToolResult::structured(json!({
            "id": file.id.to_string(),
            "url": format!("{}/raw/{}", self.base(&parts), file.path()),
        })))
    }

    /// Download a file: images as image content, PDFs as an embedded resource, text as text.
    #[tool]
    async fn download_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(IdArgs { id }): Parameters<IdArgs>,
    ) -> ToolResult {
        let id = parse_pad_id(&id).map_err(|_| message(NOT_FOUND))?;
        let metadata = self.db.get_metadata(id).await.map_err(db_failure)?;
        let data = self.readable(id, &metadata).await?;

        if !metadata.is_asset {
            return Ok(CallToolResult::success(vec![ContentBlock::text(data.text)]));
        }

        let mime = crate::media::asset_mime(metadata.extension.as_deref());
        let base64 = BASE64.encode(&data.bytes);
        let content = if mime.starts_with("image/") {
            ContentBlock::image(base64, mime)
        } else {
            let url = format!(
                "{}/raw/{id}.{}",
                self.base(&parts),
                metadata.extension.unwrap_or_default()
            );
            ContentBlock::resource(ResourceContents::blob(base64, url).with_mime_type(mime))
        };

        Ok(CallToolResult::success(vec![content]))
    }

    /// Read a paste's text. Refuses, untouched, burn-after-reading and password-protected pastes.
    #[tool]
    async fn read_paste(&self, Parameters(IdArgs { id }): Parameters<IdArgs>) -> ToolResult {
        let id = parse_pad_id(&id).map_err(|_| message(NOT_FOUND))?;
        let metadata = self.db.get_metadata(id).await.map_err(db_failure)?;
        if metadata.is_pad {
            return Err(message("this is a pad: use read_pad"));
        }
        if metadata.is_asset {
            return Err(message("this is a file: use download_file"));
        }
        let data = self.readable(id, &metadata).await?;

        Ok(CallToolResult::success(vec![ContentBlock::text(data.text)]))
    }

    /// Create a paste, an immutable snippet. Returns its URL and the URL of its raw text.
    #[tool]
    async fn create_paste(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<CreatePasteArgs>,
    ) -> ToolResult {
        let expires = new_expiration(&self.page, self.expiration(args.expires)?);
        let extension = args.language.filter(|ext| !ext.is_empty());
        if extension
            .as_deref()
            .is_some_and(|ext| !valid_extension(ext))
        {
            return Err(message("invalid language"));
        }

        let uid = self.db.next_uid().await.map_err(db_failure)?;
        let entry = write::Entry {
            text: args.text,
            extension,
            expires,
            burn_after_reading: args.burn,
            uid: Some(uid),
            password: args.password.filter(|password| !password.is_empty()),
            ..Default::default()
        };
        let (id, entry) = self.db.insert(entry).await.map_err(db_failure)?;
        let path = id.to_url_path(&entry);
        let base = self.base(&parts);

        Ok(CallToolResult::structured(json!({
            "url": format!("{base}/{path}"),
            "raw_url": format!("{base}/raw/{path}"),
        })))
    }
}

#[tool_handler(router = self.tool_router, name = "wastebin")]
impl ServerHandler for Mcp {}

impl Mcp {
    /// Origin the URLs in results start with.
    fn base(&self, parts: &Parts) -> String {
        if let Some(url) = &self.base_url {
            return url.as_str().trim_end_matches('/').to_string();
        }
        let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
        let host = header("host").unwrap_or("localhost");
        let scheme = header("x-forwarded-proto").unwrap_or("http");
        format!("{scheme}://{host}")
    }

    /// `expires` if it is one of the configured expiration options.
    fn expiration(&self, expires: Option<u32>) -> Result<Option<u32>, CallToolResult> {
        let Some(secs) = expires else {
            return Ok(None);
        };
        let options: Vec<u64> = self
            .page
            .expirations
            .iter()
            .map(|expiration| expiration.duration.as_secs())
            .collect();
        if options.contains(&u64::from(secs)) {
            Ok(Some(secs))
        } else {
            let options: Vec<String> = options.iter().map(u64::to_string).collect();
            Err(message(format!(
                "expires must be one of: {}",
                options.join(", ")
            )))
        }
    }

    /// Content of entry `id`, refusing to read what reading would burn or what needs a password.
    async fn readable(
        &self,
        id: Id,
        metadata: &db::read::Metadata,
    ) -> Result<db::read::Data, CallToolResult> {
        if metadata.must_be_deleted {
            return Err(message(
                "burn after reading: refused, reading would delete it",
            ));
        }
        match self.db.get(id, None).await {
            Ok(Entry::Regular(data) | Entry::Burned(data) | Entry::Encrypted(data)) => Ok(data),
            Err(db::Error::NoPassword) => Err(message("password protected: refused")),
            Err(err) => Err(db_failure(err)),
        }
    }
}

const NOT_FOUND: &str = "not found";

/// A tool error saying `text`.
fn message(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

fn db_failure(err: db::Error) -> CallToolResult {
    failure(PadError::from(err))
}

/// The tool error for a failed pad operation. A conflict carries the current pad.
fn failure(err: PadError) -> CallToolResult {
    match err {
        PadError::Invalid(text) => message(text),
        PadError::UnsupportedMedia => message("only images and PDFs can be uploaded"),
        PadError::Conflict(pad) => CallToolResult::structured_error(json!({
            "error": "the pad changed since your revision: merge into text and retry with revision",
            "revision": pad.revision,
            "text": text_of(&pad),
        })),
        PadError::Other(Error::Database(db::Error::NotFound) | Error::Id(_)) => message(NOT_FOUND),
        PadError::Other(err) => message(err.to_string()),
    }
}

/// Content of a loaded pad as agents see it: Markdown for documents.
fn text_of(pad: &PadResponse) -> String {
    let content = pad.content.as_deref().unwrap_or_default();
    if pad.extension == "rich" {
        editor_html_to_markdown(content)
    } else {
        content.to_string()
    }
}

/// Language of a pad as agents name it.
fn language(extension: &str) -> &str {
    if extension == "rich" {
        "document"
    } else {
        extension
    }
}

/// Label of a pad in the pad list, as the editor makes it: the first non-blank line without a
/// Markdown heading, list, task or quote marker.
fn preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    without_marker(line).trim().to_string()
}

fn without_marker(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix('>') {
        return rest.trim_start();
    }
    let hashes = line.len() - line.trim_start_matches('#').len();
    if (1..=6).contains(&hashes)
        && let Some(rest) = after_space(&line[hashes..])
    {
        return rest;
    }
    if let Some(rest) = line.strip_prefix(['-', '*', '+']).and_then(after_space) {
        return ["[ ]", "[x]", "[X]"]
            .iter()
            .find_map(|task| rest.strip_prefix(task).and_then(after_space))
            .unwrap_or(rest);
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(['.', ')'])
            .and_then(after_space)
    {
        return rest;
    }
    line
}

/// `text` after its leading whitespace, if it has any.
fn after_space(text: &str) -> Option<&str> {
    text.starts_with(char::is_whitespace)
        .then(|| text.trim_start())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::test_helpers::{Client, StoreCookies};

    /// Send the JSON-RPC request `method` with `params` and return its result.
    async fn rpc(
        client: &Client,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let res = client
            .post("/mcp")
            .header("accept", "application/json, text/event-stream")
            // Any host is accepted, and URLs are built from it.
            .header("host", "wastebin.lan")
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await?;
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        let body = res.json::<Value>().await?;
        Ok(body["result"].clone())
    }

    async fn call(
        client: &Client,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let result = rpc(
            client,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
        .await?;
        assert_eq!(result["isError"], false, "{result}");
        Ok(result["structuredContent"].clone())
    }

    #[tokio::test]
    async fn smoke() -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(StoreCookies(false)).await;

        let init = rpc(
            &client,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" },
            }),
        )
        .await?;
        assert_eq!(init["serverInfo"]["name"], "wastebin");

        let tools = rpc(&client, "tools/list", json!({})).await?;
        let mut names: Vec<&str> = tools["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "append_to_pad",
                "create_pad",
                "create_paste",
                "download_file",
                "edit_pad",
                "list_pads",
                "read_pad",
                "read_paste",
                "upload_file",
            ]
        );

        let created = call(&client, "create_pad", json!({ "text": "fn main() {}" })).await?;
        let id = created["id"].as_str().expect("id");
        assert_eq!(created["url"], format!("http://wastebin.lan/pad/{id}"));

        let pad = call(&client, "read_pad", json!({ "id": id })).await?;
        assert_eq!(pad["text"], "fn main() {}");
        assert_eq!(pad["revision"], created["revision"]);
        assert_eq!(pad["files"], json!([]));

        Ok(())
    }
}
