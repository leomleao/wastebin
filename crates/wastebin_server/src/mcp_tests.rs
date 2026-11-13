//! Integration tests of the MCP server at `/mcp`, driven over HTTP with raw JSON-RPC requests.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::handlers::extract::verify_owner_token;
use crate::test_helpers::{Client, StoreCookies, state_with, state_with_limit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-test-image";
const PDF: &[u8] = b"%PDF-1.4\n%test\n";

const TOOLS: [&str; 9] = [
    "append_to_pad",
    "create_pad",
    "create_paste",
    "download_file",
    "edit_pad",
    "list_pads",
    "read_pad",
    "read_paste",
    "upload_file",
];

/// Send the JSON-RPC request `method` and return the whole HTTP response.
async fn post_rpc(
    client: &Client,
    method: &str,
    params: &Value,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post("/mcp")
        .header("accept", "application/json, text/event-stream")
        .header("host", "wastebin.lan")
        .json(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }))
        .send()
        .await
}

/// Send the JSON-RPC request `method` and return its result.
async fn rpc(
    client: &Client,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let res = post_rpc(client, method, &params).await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.json::<Value>().await?;
    assert_eq!(body["jsonrpc"], "2.0", "{body}");
    assert_eq!(body["id"], 7, "{body}");
    assert!(body.get("error").is_none(), "protocol error: {body}");
    Ok(body["result"].clone())
}

/// Call `tool` and return its result, whether it succeeded or not.
async fn call_raw(
    client: &Client,
    tool: &str,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    rpc(
        client,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
    .await
}

/// Call `tool`, which must succeed, and return its result.
async fn call(
    client: &Client,
    tool: &str,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let result = call_raw(client, tool, arguments).await?;
    assert_ne!(result["isError"], true, "{tool} failed: {result}");
    Ok(result)
}

/// Call `tool`, which must fail with a tool error, and return the error's text.
async fn call_err(
    client: &Client,
    tool: &str,
    arguments: Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let result = call_raw(client, tool, arguments).await?;
    assert_eq!(result["isError"], true, "{tool} did not fail: {result}");
    Ok(content_text(&result))
}

/// All text content of a tool result.
fn content_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The structured result of a tool, falling back to JSON in its text content.
fn structured(result: &Value) -> Value {
    if result["structuredContent"].is_object() {
        return result["structuredContent"].clone();
    }
    serde_json::from_str(&content_text(result)).unwrap_or(Value::Null)
}

async fn create_pad(
    client: &Client,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(structured(&call(client, "create_pad", arguments).await?))
}

async fn read_pad(client: &Client, id: &str) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(structured(
        &call(client, "read_pad", json!({ "id": id })).await?,
    ))
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| unreachable!("no string {key} in {value}"))
}

/// Path of an absolute URL returned by a tool, e.g. `/pad/abc`.
fn path_of(url: &str) -> &str {
    let rest = url
        .strip_prefix("http://wastebin.lan")
        .unwrap_or_else(|| unreachable!("{url} is not built from the request host"));
    assert!(rest.starts_with('/'), "{url}");
    rest
}

async fn client() -> Client {
    Client::new(StoreCookies(false)).await
}

#[tokio::test]
async fn initialize_and_list_exactly_nine_tools() -> TestResult {
    let client = client().await;

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
    assert_eq!(init["serverInfo"]["name"], "wastebin", "{init}");
    assert!(init["protocolVersion"].is_string(), "{init}");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    // Tools only.
    assert!(init["capabilities"].get("resources").is_none(), "{init}");
    assert!(init["capabilities"].get("prompts").is_none(), "{init}");

    let tools = rpc(&client, "tools/list", json!({})).await?;
    let tools = tools["tools"].as_array().expect("tools");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, TOOLS);

    let required = |name: &str| -> Vec<String> {
        let tool = tools.iter().find(|t| t["name"] == name).expect("tool");
        let mut required: Vec<String> = tool["inputSchema"]["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(String::from))
            .collect();
        required.sort();
        required
    };
    for tool in tools {
        let name = str_of(tool, "name");
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(!description.trim().is_empty(), "{name} has no description");
        assert!(description.len() < 200, "{name}: description too long");
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}: {schema}");
        for (arg, prop) in schema["properties"].as_object().into_iter().flatten() {
            let described = prop["description"].as_str().is_some_and(|d| !d.is_empty());
            assert!(described, "{name}.{arg} has no description: {prop}");
        }
    }
    assert_eq!(required("list_pads"), Vec::<String>::new());
    assert_eq!(required("read_pad"), ["id"]);
    assert_eq!(required("create_pad"), ["text"]);
    assert_eq!(required("edit_pad"), ["id", "revision", "text"]);
    assert_eq!(required("append_to_pad"), ["id", "text"]);
    assert_eq!(required("upload_file"), ["data_base64", "filename", "pad"]);
    assert_eq!(required("download_file"), ["id"]);
    assert_eq!(required("read_paste"), ["id"]);
    assert_eq!(required("create_paste"), ["text"]);

    let create_pad = tools
        .iter()
        .find(|t| t["name"] == "create_pad")
        .expect("tool");
    let props = &create_pad["inputSchema"]["properties"];
    for arg in ["text", "title", "language", "expires"] {
        assert!(props[arg].is_object(), "create_pad.{arg}: {props}");
    }
    let language = str_of(&props["language"], "description");
    assert!(
        language.contains("auto") && language.contains("document"),
        "{language}"
    );
    let create_paste = tools
        .iter()
        .find(|t| t["name"] == "create_paste")
        .expect("tool");
    let props = &create_paste["inputSchema"]["properties"];
    for arg in ["text", "language", "expires", "burn", "password"] {
        assert!(props[arg].is_object(), "create_paste.{arg}: {props}");
    }

    Ok(())
}

