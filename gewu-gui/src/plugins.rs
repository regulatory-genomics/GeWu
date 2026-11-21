use crate::server::AppState;
use axum::{
    body::Body,
    extract::{Path, State, ws::{WebSocketUpgrade, Message, rejection::WebSocketUpgradeRejection}},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::{SinkExt,StreamExt};
use tokio_tungstenite::tungstenite::Message as NativeMessage;

pub(super) async fn proxy(
    State(app): State<AppState>,
    Path((id, path)): Path<(String, String)>,
    upgrade: Result<WebSocketUpgrade,WebSocketUpgradeRejection>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if uuid::Uuid::parse_str(&id).is_err()
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.contains(['\\', '\0']))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(mut url) = reqwest::Url::parse(&app.server_http) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if let Ok(mut parts) = url.path_segments_mut() {
        parts
            .clear()
            .push("plugin-data")
            .push(&id)
            .extend(path.split('/'));
    }
    if let Ok(upgrade)=upgrade {
        let scheme=if url.scheme()=="https" {"wss"} else {"ws"};
        if url.set_scheme(scheme).is_err(){return StatusCode::BAD_GATEWAY.into_response();}
        let upstream=tokio::time::timeout(std::time::Duration::from_secs(5),tokio_tungstenite::connect_async(url.as_str())).await;
        let Ok(Ok((mut upstream,_)))=upstream else {return StatusCode::BAD_GATEWAY.into_response();};
        return upgrade.max_message_size(8*1024*1024).max_frame_size(8*1024*1024).on_upgrade(move |mut browser|async move {
            loop {
                tokio::select! {
                    value=browser.recv()=>{
                        let Some(Ok(value))=value else {break;};
                        let value=match value {Message::Text(v)=>NativeMessage::Text(v.to_string().into()),Message::Binary(v)=>NativeMessage::Binary(v),Message::Ping(v)=>NativeMessage::Ping(v),Message::Pong(v)=>NativeMessage::Pong(v),Message::Close(_)=>break};
                        if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5),upstream.send(value)).await,Ok(Ok(()))){break;}
                    },
                    value=upstream.next()=>{
                        let Some(Ok(value))=value else {break;};
                        let value=match value {NativeMessage::Text(v)=>Message::Text(v.to_string().into()),NativeMessage::Binary(v)=>Message::Binary(v),NativeMessage::Ping(v)=>Message::Ping(v),NativeMessage::Pong(v)=>Message::Pong(v),NativeMessage::Close(_)=>break,NativeMessage::Frame(_)=>continue};
                        if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5),browser.send(value)).await,Ok(Ok(()))){break;}
                    }
                }
            }
            let _=browser.close().await;let _=upstream.close(None).await;
        }).into_response();
    }
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
    });
    let mut request = client.request(method.clone(), url);
    for name in [header::RANGE, header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value);
        }
    }
    if method != Method::GET && method != Method::HEAD && method != Method::OPTIONS {
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
        header::CONTENT_SECURITY_POLICY,
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::ACCESS_CONTROL_ALLOW_METHODS,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        header::X_CONTENT_TYPE_OPTIONS,
        header::REFERRER_POLICY,
        header::CACHE_CONTROL,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value);
        }
    }
    response
        .body(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from_stream(upstream.bytes_stream())
        })
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}
