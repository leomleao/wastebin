use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum_extra::extract::cookie::Key;
use reqwest::RequestBuilder;
use tokio::net::TcpListener;

use crate::cache::Cache;
use crate::page;

use wastebin_core::db::{self, Database};
use wastebin_core::expiration::ExpirationSet;
use wastebin_highlight::{Highlighter, Theme};

pub(crate) struct Client {
    client: reqwest::Client,
    addr: SocketAddr,
}

/// Determine if the client should store cookies.
pub(crate) struct StoreCookies(pub bool);

impl Client {
    pub(crate) async fn new(store_cookies: StoreCookies) -> Self {
        Self::with_timeout(store_cookies, Duration::from_secs(30)).await
    }

    /// Server whose requests time out after `timeout`.
    pub(crate) async fn with_timeout(store_cookies: StoreCookies, timeout: Duration) -> Self {
        let (db, handler) = Database::new(db::Open::Memory).expect("open memory database");
        let cache = Cache::new(NonZeroUsize::new(128).unwrap()).unwrap();
        let key = Key::generate();
        let expirations = "0".parse::<ExpirationSet>().unwrap();
        let page = Arc::new(page::Page::new(
            String::from("test"),
            url::Url::parse("https://localhost:8888").unwrap(),
            Theme::Ayu,
            expirations,
            1024 * 1024,
        ));
        let state = crate::AppState {
            db,
            cache,
            key,
            page,
            highlighter: Arc::new(Highlighter::default()),
            pad_tx: tokio::sync::broadcast::channel(128).0,
        };

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Could not bind ephemeral socket");

        let addr = listener.local_addr().unwrap();

        tokio::spawn(handler);

        tokio::spawn(async move {
            let app = crate::make_app(state, timeout, 1024 * 1024);

            axum::serve(listener, app)
                .with_graceful_shutdown(crate::shutdown_signal())
                .await
                .unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(store_cookies.0)
            .build()
            .unwrap();

        Self { client, addr }
    }

    pub(crate) fn get(&self, url: &str) -> RequestBuilder {
        self.client.get(format!("http://{}{}", self.addr, url))
    }

    pub(crate) fn post(&self, url: &str) -> RequestBuilder {
        self.client.post(format!("http://{}{}", self.addr, url))
    }

    pub(crate) fn put(&self, url: &str) -> RequestBuilder {
        self.client.put(format!("http://{}{}", self.addr, url))
    }

    pub(crate) fn post_form(&self) -> RequestBuilder {
        self.client.post(format!("http://{}/new", self.addr))
    }

    pub(crate) fn post_json(&self) -> RequestBuilder {
        self.client.post(format!("http://{}/", self.addr))
    }

    pub(crate) fn delete(&self, url: &str) -> RequestBuilder {
        self.client.delete(format!("http://{}{}", self.addr, url))
    }

    /// Upload an asset through the pad API and return its `/raw/` URL.
    pub(crate) async fn upload_asset(
        &self,
        bytes: &'static [u8],
        mime: &str,
        filename: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.upload_asset_into(bytes, mime, filename, None).await
    }

    /// Upload an asset into `pad` through the pad API and return its `/raw/` URL.
    pub(crate) async fn upload_asset_into(
        &self,
        bytes: &'static [u8],
        mime: &str,
        filename: &str,
        pad: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut query = vec![("filename", filename)];
        query.extend(pad.map(|pad| ("pad", pad)));
        let res = self
            .post("/api/asset")
            .query(&query)
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes)
            .send()
            .await?;
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        let body = res.json::<serde_json::Value>().await?;
        Ok(body["url"].as_str().expect("asset url").to_string())
    }
}