#[tokio::test]
async fn pad_tools_happy_paths() -> TestResult {
    let client = client().await;

    let created = create_pad(
        &client,
        json!({ "text": "fn main() {}\n", "title": "plan", "language": "rs", "expires": 0 }),
    )
    .await?;
    let id = str_of(&created, "id").to_string();
    assert_eq!(path_of(str_of(&created, "url")), format!("/pad/{id}"));
    let revision = str_of(&created, "revision").to_string();

    // The pad is a real pad: the editor and the pad API see it.
    let res = client.get(&format!("/pad/{id}")).send().await?;
    assert_eq!(res.status(), StatusCode::OK);
    let api: Value = client
        .get(&format!("/api/pad/{id}"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(api["content"], "fn main() {}\n");
    assert_eq!(api["extension"], "rs");
    assert_eq!(api["title"], "plan");

    let pad = read_pad(&client, &id).await?;
    assert_eq!(pad["id"], id.as_str());
    assert_eq!(pad["text"], "fn main() {}\n");
    assert_eq!(pad["language"], "rs");
    assert_eq!(pad["title"], "plan");
    assert_eq!(pad["revision"], revision.as_str());
    assert_eq!(path_of(str_of(&pad, "url")), format!("/pad/{id}"));
    assert_eq!(pad["files"], json!([]));
    assert!(pad.get("highlighted_html").is_none(), "{pad}");
    assert!(
        !pad.to_string().contains("<span"),
        "no highlighted HTML: {pad}"
    );

    // An extension in the id is ignored.
    let same = read_pad(&client, &format!("{id}.rs")).await?;
    assert_eq!(same["text"], "fn main() {}\n");

    let edited = structured(
        &call(
            &client,
            "edit_pad",
            json!({ "id": id, "text": "fn main() { run(); }", "revision": revision }),
        )
        .await?,
    );
    let revision2 = str_of(&edited, "revision").to_string();
    assert_ne!(revision2, revision);

    let appended = structured(
        &call(
            &client,
            "append_to_pad",
            json!({ "id": id, "text": "fn run() {}" }),
        )
        .await?,
    );
    let revision3 = str_of(&appended, "revision").to_string();
    assert_ne!(revision3, revision2);

    let pad = read_pad(&client, &id).await?;
    assert_eq!(pad["text"], "fn main() { run(); }\nfn run() {}");
    assert_eq!(pad["revision"], revision3.as_str());
    // Edits keep the title and language.
    assert_eq!(pad["title"], "plan");
    assert_eq!(pad["language"], "rs");

    // Appending to text that ends with a newline doesn't add a blank line.
    let other = create_pad(&client, json!({ "text": "a\n" })).await?;
    let other_id = str_of(&other, "id");
    call(
        &client,
        "append_to_pad",
        json!({ "id": other_id, "text": "b" }),
    )
    .await?;
    assert_eq!(read_pad(&client, other_id).await?["text"], "a\nb");

    let list = structured(&call(&client, "list_pads", json!({})).await?);
    let pads = list["pads"].as_array().expect("pads");
    assert_eq!(pads.len(), 2, "{list}");
    // Newest first.
    assert_eq!(pads[0]["id"], other_id);
    let entry = pads
        .iter()
        .find(|p| p["id"] == id.as_str())
        .expect("listed");
    assert_eq!(entry["title"], "plan");
    assert!(entry["preview"].is_string(), "{entry}");
    assert!(entry["updated_at"].is_string(), "{entry}");
    assert_eq!(path_of(str_of(entry, "url")), format!("/pad/{id}"));

    Ok(())
}

#[tokio::test]
async fn create_pad_expiry_follows_the_configured_options() -> TestResult {
    let state = state_with("1h=d,1d,0", true);
    let client = Client::with_state(state, StoreCookies(false), Duration::from_secs(30)).await;

    let expires_in = async |id: &str| -> Result<u64, Box<dyn std::error::Error>> {
        let api: Value = client
            .get(&format!("/api/pad/{id}"))
            .send()
            .await?
            .json()
            .await?;
        Ok(api["expires_in"].as_u64().unwrap_or(0))
    };

    // Agents learn the options from the tools.
    let tools = rpc(&client, "tools/list", json!({})).await?;
    for tool in ["create_pad", "create_paste"] {
        let tool = tools["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["name"] == tool)
            .expect("tool");
        let expires = str_of(&tool["inputSchema"]["properties"]["expires"], "description");
        assert_eq!(
            expires,
            "Seconds until it expires, one of: 0 (never), 3600 (default), 86400."
        );
    }

    // The default is the one the UI preselects.
    let pad = create_pad(&client, json!({ "text": "default" })).await?;
    let secs = expires_in(str_of(&pad, "id")).await?;
    assert!((3500..=3600).contains(&secs), "{secs}");

    let pad = create_pad(&client, json!({ "text": "a day", "expires": 86400 })).await?;
    let secs = expires_in(str_of(&pad, "id")).await?;
    assert!((86300..=86400).contains(&secs), "{secs}");

    let pad = create_pad(&client, json!({ "text": "never", "expires": 0 })).await?;
    let api: Value = client
        .get(&format!("/api/pad/{}", str_of(&pad, "id")))
        .send()
        .await?
        .json()
        .await?;
    assert!(api["expires_in"].is_null(), "{api}");

    let err = call_err(&client, "create_pad", json!({ "text": "x", "expires": 5 })).await?;
    assert!(err.contains("3600") && err.contains("86400"), "{err}");

    Ok(())
}

#[tokio::test]
async fn unknown_ids_are_tool_errors() -> TestResult {
    let client = client().await;

    for (tool, args) in [
        ("read_pad", json!({ "id": "zzzzzz" })),
        (
            "edit_pad",
            json!({ "id": "zzzzzz", "text": "x", "revision": "1" }),
        ),
        ("append_to_pad", json!({ "id": "zzzzzz", "text": "x" })),
        (
            "upload_file",
            json!({ "pad": "zzzzzz", "filename": "a.png", "data_base64": BASE64.encode(PNG) }),
        ),
        ("download_file", json!({ "id": "zzzzzz" })),
        ("read_paste", json!({ "id": "zzzzzz" })),
    ] {
        let err = call_err(&client, tool, args).await?;
        assert!(err.contains("not found"), "{tool}: {err}");
    }

    Ok(())
}

#[tokio::test]
async fn stale_edit_returns_the_current_text_and_revision() -> TestResult {
    let client = client().await;

    let created = create_pad(&client, json!({ "text": "base", "language": "txt" })).await?;
    let id = str_of(&created, "id");
    let stale = str_of(&created, "revision");

    // A human saves meanwhile.
    let res = client
        .put(&format!("/api/pad/{id}"))
        .json(&json!({ "content": "base + human", "extension": "txt", "base_revision": stale }))
        .send()
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let current = str_of(&res.json::<Value>().await?, "revision").to_string();

    let result = call_raw(
        &client,
        "edit_pad",
        json!({ "id": id, "text": "base + agent", "revision": stale }),
    )
    .await?;
    assert_eq!(result["isError"], true, "{result}");
    // Clients that only show text content see the current state too.
    let text = content_text(&result);
    assert!(text.contains("base + human"), "{text}");
    assert!(text.contains(&current), "{text}");
    let conflict = structured(&result);
    assert_eq!(conflict["text"], "base + human", "{result}");
    assert_eq!(conflict["revision"], current.as_str(), "{result}");

    // Nothing was written, and retrying with the returned revision succeeds.
    assert_eq!(read_pad(&client, id).await?["text"], "base + human");
    call(
        &client,
        "edit_pad",
        json!({ "id": id, "text": "base + human + agent", "revision": current }),
    )
    .await?;
    assert_eq!(read_pad(&client, id).await?["text"], "base + human + agent");

    // The revision is required.
    let result = post_rpc(
        &client,
        "tools/call",
        &json!({ "name": "edit_pad", "arguments": { "id": id, "text": "no revision" } }),
    )
    .await?
    .json::<Value>()
    .await?;
    let refused = result.get("error").is_some() || result["result"]["isError"] == true;
    assert!(refused, "{result}");
    assert_eq!(read_pad(&client, id).await?["text"], "base + human + agent");

    Ok(())
}

#[tokio::test]
async fn append_concurrent_with_http_puts_loses_nothing() -> TestResult {
    const N: usize = 12;
    let client = std::sync::Arc::new(client().await);

    let created = create_pad(&client, json!({ "text": "start", "language": "txt" })).await?;
    let id = str_of(&created, "id").to_string();

    // A human editor: read, change, save based on what was read; reload on conflict.
    let human = {
        let client = client.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let mut saved = Vec::new();
            for i in 0..N {
                for _ in 0..100 {
                    let pad: Value = client
                        .get(&format!("/api/pad/{id}"))
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    let content = format!("{}\nhuman {i}", pad["content"].as_str().unwrap());
                    let res = client
                        .put(&format!("/api/pad/{id}"))
                        .json(&json!({
                            "content": content,
                            "extension": "txt",
                            "base_revision": pad["revision"],
                        }))
                        .send()
                        .await
                        .unwrap();
                    match res.status() {
                        StatusCode::OK => {
                            saved.push(format!("human {i}"));
                            break;
                        }
                        StatusCode::CONFLICT => tokio::task::yield_now().await,
                        status => unreachable!("PUT failed: {status}"),
                    }
                }
            }
            saved
        })
    };

    let agent = {
        let client = client.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let mut appended = Vec::new();
            let mut failed = 0;
            for i in 0..N {
                let line = format!("agent {i}");
                let result = call_raw(&client, "append_to_pad", json!({ "id": id, "text": line }))
                    .await
                    .unwrap();
                if result["isError"] == true {
                    failed += 1;
                } else {
                    appended.push(line);
                }
            }
            (appended, failed)
        })
    };

    let saved = human.await?;
    let (appended, failed) = agent.await?;
    assert_eq!(saved.len(), N, "human saves: {saved:?}");
    // Appends retry on conflict, so under this load they rarely give up.
    assert!(failed <= N / 4, "{failed} appends failed");

    let text = read_pad(&client, &id).await?["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "start");
    for line in saved.iter().chain(&appended) {
        assert_eq!(
            lines.iter().filter(|l| *l == line).count(),
            1,
            "{line} missing or duplicated in:\n{text}"
        );
    }
    assert_eq!(lines.len(), 1 + saved.len() + appended.len(), "{text}");

    Ok(())
}

