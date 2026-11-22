//! Browser-facing isolated application origins. The upstream credential stays here.
use crate::server::AppState;
use anyhow::{Context, Result, ensure};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct AppGateway {
    config: Option<Config>,
    sessions: Mutex<HashMap<String, Session>>,
}

struct Config {
    parent: String,
    template: String,
    local_aliases: Vec<String>,
}
#[derive(Clone)]
struct Session {
    owner: String,
    origin: String,
    bootstrap: String,
    cookie: String,
    nonce: String,
    entry: String,
    relay: String,
    key: String,
    created: Instant,
    redeemed: bool,
}

pub(crate) struct AppOwner {
    gateway: Arc<AppGateway>,
    id: String,
}
impl Drop for AppOwner {
    fn drop(&mut self) {
        self.gateway
            .sessions
            .lock()
            .unwrap()
            .retain(|_, s| s.owner != self.id);
    }
}
fn secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn origin(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "expected an HTTP origin without a path"
    );
    Ok(url)
}

impl AppGateway {
    pub fn configure(parent: Option<&str>, template: Option<&str>) -> Result<Self> {
        let (Some(parent), Some(template)) = (parent, template) else {
            ensure!(
                parent.is_none() && template.is_none(),
                "public-origin and plugin-app-origin-template must be configured together"
            );
            return Ok(Self::default());
        };
        let parent = origin(parent)?;
        ensure!(
            template.matches("{view}").count() == 1,
            "app origin template needs exactly one {{view}} label"
        );
        let example = origin(&template.replace("{view}", "view"))?;
        let host = example.host_str().context("missing application host")?;
        let suffix = host
            .strip_prefix("view.")
            .context("{{view}} must be the first hostname label")?;
        let local = (suffix == "localhost" || suffix.ends_with(".localhost"))
            && parent
                .host_str()
                .is_some_and(|h| h == "localhost" || h.ends_with(".localhost"));
        ensure!(
            local || (example.scheme() == "https" && parent.scheme() == "https"),
            "remote Studio requires HTTPS"
        );
        if !local {
            let site = suffix
                .strip_prefix("apps.")
                .context("remote application template must be {{view}}.apps.<site>")?;
            let gui = parent.host_str().unwrap_or("");
            ensure!(
                site.contains('.') && (gui == site || gui.ends_with(&format!(".{site}"))),
                "app and GUI origins must be same-site"
            );
        }
        ensure!(
            example.origin() != parent.origin(),
            "application origin must differ from GUI"
        );
        Ok(Self {
            config: Some(Config {
                parent: parent.origin().ascii_serialization(),
                template: template.trim_end_matches('/').into(),
                local_aliases: Vec::new(),
            }),
            sessions: Mutex::new(HashMap::new()),
        })
    }
    pub fn with_local_aliases(mut self, authorities: Vec<String>) -> Self {
        if let Some(config) = &mut self.config {
            config.local_aliases = authorities;
        }
        self
    }
    pub fn owner(self: &Arc<Self>) -> AppOwner {
        AppOwner {
            gateway: self.clone(),
            id: secret(),
        }
    }
    pub fn frame_source(&self) -> Option<String> {
        self.config
            .as_ref()
            .map(|c| c.template.replace("{view}", "*"))
    }
    pub fn unbind(&self, owner: &AppOwner, view: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.get(view).is_some_and(|s| s.owner == owner.id) {
            sessions.remove(view);
        }
    }
    pub fn bind(&self, owner: &AppOwner, result: &mut Value) -> Result<()> {
        let Some(app) = result.as_object_mut().and_then(|r| r.remove("app")) else {
            return Ok(());
        };
        let c = self.config.as_ref().context(
            "APP_ORIGIN_NOT_CONFIGURED: set --public-origin and --plugin-app-origin-template",
        )?;
        let view = result["viewId"]
            .as_str()
            .context("missing view id")?
            .to_owned();
        Uuid::parse_str(&view)?;
        let relay = app["relayPath"].as_str().context("missing relay path")?;
        ensure!(
            relay
                .strip_prefix("/plugin-app/")
                .is_some_and(|v| Uuid::parse_str(v).is_ok()),
            "invalid application relay"
        );
        let key = app["relayKey"].as_str().context("missing relay key")?;
        ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid relay key"
        );
        let entry = app["entryPath"]
            .as_str()
            .context("missing application entry")?;
        ensure!(
            entry.starts_with('/') && !entry.starts_with("//") && !entry.contains(['#', '?', '\\']),
            "invalid app entry"
        );
        let s = Session {
            owner: owner.id.clone(),
            origin: c.template.replace("{view}", &view),
            bootstrap: secret(),
            cookie: secret(),
            nonce: secret(),
            entry: entry.into(),
            relay: relay.into(),
            key: key.into(),
            created: Instant::now(),
            redeemed: false,
        };
        result["url"] = format!("{}/_gewu/bootstrap#{}", s.origin, s.bootstrap).into();
        result["origin"] = s.origin.clone().into();
        result["nonce"] = s.nonce.clone().into();
        let mut sessions = self.sessions.lock().unwrap();
        ensure!(sessions.len() < 1024, "too many application views");
        sessions.insert(view.into(), s);
        Ok(())
    }
    fn for_host(&self, host: &str) -> Option<(String, Session)> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(_, s)| reqwest::Url::parse(&s.origin).is_ok_and(|u| u.authority() == host))
            .map(|(id, s)| (id.clone(), s.clone()))
    }
}

