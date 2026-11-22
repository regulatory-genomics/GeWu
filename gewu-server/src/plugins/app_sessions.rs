//! View-bound, path-transparent applications. Credentials never reach model context.
use super::Instance;
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use futures::StreamExt;
use gewu_protocol::plugin::{HttpProtocol, ServiceEndpoint};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(900);

#[derive(Default)]
pub(super) struct Apps {
    grants: Mutex<HashMap<String, Arc<Grant>>>,
}

struct Grant {
    key: String,
    view_id: String,
    instance: Weak<Instance>,
    view: Weak<AtomicBool>,
    expires: Mutex<Instant>,
    service: ServiceEndpoint,
}

impl Grant {
    fn active(&self) -> bool {
        *self.expires.lock().unwrap() > Instant::now()
            && self
                .view
                .upgrade()
                .is_some_and(|v| v.load(Ordering::Acquire))
            && self
                .instance
                .upgrade()
                .is_some_and(|i| !i.adapter.is_closed())
    }
}

impl Apps {
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/plugin-app/{grant}/{*path}", any(relay))
            .route("/plugin-app/{grant}/", any(relay))
            .with_state(self.clone())
    }

    pub fn create(
        &self,
        instance: &Arc<Instance>,
        active: &Arc<AtomicBool>,
        view_id: &str,
        service_id: &str,
        entry: &str,
    ) -> Result<Value> {
        let service = instance
            .services
            .iter()
            .find(|s| s.id == service_id && matches!(s.protocol, HttpProtocol::Http))
            .context("webapp service is not registered")?
            .clone();
        let origin = reqwest::Url::parse(&service.base_url)?;
        let local = origin
            .host_str()
            .and_then(|h| h.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        ensure!(
            local
                || instance
                    .service_origins
                    .iter()
                    .any(|u| reqwest::Url::parse(u).is_ok_and(|u| u.origin() == origin.origin())),
            "webapp service origin is not allowed"
        );
        ensure!(
            allowed(&service, entry, &Method::GET),
            "webapp entry is not allowed"
        );
        let mut grants = self.grants.lock().unwrap();
        grants.retain(|_, g| g.active());
        ensure!(grants.len() < 1024, "too many application views");
        let id = Uuid::new_v4().to_string();
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        grants.insert(
            id.clone(),
            Arc::new(Grant {
                key: key.clone(),
                view_id: view_id.into(),
                instance: Arc::downgrade(instance),
                view: Arc::downgrade(active),
                expires: Mutex::new(Instant::now() + LEASE),
                service,
            }),
        );
        Ok(json!({"relayPath":format!("/plugin-app/{id}"),"relayKey":key,"entryPath":entry}))
    }

    pub fn renew(&self, view_id: &str) -> Result<()> {
        let grants = self.grants.lock().unwrap();
        let grant = grants
            .values()
            .find(|g| g.view_id == view_id && g.active())
            .context("application view expired")?;
        *grant.expires.lock().unwrap() = Instant::now() + LEASE;
        Ok(())
    }
}

fn allowed(service: &ServiceEndpoint, raw: &str, method: &Method) -> bool {
    if !raw.starts_with('/') || raw.starts_with("//") || raw.contains(['\\', '#', '\0']) {
        return false;
    }
    let path = raw.split('?').next().unwrap_or(raw);
    let mut decoded = path.to_string();
    for _ in 0..3 {
        if decoded.contains(['\\', '\0']) || decoded.split('/').any(|p| p == "." || p == "..") {
            return false;
        }
        let Ok(next) = percent_encoding::percent_decode_str(&decoded).decode_utf8() else {
            return false;
        };
        if next == decoded {
            break;
        }
        decoded = next.into_owned();
    }
    if decoded.contains('%') || decoded.split('/').any(|p| p == "." || p == "..") {
        return false;
    }
    service.routes.iter().any(|r| {
        let prefix = r.path_prefix.trim_end_matches('/');
        (decoded == prefix
            || decoded
                .strip_prefix(prefix)
                .is_some_and(|p| p.starts_with('/')))
            && r.methods.iter().any(|m| {
                serde_json::to_value(m).is_ok_and(|v| {
                    v.as_str() == Some(method.as_str()) || (method == Method::HEAD && v == "GET")
                })
            })
    })
}