#[tokio::test]
async fn paste_tools_round_trip() -> TestResult {
    let client = client().await;

    let created = structured(
        &call(
            &client,
            "create_paste",
            json!({ "text": "fn main() {}", "language": "rs" }),
        )
        .await?,
    );
    let url = path_of(str_of(&created, "url")).to_string();
    let raw_url = path_of(str_of(&created, "raw_url")).to_string();
    assert_eq!(raw_url, format!("/raw{url}"));

    // The normal routes serve it.
    let res = client.get(&raw_url).send().await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await?, "fn main() {}");
    assert_eq!(client.get(&url).send().await?.status(), StatusCode::OK);

    // With or without the extension in the id.
    let id_with_ext = url.trim_start_matches('/');
    let id = id_with_ext.split('.').next().unwrap_or_default();
    for id in [id, id_with_ext] {
        let result = call(&client, "read_paste", json!({ "id": id })).await?;
        assert_eq!(content_text(&result), "fn main() {}", "{result}");
    }

    // A text paste downloads as text.
    let result = call(&client, "download_file", json!({ "id": id })).await?;
    assert_eq!(result["content"][0]["type"], "text", "{result}");
    assert_eq!(content_text(&result), "fn main() {}");

    // Pads are not pastes.
    let pad = create_pad(&client, json!({ "text": "pad" })).await?;
    call_err(&client, "read_paste", json!({ "id": str_of(&pad, "id") })).await?;

    // Invalid languages are refused, like `POST /`.
    call_err(
        &client,
        "create_paste",
        json!({ "text": "x", "language": "../x" }),
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn create_paste_mints_no_owner() -> TestResult {
    let state = state_with("0", true);
    let key = state.key.clone();
    let client = Client::with_state(state, StoreCookies(false), Duration::from_secs(30)).await;

    // The uid of a paste created through `POST /`, read from its owner token.
    let uid = async || -> Result<i64, Box<dyn std::error::Error>> {
        let res: Value = client
            .post_json()
            .json(&json!({ "text": "x" }))
            .send()
            .await?
            .json()
            .await?;
        Ok(verify_owner_token(&key, str_of(&res, "owner")).ok_or("bad owner token")?)
    };

    let before = uid().await?;
    call(
        &client,
        "create_paste",
        json!({ "text": "x", "password": "" }),
    )
    .await?;
    assert_eq!(uid().await?, before + 1);

    Ok(())
}

#[tokio::test]
async fn read_paste_refuses_burn_and_password_pastes_and_leaves_them_intact() -> TestResult {
    let client = client().await;

    // Created through MCP.
    let burn = structured(
        &call(
            &client,
            "create_paste",
            json!({ "text": "burn-secret", "burn": true }),
        )
        .await?,
    );
    let burn_url = path_of(str_of(&burn, "url")).to_string();
    let locked = structured(
        &call(
            &client,
            "create_paste",
            json!({ "text": "locked-secret", "password": "hunter2" }),
        )
        .await?,
    );
    let locked_url = path_of(str_of(&locked, "url")).to_string();

    // Created through the HTTP API.
    let res: Value = client
        .post_json()
        .json(&json!({ "text": "burn-secret-2", "burn_after_reading": true }))
        .send()
        .await?
        .json()
        .await?;
    let http_burn_url = format!("/{}", str_of(&res, "path").trim_start_matches('/'));

    for url in [&burn_url, &locked_url, &http_burn_url] {
        let id = url.trim_start_matches('/');
        // Twice: the first refusal must not have burned it.
        for _ in 0..2 {
            let err = call_err(&client, "read_paste", json!({ "id": id })).await?;
            assert!(!err.contains("secret"), "{err}");
            let err = call_err(&client, "download_file", json!({ "id": id })).await?;
            assert!(!err.contains("secret"), "{err}");
        }
    }

    // Still intact through the normal routes.
    for (url, secret) in [
        (&burn_url, "burn-secret"),
        (&http_burn_url, "burn-secret-2"),
    ] {
        let res = client
            .post(url)
            .form(&[("confirm_burn", "1")])
            .header(reqwest::header::ACCEPT, "text/html")
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.text().await?.contains(secret), "{url} was burned");
    }
    let res = client
        .post(&locked_url)
        .form(&[("password", "hunter2")])
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.text().await?.contains("locked-secret"));

    Ok(())
}

