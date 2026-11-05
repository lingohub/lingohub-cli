// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::error::Error;
use reqwest::{
    Client, Method, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

pub const DEFAULT_BASE_URL: &str = "https://api.lingohub.com";
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_COLLECTION_PAGES: usize = 1_000;
const MAX_COLLECTION_ITEMS: usize = 100_000;
const MAX_COLLECTION_BYTES: usize = 64 * 1024 * 1024;

/// An authenticated V2 client. Credentials are deliberately excluded from Debug output.
#[derive(Clone)]
pub struct ApiClient {
    client: Client,
    base: Url,
}

/// JSON response plus headers needed for conditional and asynchronous workflows.
pub struct ApiResponse {
    pub body: Option<Value>,
    pub status: u16,
    pub headers: HeaderMap,
    pub received_bytes: usize,
}

#[derive(Clone, Default)]
pub struct RequestOptions {
    pub if_match: Option<String>,
    pub idempotency_key: Option<String>,
}

/// One bounded multipart body; bytes are sent once without automatic retry.
pub struct MultipartUpload {
    pub content_type: String,
    pub body: Vec<u8>,
}

pub fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

/// Validates the configured API origin: HTTPS, or HTTP only on loopback, without a path.
pub fn api_origin(base: &str) -> Result<Url, Error> {
    let base = Url::parse(base).map_err(|_| Error::Input("Invalid API base URL".into()))?;
    if !matches!(base.scheme(), "https" | "http")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || base.path() != "/"
    {
        return Err(Error::Input(
            "API base URL must be an HTTP(S) origin without credentials, query or path".into(),
        ));
    }
    if base.scheme() == "http" && !is_loopback(&base) {
        return Err(Error::Input(
            "Remote API origins require HTTPS; HTTP is allowed only on loopback".into(),
        ));
    }
    Ok(base)
}

impl ApiClient {
    pub fn new(base: &str, token: &str) -> Result<Self, Error> {
        let base = api_origin(base)?;
        if token.trim().is_empty() {
            return Err(Error::Input(
                "API key required: run `lingohub login`, or set LINGOHUB_API_KEY or --api-key".into(),
            ));
        }
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Input("Invalid API key header".into()))?;
        auth.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);
        let client = Client::builder()
            .default_headers(headers)
            .user_agent(concat!("lingohub-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(network_error)?;
        Ok(Self { client, base })
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        options: &RequestOptions,
    ) -> Result<ApiResponse, Error> {
        let mut request = self.prepare_request(method.clone(), path, query, options)?;
        if let Some(body) = body {
            if method == Method::PATCH {
                request = request.header("Content-Type", "application/merge-patch+json");
            }
            request = request.json(body);
        }
        Self::send_json(request).await
    }

    pub async fn upload(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        multipart: MultipartUpload,
        options: &RequestOptions,
    ) -> Result<ApiResponse, Error> {
        let request = self
            .prepare_request(method, path, query, options)?
            .header("Content-Type", multipart.content_type)
            .body(multipart.body);
        Self::send_json(request).await
    }

    fn prepare_request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        options: &RequestOptions,
    ) -> Result<reqwest::RequestBuilder, Error> {
        if !path.starts_with("/v2/")
            || path.contains(['?', '#', '\\', '%'])
            || path.split('/').any(|p| matches!(p, "." | ".."))
        {
            return Err(Error::Input(
                "Use a /v2/... path without query, fragments or encoded separators; pass --query separately"
                    .into(),
            ));
        }
        let mut url = self.base.clone();
        url.set_path(path);
        let mut request = self
            .client
            .request(method.clone(), url)
            .query(query)
            .header("Accept", "application/json");
        if let Some(value) = &options.if_match {
            request = request.header(
                "If-Match",
                HeaderValue::from_str(value).map_err(|_| Error::Input("Invalid If-Match header".into()))?,
            );
        }
        if let Some(value) = &options.idempotency_key {
            request = request.header(
                "Idempotency-Key",
                HeaderValue::from_str(value)
                    .map_err(|_| Error::Input("Invalid Idempotency-Key header".into()))?,
            );
        }
        Ok(request)
    }

    async fn send_json(request: reqwest::RequestBuilder) -> Result<ApiResponse, Error> {
        let mut response = request.send().await.map_err(network_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Error::Protocol("Response exceeds the 16 MiB JSON limit".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(Error::from_response(status.as_u16(), &bytes, &headers));
        }
        let body = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).map_err(|_| {
                Error::Protocol(
                    "Expected a JSON response; use the artifact download workflow for binary files".into(),
                )
            })?)
        };
        Ok(ApiResponse {
            body,
            status: status.as_u16(),
            headers,
            received_bytes: bytes.len(),
        })
    }

    pub async fn get(&self, path: &str) -> Result<Value, Error> {
        self.request(Method::GET, path, &[], None, &RequestOptions::default())
            .await?
            .body
            .ok_or_else(|| Error::Protocol("Expected a JSON response body".into()))
    }

    /// Traverse only opaque cursors on the original path, preserving all query filters.
    pub async fn list(&self, path: &str, query: &[(String, String)], all: bool) -> Result<Value, Error> {
        self.list_observed(path, query, all, |_| {}).await
    }

    pub async fn list_observed(
        &self,
        path: &str,
        query: &[(String, String)],
        all: bool,
        mut observe: impl FnMut(&ApiResponse),
    ) -> Result<Value, Error> {
        let mut query = query.to_vec();
        let mut seen: HashSet<String> = query
            .iter()
            .filter(|(k, _)| k == "cursor")
            .map(|(_, v)| v.clone())
            .collect();
        let mut items = Vec::new();
        let mut pages = 0;
        let mut bytes = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        loop {
            if pages == MAX_COLLECTION_PAGES {
                return Err(Error::Protocol(
                    "Collection exceeds 1,000 pages; use explicit cursors".into(),
                ));
            }
            let response = tokio::time::timeout_at(
                deadline,
                self.request(Method::GET, path, &query, None, &RequestOptions::default()),
            )
            .await
            .map_err(|_| {
                Error::Protocol("Collection traversal exceeded five minutes; use explicit cursors".into())
            })??;
            pages += 1;
            observe(&response);
            let body = response
                .body
                .ok_or_else(|| Error::Protocol("Expected a collection response".into()))?;
            let (data, cursor) = collection_page(&body)?;
            if !all {
                return Ok(body);
            }
            bytes += response.received_bytes;
            if items.len() + data.len() > MAX_COLLECTION_ITEMS || bytes > MAX_COLLECTION_BYTES {
                return Err(Error::Protocol(
                    "Collection exceeds 100,000 items or 64 MiB; use explicit cursors".into(),
                ));
            }
            items.extend(data.iter().cloned());
            let Some(cursor) = cursor else {
                return Ok(Value::Array(items));
            };
            if !seen.insert(cursor.to_owned()) {
                return Err(Error::Protocol("Server repeated a pagination cursor".into()));
            }
            query.retain(|(k, _)| k != "cursor");
            query.push(("cursor".into(), cursor.into()));
        }
    }
}

pub fn collection_page(body: &Value) -> Result<(&Vec<Value>, Option<&str>), Error> {
    let invalid =
        || Error::Protocol("Invalid V2 collection: expected data, has_more and matching next_cursor".into());
    if body.get("object_type").and_then(Value::as_str) != Some("LIST") {
        return Err(invalid());
    }
    let data = body.get("data").and_then(Value::as_array).ok_or_else(invalid)?;
    let more = body
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let cursor = body.get("next_cursor").ok_or_else(invalid)?;
    if more {
        let cursor = cursor.as_str().filter(|s| !s.is_empty()).ok_or_else(invalid)?;
        Ok((data, Some(cursor)))
    } else if cursor.is_null() {
        Ok((data, None))
    } else {
        Err(invalid())
    }
}

pub fn network_error(e: reqwest::Error) -> Error {
    Error::Network(
        if e.is_timeout() {
            "Request timed out"
        } else if e.is_connect() {
            "Could not connect to the API"
        } else {
            "HTTP request failed"
        }
        .into(),
    )
}
