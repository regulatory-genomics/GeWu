//! Capabilities for HTTP bytes. Native URLs, headers and filesystem paths stay here.
use super::{
    Instance,
    package::{contained, relative},
    workspace_path,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    extract::{Path as AxPath, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use gewu_protocol::plugin::{HttpMethod, ResourceRef, Source};
use reqwest::Url;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use uuid::Uuid;

const TTL: Duration = Duration::from_secs(120);
const ASSET_CSP: &str = "sandbox allow-scripts; default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' blob:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";
const DOCUMENT_CSP: &str = "sandbox; default-src 'none'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; media-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

#[derive(Default)]
pub(crate) struct Resources {
    tickets: Mutex<HashMap<Uuid, Ticket>>,
}

#[derive(Clone)]
struct Ticket {
    target: Data,
    expires: Instant,
    instance: Weak<Instance>,
    view: Option<Weak<AtomicBool>>,
}

#[derive(Clone)]
enum Data {
    Assets(PathBuf),
    File {
        root: PathBuf,
        path: PathBuf,
        mime: String,
        sibling_assets: bool,
    },
    Service {
        url: Url,
        method: Method,
        headers: HeaderMap,
    },
}

impl Resources {
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/plugin-data/{ticket}/{*path}", any(fetch))
            .with_state(self.clone())
    }

    fn mint(
        &self,
        instance: &Arc<Instance>,
        view: Option<&Arc<AtomicBool>>,
        target: Data,
        path: &str,
    ) -> Result<String> {
        let mut tickets = self.tickets.lock().unwrap();
        let now = Instant::now();
        tickets.retain(|_, t| {
            t.expires > now
                && t.instance.strong_count() != 0
                && t.view.as_ref().is_none_or(|v| v.strong_count() != 0)
        });
        ensure!(tickets.len() < 4096, "too many active resource tickets");
        let id = Uuid::new_v4();
        tickets.insert(
            id,
            Ticket {
                target,
                expires: now + TTL,
                instance: Arc::downgrade(instance),
                view: view.map(Arc::downgrade),
            },
        );
        let path = percent_encoding::utf8_percent_encode(path, &super::URI_ESCAPE);
        Ok(format!("/plugin-data/{id}/{path}"))
    }

    pub fn assets(
        &self,
        instance: &Arc<Instance>,
        view: &Arc<AtomicBool>,
        entry: &str,
    ) -> Result<String> {
        let root = contained(&instance.package.root, Path::new("web"))?;
        let path = contained(
            &root,
            relative(
                entry
                    .strip_prefix("web/")
                    .context("webview entries must be under web/")?,
            )?,
        )?;
        self.mint(
            instance,
            Some(view),
            Data::Assets(root.clone()),
            path.strip_prefix(root)?
                .to_str()
                .context("non-UTF8 asset")?,
        )
    }

    pub fn service(
        &self,
        instance: &Arc<Instance>,
        view: Option<&Arc<AtomicBool>>,
        id: &str,
        path: &str,
        method: HttpMethod,
    ) -> Result<String> {
        let endpoint = instance
            .services
            .iter()
            .find(|item| item.id == id)
            .context("unknown service")?;
        let origin = Url::parse(&endpoint.base_url)?;
        ensure!(
            origin.username().is_empty()
                && origin.password().is_none()
                && origin.query().is_none()
                && origin.fragment().is_none()
                && origin.path() == "/",
            "invalid service origin"
        );
        // Non-local services must be configured by the operator, not a browser or catalog alone.
        let local = match origin.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        ensure!(
            local
                || instance
                    .service_origins
                    .iter()
                    .any(|entry| Url::parse(entry)
                        .is_ok_and(|allowed| allowed.origin() == origin.origin())),
            "service origin is not allowed"
        );
        ensure!(
            path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains(['\\', '#'])
                && !path.chars().any(char::is_control),
            "invalid service path"
        );
        let url = origin.join(path)?;
        ensure!(
            url.origin() == origin.origin(),
            "service path escapes its origin"
        );
        let decoded = percent_encoding::percent_decode_str(url.path()).decode_utf8()?;
        ensure!(
            !decoded.contains(['\\', '%', '\0'])
                && !decoded.split('/').any(|p| p == "." || p == ".."),
            "ambiguous service path"
        );
        ensure!(
            endpoint.routes.iter().any(|route| {
                let prefix = route.path_prefix.trim_end_matches('/');
                route.methods.contains(&method)
                    && (decoded == prefix
                        || decoded
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.starts_with('/')))
            }),
            "service route or method is not allowed"
        );
        let method = match method {
            HttpMethod::Get => Method::GET,
            HttpMethod::Head => Method::HEAD,
            HttpMethod::Post => Method::POST,
            HttpMethod::Put => Method::PUT,
            HttpMethod::Delete => Method::DELETE,
            HttpMethod::Patch => Method::PATCH,
        };
        let mut headers = HeaderMap::new();
        for (key, value) in &endpoint.headers {
            let name = header::HeaderName::from_bytes(key.as_bytes())?;
            ensure!(
                name == header::AUTHORIZATION || name.as_str().starts_with("x-"),
                "service header is not allowed"
            );
            headers.insert(name, value.parse()?);
        }
        self.mint(
            instance,
            view,
            Data::Service {
                url,
                method,
                headers,
            },
            "resource",
        )
    }

    pub async fn resolve(
        &self,
        instance: &Arc<Instance>,
        view: Option<&Arc<AtomicBool>>,
        resource: ResourceRef,
    ) -> Result<String> {
        let root = Path::new(&instance.activation.workspace_root);
        let file = if resource.uri.starts_with("workspace:///") {
            let path = workspace_path(root, &resource.uri)?;
            let mime = content_type(&path);
            (path, mime)
        } else {
            ensure!(
                resource
                    .uri
                    .starts_with(&format!("plugin://{}/", instance.package.manifest.id)),
                "resource belongs to a different plugin"
            );
            let response = instance
                .adapter
                .request(gewu_protocol::plugin::Operation::Resolve(resource), None)
                .await
                .map_err(|err| anyhow::anyhow!("{}: {}", err.code, err.message))?;
            match response {
                gewu_protocol::plugin::Response::Resolved(Source::File {
                    path, mime_type, ..
                }) => (contained(root, Path::new(&path))?, mime_type),
                gewu_protocol::plugin::Response::Resolved(Source::Service {
                    service_id,
                    path,
                    ..
                }) => return self.service(instance, view, &service_id, &path, HttpMethod::Get),
                _ => anyhow::bail!("invalid resolve response"),
            }
        };
        let (path, mime) = file;
        ensure!(path.is_file(), "resource is not a regular file");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("non-UTF8 path")?
            .to_owned();
        let sibling_assets = mime == "text/html";
        self.mint(
            instance,
            view,
            Data::File {
                root: root.to_owned(),
                path,
                mime,
                sibling_assets,
            },
            &name,
        )
    }
}