#[tokio::test]
async fn upload_and_download_files() -> TestResult {
    let client = client().await;

    let pad = create_pad(&client, json!({ "text": "notes", "language": "txt" })).await?;
    let pad_id = str_of(&pad, "id");

    let png = structured(
        &call(
            &client,
            "upload_file",
            json!({ "pad": pad_id, "filename": "shot.png", "data_base64": BASE64.encode(PNG) }),
        )
        .await?,
    );
    let png_id = str_of(&png, "id").to_string();
    let png_url = path_of(str_of(&png, "url")).to_string();
    assert!(png_url.starts_with(&format!("/raw/{png_id}")), "{png_url}");
    let res = client.get(&png_url).send().await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/png");
    assert_eq!(res.bytes().await?.as_ref(), PNG);

    // Base64 with line breaks, as some encoders wrap it.
    let wrapped = BASE64
        .encode(PDF)
        .chars()
        .collect::<Vec<_>>()
        .chunks(8)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let pdf = structured(
        &call(
            &client,
            "upload_file",
            json!({ "pad": pad_id, "filename": "paper.pdf", "data_base64": wrapped }),
        )
        .await?,
    );
    let pdf_id = str_of(&pdf, "id").to_string();

    // Both files belong to the pad and sit in its content like pasted files.
    let read = read_pad(&client, pad_id).await?;
    let files = read["files"].as_array().expect("files");
    assert_eq!(files.len(), 2, "{read}");
    assert_eq!(files[0]["id"], png_id.as_str());
    assert_eq!(files[0]["name"], "shot.png");
    assert_eq!(files[0]["size"], PNG.len());
    assert_eq!(files[0]["mime"], "image/png");
    assert_eq!(path_of(str_of(&files[0], "url")), png_url);
    assert_eq!(files[1]["id"], pdf_id.as_str());
    assert_eq!(files[1]["mime"], "application/pdf");
    assert_eq!(read["language"], "document");
    let text = str_of(&read, "text");
    assert!(text.contains("notes"), "{text}");
    assert!(text.contains(&format!("](/raw/{png_id}.png)")), "{text}");
    assert!(text.contains(&format!("](/raw/{pdf_id}.pdf)")), "{text}");
    let api: Value = client
        .get(&format!("/api/pad/{pad_id}"))
        .send()
        .await?
        .json()
        .await?;
    let html = str_of(&api, "content");
    assert!(html.contains("class=\"inline-asset\""), "{html}");
    assert!(
        html.contains(&format!("<img src=\"/raw/{png_id}.png\"")),
        "{html}"
    );
    assert!(
        html.contains(&format!("data-src=\"/raw/{pdf_id}.pdf\"")),
        "{html}"
    );

    // Images download as image content.
    let result = call(&client, "download_file", json!({ "id": png_id })).await?;
    let block = &result["content"][0];
    assert_eq!(block["type"], "image", "{result}");
    assert_eq!(block["mimeType"], "image/png");
    assert_eq!(BASE64.decode(str_of(block, "data"))?, PNG);

    // PDFs as an embedded resource.
    let result = call(
        &client,
        "download_file",
        json!({ "id": format!("{pdf_id}.pdf") }),
    )
    .await?;
    let block = &result["content"][0];
    assert_eq!(block["type"], "resource", "{result}");
    let resource = &block["resource"];
    assert_eq!(resource["mimeType"], "application/pdf", "{result}");
    assert_eq!(BASE64.decode(str_of(resource, "blob"))?, PDF);
    assert!(
        str_of(resource, "uri").ends_with(&format!("/raw/{pdf_id}.pdf")),
        "{result}"
    );

    Ok(())
}