async fn relay(
    State(apps): State<Arc<Apps>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let rest = uri.path().strip_prefix("/plugin-app/").unwrap_or("");
    let Some((id, _)) = rest.split_once('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let grant = apps.grants.lock().unwrap().get(id).cloned();
    let Some(grant) = grant.filter(|g| {
        g.active()
            && headers
                .get("x-gewu-app-key")
                .is_some_and(|v| v.as_bytes() == g.key.as_bytes())
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let prefix = format!("/plugin-app/{id}");
    let raw = uri
        .path_and_query()
        .unwrap()
        .as_str()
        .strip_prefix(&prefix)
        .unwrap();
    if !allowed(&grant.service, raw, &method) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(origin) =
        reqwest::Url::parse(grant.service.base_url.trim_end_matches('/')).map(|u| u.origin())
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok(url) = reqwest::Url::parse(&format!(
        "{}{}",
        grant.service.base_url.trim_end_matches('/'),
        raw
    )) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    });
    let mut request = client.request(method.clone(), url.clone());
    for (k, v) in &grant.service.headers {
        request = request.header(k, v);
    }
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
        "x-gewu-parent-origin",
        "x-gewu-app-origin",
    ] {
        if let Some(v) = headers.get(name) {
            request = request.header(name, v);
        }
    }
    if let Some(origin) = headers
        .get("x-gewu-app-origin")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| reqwest::Url::parse(v).ok())
    {
        if origin.path() != "/"
            || origin.query().is_some()
            || !matches!(origin.scheme(), "http" | "https")
        {
            return StatusCode::BAD_REQUEST.into_response();
        }
        request = request
            .header("x-forwarded-host", origin.authority())
            .header("x-forwarded-proto", origin.scheme())
            .header("origin", origin.origin().ascii_serialization());
    }
    if method != Method::GET && method != Method::HEAD {
        request = request.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let reply = match request.send().await {
        Ok(r) => r,
        Err(_) => {
            return (StatusCode::BAD_GATEWAY, "the application did not answer").into_response();
        }
    };
    let mut response = Response::builder().status(reply.status());
    if reply.status().is_redirection() && reply.status() != StatusCode::NOT_MODIFIED {
        let Some(next) = reply
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| url.join(v).ok())
        else {
            return (
                StatusCode::BAD_GATEWAY,
                "the application sent an unusable redirect",
            )
                .into_response();
        };
        if next.origin() != origin {
            return (
                StatusCode::BAD_GATEWAY,
                "the application redirects outside its own origin",
            )
                .into_response();
        }
        let next_method = match reply.status().as_u16() {
            301 | 302 if method == Method::POST => &Method::GET,
            303 if method != Method::HEAD => &Method::GET,
            _ => &method,
        };
        if !allowed(&grant.service, next.path(), next_method) {
            return StatusCode::FORBIDDEN.into_response();
        }
        // The browser can replay 307/308 bodies; each hop re-enters this authenticated relay.
        response = response.header(header::LOCATION, format!("{prefix}{}", &next[url::Position::BeforePath..]));
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
    let stream = futures::stream::unfold(
        (reply.bytes_stream(), grant),
        |(mut stream, grant)| async move {
            loop {
                if !grant.active() {
                    return None;
                }
                tokio::select! {
                    next = stream.next() => return next.map(|b| (b, (stream, grant))),
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
        },
    );
    response
        .header(header::REFERRER_POLICY, "no-referrer")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_rules_allow_encoded_nested_files_but_reject_traversal() {
        let service:ServiceEndpoint = serde_json::from_value(json!({"id":"app","baseUrl":"http://127.0.0.1:1","protocol":"http","routes":[{"pathPrefix":"/api","methods":["GET","PUT"]}]})).unwrap();
        assert!(allowed(
            &service,
            "/api/files/scene%2Fintro.html?raw=1",
            &Method::PUT
        ));
        for path in [
            "/api/../secret",
            "/api/%2e%2e/secret",
            "/api/%252e%252e/secret",
            "/api/%5csecret",
            "/apiculture",
            "//host/api",
        ] {
            assert!(!allowed(&service, path, &Method::GET), "{path}");
        }
        assert!(!allowed(&service, "/api/files", &Method::DELETE));
    }
}