pub(crate) async fn dispatch(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(config) = state.apps.config.as_ref() else {
        return next.run(request).await;
    };
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if host == reqwest::Url::parse(&config.parent).unwrap().authority() {
        return next.run(request).await;
    }
    if config.local_aliases.contains(&host)
        && matches!(*request.method(), Method::GET | Method::HEAD)
    {
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        return axum::response::Redirect::temporary(&format!("{}{path}", config.parent))
            .into_response();
    }
    // Unknown application hosts must never expose the main GUI's socket or config.
    let Some((view, s)) = state.apps.for_host(&host) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = request.uri().path();
    if path == "/_gewu/bootstrap.mjs" && request.method() == Method::GET {
        return (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            include_str!("../assets/plugin-app-bootstrap.mjs"),
        )
            .into_response();
    }
    if path == "/_gewu/bootstrap" {
        if request.method() == Method::GET {
            return ([(header::CONTENT_TYPE,"text/html"),(header::CACHE_CONTROL,"no-store"),(header::CONTENT_SECURITY_POLICY,"default-src 'none'; script-src 'self'; connect-src 'self'; frame-ancestors *; base-uri 'none'")],"<!doctype html><meta charset=utf-8><title>Opening Studio</title><body>Opening Studio…<script type=module src=/_gewu/bootstrap.mjs></script>").into_response();
        }
        if request.method() != Method::POST
            || request
                .headers()
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                != Some(s.origin.as_str())
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        let Ok(bytes) = to_bytes(request.into_body(), 128).await else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let mut sessions = state.apps.sessions.lock().unwrap();
        let Some(current) = sessions.get_mut(&view) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if current.redeemed
            || current.created.elapsed() > Duration::from_secs(60)
            || bytes.as_ref() != current.bootstrap.as_bytes()
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        current.redeemed = true;
        let secure = if s.origin.starts_with("https:") {
            "; Secure"
        } else {
            ""
        };
        let cookie = format!(
            "gewu_app={}; Path=/; HttpOnly; SameSite=Strict{secure}",
            s.cookie
        );
        return (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
                (header::SET_COOKIE, &cookie),
            ],
            json!({"entryPath":s.entry,"viewId":view,"nonce":s.nonce,"parentOrigin":config.parent})
                .to_string(),
        )
            .into_response();
    }
    if !s.redeemed || !has_cookie(request.headers(), &s.cookie) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        && request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            != Some(s.origin.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let method = request.method().clone();
    let path = request.uri().path_and_query().unwrap().as_str();
    let url = format!(
        "{}{}{}",
        state.server_http.trim_end_matches('/'),
        s.relay,
        path
    );
    let Ok(origin) = reqwest::Url::parse(&state.server_http).map(|u| u.origin()) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let mut upstream = state
        .http_client
        .request(method.clone(), url.clone())
        .header("x-gewu-app-key", &s.key)
        .header("x-gewu-parent-origin", &config.parent)
        .header("x-gewu-app-origin", &s.origin);
    for name in [
        "content-type",
        "accept",
        "range",
        "if-match",
        "if-none-match",
        "last-event-id",
        "sec-fetch-site",
        "sec-fetch-dest",
        "referer",
        "x-hyperframes-write-token",
        "x-gewu-view-key",
    ] {
        if let Some(v) = request.headers().get(name) {
            upstream = upstream.header(name, v);
        }
    }
    if !matches!(method, Method::GET | Method::HEAD) {
        upstream = upstream.body(reqwest::Body::wrap_stream(
            request.into_body().into_data_stream(),
        ));
    }
    let reply = match upstream.send().await {
        Ok(r) => r,
        Err(_) => {
            return (StatusCode::BAD_GATEWAY, "the GeWu server did not answer").into_response();
        }
    };
    let mut response = Response::builder().status(reply.status());
    if reply.status().is_redirection() && reply.status() != StatusCode::NOT_MODIFIED {
        let Some(next) = reply
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| reqwest::Url::parse(&url).ok().and_then(|u| u.join(v).ok()))
        else {
            return (
                StatusCode::BAD_GATEWAY,
                "the GeWu server sent an unusable redirect",
            )
                .into_response();
        };
        if next.origin() != origin
            || !next
                .path()
                .strip_prefix(&s.relay)
                .is_some_and(|path| path.starts_with('/'))
        {
            return (
                StatusCode::BAD_GATEWAY,
                "the GeWu server redirects outside this view",
            )
                .into_response();
        }
        let path = next.path().strip_prefix(&s.relay).unwrap();
        let location = format!(
            "{path}{}{}",
            next.query()
                .map(|query| format!("?{query}"))
                .unwrap_or_default(),
            next.fragment()
                .map(|fragment| format!("#{fragment}"))
                .unwrap_or_default()
        );
        response = response.header(header::LOCATION, location);
    }
    for name in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
        "etag",
        "content-disposition",
        "cache-control",
        "content-security-policy",
        "x-frame-options",
        "x-content-type-options",
    ] {
        if let Some(v) = reply.headers().get(name) {
            response = response.header(name, v);
        }
    }
    response
        .header(header::REFERRER_POLICY, "no-referrer")
        .body(Body::from_stream(reply.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn has_cookie(headers: &HeaderMap, secret: &str) -> bool {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .any(|p| p.trim().strip_prefix("gewu_app=") == Some(secret))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_and_grants_do_not_expose_relay_keys() {
        for (parent, app) in [
            ("http://example.com", "http://{view}.apps.example.com"),
            ("https://example.com", "https://evil.com/{view}"),
            ("https://example.com", "https://{view}.apps.other.com"),
        ] {
            assert!(AppGateway::configure(Some(parent), Some(app)).is_err());
        }
        let gateway = Arc::new(
            AppGateway::configure(
                Some("http://localhost:8430"),
                Some("http://{view}.localhost:8430"),
            )
            .unwrap(),
        );
        let owner = gateway.owner();
        let id = Uuid::new_v4().to_string();
        let key = "a".repeat(64);
        let mut result = json!({"viewId":id,"app":{"relayPath":format!("/plugin-app/{}",Uuid::new_v4()),"relayKey":key,"entryPath":"/"}});
        gateway.bind(&owner, &mut result).unwrap();
        assert!(!result.to_string().contains(&key));
        assert!(
            result["url"]
                .as_str()
                .unwrap()
                .contains("/_gewu/bootstrap#")
        );
        assert!(gateway.for_host(&format!("{id}.localhost:8430")).is_some());
        drop(owner);
        assert!(gateway.for_host(&format!("{id}.localhost:8430")).is_none());
    }
}