#[tokio::test]
async fn upload_file_rejects_other_types_and_oversize() -> TestResult {
    let client = client().await;

    let pad = create_pad(&client, json!({ "text": "keep", "language": "txt" })).await?;
    let pad_id = str_of(&pad, "id");
    let revision = str_of(&pad, "revision").to_string();

    for filename in ["page.html", "script.js", "notes.txt", "noextension"] {
        let err = call_err(
            &client,
            "upload_file",
            json!({ "pad": pad_id, "filename": filename, "data_base64": BASE64.encode(b"<script>x</script>") }),
        )
        .await?;
        assert!(!err.is_empty(), "{filename}");
    }

    let err = call_err(
        &client,
        "upload_file",
        json!({ "pad": pad_id, "filename": "a.png", "data_base64": "not base64!" }),
    )
    .await?;
    assert!(err.contains("base64"), "{err}");

    // Over the server's body size limit (1 MiB in tests).
    let big = vec![0u8; 1024 * 1024 + 1];
    let err = call_err(
        &client,
        "upload_file",
        json!({ "pad": pad_id, "filename": "big.png", "data_base64": BASE64.encode(&big) }),
    )
    .await?;
    assert!(err.contains("1048576"), "{err}");

    // The pad is untouched.
    let read = read_pad(&client, pad_id).await?;
    assert_eq!(read["files"], json!([]));
    assert_eq!(read["text"], "keep");
    assert_eq!(read["revision"], revision.as_str());

    Ok(())
}