fn content_type(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

async fn fetch(
    State(state): State<Arc<Resources>>,
    AxPath((id, path)): AxPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Ok(id) = Uuid::parse_str(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ticket = state.tickets.lock().unwrap().get(&id).cloned();
    let Some(ticket) = ticket.filter(|t| {
        t.expires > Instant::now()
            && t.instance.upgrade().is_some_and(|i| !i.adapter.is_closed())
            && t.view.as_ref().is_none_or(|v| {
                v.upgrade()
                    .is_some_and(|active| active.load(Ordering::Acquire))
            })
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let assets = matches!(ticket.target, Data::Assets(_));
    let result = if method == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        match ticket.target {
            Data::Assets(root) => match relative(&path).and_then(|path| contained(&root, path)) {
                Ok(path) => file(&path, &content_type(&path), &method, &headers).await,
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            },
            Data::File {
                root,
                path: source,
                mime,
                sibling_assets,
            } => {
                if source.file_name().is_some_and(|n| n == path.as_str()) {
                    match contained(&root, &source) {
                        Ok(source) => file(&source, &mime, &method, &headers).await,
                        Err(_) => StatusCode::NOT_FOUND.into_response(),
                    }
                } else if sibling_assets {
                    match relative(&path).and_then(|path| contained(source.parent().unwrap(), path))
                    {
                        Ok(path)
                            if matches!(
                                content_type(&path).split('/').next(),
                                Some("image" | "font" | "video" | "audio")
                            ) || content_type(&path) == "text/css" =>
                        {
                            file(&path, &content_type(&path), &method, &headers).await
                        }
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            Data::Service {
                url,
                method: allowed,
                headers: private,
            } if method == allowed || (method == Method::HEAD && allowed == Method::GET) => {
                if path != "resource" {
                    return StatusCode::NOT_FOUND.into_response();
                }
                service(url, method, headers, private, body).await
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    };
    with_headers(result, assets)
}

fn with_headers(mut response: Response, assets: bool) -> Response {
    let h = response.headers_mut();
    for (name, value) in [
        ("access-control-allow-origin", "null"),
        (
            "access-control-allow-headers",
            "Content-Type, Range, Accept",
        ),
        (
            "access-control-allow-methods",
            "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS",
        ),
        (
            "access-control-expose-headers",
            "Content-Range, Accept-Ranges",
        ),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            if assets { ASSET_CSP } else { DOCUMENT_CSP },
        ),
    ] {
        h.insert(name, value.parse().unwrap());
    }
    response
}

async fn file(path: &Path, mime: &str, method: &Method, headers: &HeaderMap) -> Response {
    if *method != Method::GET && *method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if !tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(meta) = file.metadata().await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = meta.len();
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes");
    let take = if let Some(range) = headers.get(header::RANGE) {
        let Some((start, end)) = range.to_str().ok().and_then(|range| byte_range(range, len))
        else {
            return response
                .status(416)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response());
        };
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        response = response
            .status(206)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
        end - start + 1
    } else {
        len
    };
    response
        .header(header::CONTENT_LENGTH, take)
        .body(if *method == Method::HEAD {
            Body::empty()
        } else {
            Body::from_stream(tokio_util::io::ReaderStream::new(file.take(take)))
        })
        .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
}

fn byte_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let last = len.checked_sub(1)?;
    if start.is_empty() {
        return Some((
            len.saturating_sub(end.parse::<u64>().ok()?.checked_sub(1)? + 1),
            last,
        ));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        last
    } else {
        end.parse::<u64>().ok()?.min(last)
    };
    (start <= end).then_some((start, end))
}

async fn service(
    url: Url,
    method: Method,
    incoming: HeaderMap,
    private: HeaderMap,
    body: Body,
) -> Response {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    });
    let mut request = client.request(method.clone(), url).headers(private);
    for name in [header::CONTENT_TYPE, header::RANGE, header::ACCEPT] {
        if let Some(value) = incoming.get(&name) {
            request = request.header(name, value);
        }
    }
    if method != Method::GET && method != Method::HEAD {
        request = request.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let Ok(upstream) = request.send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if upstream.status().is_redirection() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut response = Response::builder().status(upstream.status());
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests {
    use super::byte_range;
    #[test]
    fn byte_ranges_reject_invalid_and_clamp_suffix() {
        assert_eq!(byte_range("bytes=1-4", 3), Some((1, 2)));
        assert_eq!(byte_range("bytes=-2", 3), Some((1, 2)));
        assert_eq!(byte_range("bytes=3-", 3), None);
        assert_eq!(byte_range("bytes=-0", 3), None);
    }
}
