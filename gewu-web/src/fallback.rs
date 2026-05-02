use axum::{
    body::Body,
    extract::Request,
    response::{Html, IntoResponse, Response},
};

pub async fn serve_static(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');

    let static_path = std::path::Path::new("gewu-web/static");

    let full_path = static_path.join(path);

    if full_path.exists() && full_path.is_file() {
        let body = Body::from(std::fs::read(&full_path).unwrap_or_default());
        let mime = if full_path.extension().map_or(false, |e| e == "wasm") {
            "application/wasm"
        } else if full_path.extension().map_or(false, |e| e == "js") {
            "application/javascript"
        } else if full_path.extension().map_or(false, |e| e == "css") {
            "text/css"
        } else if full_path.extension().map_or(false, |e| e == "html") {
            "text/html"
        } else {
            "application/octet-stream"
        };
        return Response::builder()
            .header("Content-Type", mime)
            .body(body)
            .unwrap_or_else(|_| Body::empty().into_response());
    }

    Html("<html><body><h1>Not Found</h1></body></html>").into_response()
}

pub async fn serve_index() -> impl IntoResponse {
    let static_path = std::path::Path::new("gewu-web/static/index.html");
    let body = Body::from(std::fs::read(static_path).unwrap_or_default());
    Response::builder()
        .header("Content-Type", "text/html")
        .body(body)
        .unwrap_or_else(|_| Body::empty().into_response())
}