#[tokio::test]
async fn upload_file_takes_what_the_asset_api_takes() -> TestResult {
    const MAX: usize = 4096;
    let client = Client::with_state(
        state_with_limit(MAX),
        StoreCookies(false),
        Duration::from_secs(30),
    )
    .await;
    let pad = create_pad(&client, json!({ "text": "notes", "language": "txt" })).await?;
    let pad_id = str_of(&pad, "id");

    let file = |len: usize| [PNG, &vec![0; len - PNG.len()]].concat();
    let over_http = async |bytes: Vec<u8>| {
        client
            .post("/api/asset")
            .query(&[("pad", pad_id), ("filename", "a.png")])
            .header(reqwest::header::CONTENT_TYPE, "image/png")
            .body(bytes)
            .send()
            .await
            .map(|res| res.status())
    };
    let upload = |bytes: &[u8]| json!({ "pad": pad_id, "filename": "a.png", "data_base64": BASE64.encode(bytes) });

    // At the limit: both take it.
    assert_eq!(over_http(file(MAX)).await?, StatusCode::OK);
    call(&client, "upload_file", upload(&file(MAX))).await?;

    // Over it: both refuse it, MCP with a tool error naming the limit.
    assert_eq!(
        over_http(file(MAX + 1)).await?,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let err = call_err(&client, "upload_file", upload(&file(MAX + 1))).await?;
    assert!(err.contains("4096"), "{err}");

    // Far over it, the request itself is refused.
    let res = post_rpc(
        &client,
        "tools/call",
        &json!({ "name": "upload_file", "arguments": upload(&file(100 * MAX)) }),
    )
    .await?;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);

    Ok(())
}

#[tokio::test]
async fn text_is_held_to_the_body_size_limit() -> TestResult {
    const MAX: usize = 4096;
    let client = Client::with_state(
        state_with_limit(MAX),
        StoreCookies(false),
        Duration::from_secs(30),
    )
    .await;
    let at_limit = "a".repeat(MAX);
    let over = "a".repeat(MAX + 1);

    let pad = create_pad(&client, json!({ "text": at_limit, "language": "txt" })).await?;
    let id = str_of(&pad, "id");
    call(&client, "create_paste", json!({ "text": at_limit })).await?;

    let err = call_err(&client, "create_pad", json!({ "text": over })).await?;
    assert!(err.contains("4096"), "{err}");
    let err = call_err(
        &client,
        "edit_pad",
        json!({ "id": id, "text": over, "revision": pad["revision"] }),
    )
    .await?;
    assert!(err.contains("4096"), "{err}");
    let err = call_err(&client, "create_paste", json!({ "text": over })).await?;
    assert!(err.contains("4096"), "{err}");

    // A document is held to the size of its HTML.
    let markdown = "a".repeat(MAX - 2);
    let err = call_err(
        &client,
        "create_pad",
        json!({ "text": markdown, "language": "document" }),
    )
    .await?;
    assert!(err.contains("4096"), "{err}");

    assert_eq!(read_pad(&client, id).await?["text"], at_limit.as_str());
    let list = structured(&call(&client, "list_pads", json!({})).await?);
    assert_eq!(list["pads"].as_array().map(Vec::len), Some(1), "{list}");

    Ok(())
}

#[tokio::test]
async fn document_pads_round_trip_markdown() -> TestResult {
    let client = client().await;

    let markdown = "# Plan\n\nSome **bold** and *italic* text with `code`.\n\n- one\n- two\n\n1. first\n2. second\n\n> quoted\n\n[link](https://example.com)";
    let created = create_pad(
        &client,
        json!({ "text": markdown, "title": "doc", "language": "document" }),
    )
    .await?;
    let id = str_of(&created, "id");

    // Stored as editor HTML.
    let api: Value = client
        .get(&format!("/api/pad/{id}"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(api["extension"], "rich");
    let html = str_of(&api, "content");
    assert!(html.contains("<h1>Plan</h1>"), "{html}");
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    assert!(!html.contains("**"), "{html}");

    // Read back as the same Markdown.
    let pad = read_pad(&client, id).await?;
    assert_eq!(pad["language"], "document");
    assert_eq!(str_of(&pad, "text").trim_end(), markdown);

    // Editing and appending take Markdown too.
    let edited = format!("{markdown}\n\n## Next\n\n- [ ] ship");
    call(
        &client,
        "edit_pad",
        json!({ "id": id, "text": edited, "revision": pad["revision"] }),
    )
    .await?;
    call(
        &client,
        "append_to_pad",
        json!({ "id": id, "text": "Done **today**." }),
    )
    .await?;
    let pad = read_pad(&client, id).await?;
    let text = str_of(&pad, "text");
    assert!(text.starts_with(markdown), "{text}");
    assert!(text.contains("## Next"), "{text}");
    assert!(text.contains("ship"), "{text}");
    assert!(text.trim_end().ends_with("Done **today**."), "{text}");
    assert_eq!(pad["language"], "document");
    assert_eq!(pad["title"], "doc");

    // An uploaded image survives a read, edit, read cycle as an editor figure.
    let png = structured(
        &call(
            &client,
            "upload_file",
            json!({ "pad": id, "filename": "fig.png", "data_base64": BASE64.encode(PNG) }),
        )
        .await?,
    );
    let png_id = str_of(&png, "id");
    let pad = read_pad(&client, id).await?;
    let text = str_of(&pad, "text").to_string();
    assert!(
        text.contains(&format!("![fig.png](/raw/{png_id}.png)")),
        "{text}"
    );
    call(
        &client,
        "edit_pad",
        json!({ "id": id, "text": format!("{text}\n\nafter"), "revision": pad["revision"] }),
    )
    .await?;
    let api: Value = client
        .get(&format!("/api/pad/{id}"))
        .send()
        .await?
        .json()
        .await?;
    let html = str_of(&api, "content");
    assert!(
        html.contains(&format!("<img src=\"/raw/{png_id}.png\"")) && html.contains("inline-asset"),
        "{html}"
    );
    let again = read_pad(&client, id).await?;
    assert!(str_of(&again, "text").contains(&format!("![fig.png](/raw/{png_id}.png)")));

    // Unsafe HTML in Markdown is sanitized.
    let evil = create_pad(
        &client,
        json!({ "text": "<script>alert(1)</script><img src=x onerror=alert(1)>", "language": "document" }),
    )
    .await?;
    let api: Value = client
        .get(&format!("/api/pad/{}", str_of(&evil, "id")))
        .send()
        .await?
        .json()
        .await?;
    let html = str_of(&api, "content");
    assert!(
        !html.contains("<script") && !html.contains("onerror"),
        "{html}"
    );

    Ok(())
}

#[tokio::test]
async fn noop_edit_leaves_an_uploaded_into_code_pad_unchanged() -> TestResult {
    let client = client().await;

    let code = "fn main() {\n    run();\n}";
    let pad = create_pad(&client, json!({ "text": code, "language": "rs" })).await?;
    let id = str_of(&pad, "id");
    // The upload turns the code pad into a document.
    call(
        &client,
        "upload_file",
        json!({ "pad": id, "filename": "shot.png", "data_base64": BASE64.encode(PNG) }),
    )
    .await?;

    let before = read_pad(&client, id).await?;
    call(
        &client,
        "edit_pad",
        json!({ "id": id, "text": before["text"], "revision": before["revision"] }),
    )
    .await?;
    let after = read_pad(&client, id).await?;
    assert_eq!(after["text"], before["text"]);
    let text = str_of(&after, "text");
    assert!(
        text.starts_with("fn main() {\n\u{a0}\u{a0}\u{a0}\u{a0}run();\n}"),
        "{text}"
    );

    // The figure keeps its size.
    let api: Value = client
        .get(&format!("/api/pad/{id}"))
        .send()
        .await?
        .json()
        .await?;
    let html = str_of(&api, "content");
    assert!(
        html.contains(&format!("shot.png · {} B · ", PNG.len())),
        "{html}"
    );

    Ok(())
}

#[tokio::test]
async fn documents_take_the_absolute_urls_of_uploads() -> TestResult {
    let client = client().await;

    let notes = create_pad(&client, json!({ "text": "notes" })).await?;
    let png = structured(
        &call(
            &client,
            "upload_file",
            json!({ "pad": str_of(&notes, "id"), "filename": "fig.png", "data_base64": BASE64.encode(PNG) }),
        )
        .await?,
    );
    let url = str_of(&png, "url");
    let path = path_of(url);

    let doc = create_pad(
        &client,
        json!({ "text": format!("![x]({url})"), "language": "document" }),
    )
    .await?;
    let id = str_of(&doc, "id");

    let api: Value = client
        .get(&format!("/api/pad/{id}"))
        .send()
        .await?
        .json()
        .await?;
    let html = str_of(&api, "content");
    assert!(html.starts_with("<figure class=\"inline-asset\""), "{html}");
    assert!(html.contains(&format!("<img src=\"{path}\"")), "{html}");
    assert_eq!(
        read_pad(&client, id).await?["text"],
        format!("![x]({path})")
    );

    Ok(())
}

#[tokio::test]
async fn create_pad_takes_rich_as_document() -> TestResult {
    let client = client().await;

    let created = create_pad(
        &client,
        json!({ "text": "# Title\n\n<script>alert(1)</script>", "language": "rich" }),
    )
    .await?;
    let id = str_of(&created, "id");

    let api: Value = client
        .get(&format!("/api/pad/{id}"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(api["extension"], "rich");
    assert_eq!(api["content"], "<h1>Title</h1>");
    assert_eq!(read_pad(&client, id).await?["text"], "# Title");

    Ok(())
}

#[tokio::test]
async fn mcp_writes_emit_pad_update_events() -> TestResult {
    let client = client().await;

    let created = create_pad(&client, json!({ "text": "live" })).await?;
    let id = str_of(&created, "id");

    let mut events = client.get("/api/pad/events").send().await?;
    assert_eq!(events.status(), StatusCode::OK);
    // The stream is subscribed once its headers arrive.
    let appended = structured(
        &call(
            &client,
            "append_to_pad",
            json!({ "id": id, "text": "more" }),
        )
        .await?,
    );

    let mut received = String::new();
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let chunk = events.chunk().await?.expect("event stream open");
            received.push_str(std::str::from_utf8(&chunk)?);
            if received.contains("\n\n") {
                return Ok::<_, Box<dyn std::error::Error>>(received.clone());
            }
        }
    })
    .await
    .map_err(|_| "no event within 5s")??;

    assert!(event.contains("event: pad_update"), "{event}");
    let data = event
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("data");
    let data: Value = serde_json::from_str(data)?;
    assert_eq!(data["type"], "update", "{data}");
    assert_eq!(data["id"], id);
    assert_eq!(data["revision"], appended["revision"]);

    Ok(())
}

/// `WASTEBIN_MCP=false` makes `/mcp` a 404, not a paste id, and hides it from the settings cog.
#[tokio::test]
async fn disabled_mcp_is_not_found() -> TestResult {
    let state = state_with("0", false);
    let client = Client::with_state(state, StoreCookies(false), Duration::from_secs(30)).await;

    let res = post_rpc(&client, "tools/list", &json!({})).await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = client.get("/mcp").send().await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let html = client.get("/").send().await?.text().await?;
    assert!(!html.contains("mcp-url"), "cog shows mcp");
    let html = self::client().await.get("/").send().await?.text().await?;
    assert!(html.contains("mcp-url"), "cog lacks mcp");

    Ok(())
}
