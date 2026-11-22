use axum::{
    Router,
    body::Body,
    extract::OriginalUri,
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::any,
};
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

#[tokio::test]
async fn webapp_relays_query_and_versions_without_browser_credentials_and_revokes_on_detach() {
    let native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", native.local_addr().unwrap());
    let service = Router::new().fallback(any(
        |OriginalUri(uri): OriginalUri, method: Method, headers: HeaderMap, body: Body| async move {
            assert!(!headers.contains_key("cookie"));
            assert!(!headers.contains_key("authorization"));
            if headers.get("x-demo-key").is_none_or(|value| value != "native-secret") {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            assert_eq!(headers["x-demo-key"], "native-secret");
            assert_eq!(headers["if-match"], "\"old\"");
            assert_eq!(headers["sec-fetch-site"], "same-origin");
            let bytes = axum::body::to_bytes(body, 1024).await.unwrap();
            if uri.path() == "/native/redirect" {
                return (StatusCode::TEMPORARY_REDIRECT, [("location", "/native/final?next=1")]).into_response();
            }
            if uri.path() == "/native/outside" {
                return (StatusCode::FOUND, [("location", "http://example.invalid/native/final")]).into_response();
            }
            if uri.path() == "/native/forbidden" {
                return (StatusCode::FOUND, [("location", "/private")]).into_response();
            }
            if uri.path() == "/native/final" {
                return format!("{method} {uri}:{}", String::from_utf8_lossy(&bytes)).into_response();
            }
            (
                [("etag", "\"new\"")],
                format!("{uri}:{}", String::from_utf8_lossy(&bytes)),
            )
                .into_response()
        },
    ));
    let native_task = tokio::spawn(axum::serve(native, service).into_future());
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("package");
    let root = tmp.path().join("workspace");
    std::fs::create_dir_all(pkg.join("web")).unwrap();
    std::fs::create_dir(&root).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    for f in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(f), pkg.join(f)).unwrap();
    }
    let adapter = std::fs::read_to_string(pkg.join("adapter.py"))
        .unwrap()
        .replace("[\"GET\",\"POST\"]", "[\"GET\",\"POST\",\"PUT\"]");
    std::fs::write(pkg.join("adapter.py"), adapter).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        pkg.join("gewu_plugin.py"),
    )
    .unwrap();
    let manifest = pkg.join("plugin.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    value["views"][0] = json!({"id":"studio","title":"Studio","kind":"webapp","placement":"main","serviceId":"native","entryPath":"/native/","resourceTypes":["text"],"stateSchema":{"type":"object"}});
    std::fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    let host = PluginHost::from_config(
        &BTreeMap::from([(
            "demo".into(),
            PluginEntry {
                package: pkg,
                enabled: true,
                settings: json!({"service":origin}).as_object().unwrap().clone(),
                workspace_settings: Default::default(),
                service_origins: vec![],
                sources: Default::default(),
            },
        )]),
        None,
        Limits::default(),
    )
    .unwrap();
    let instance = host.open("demo", "w", &root, None).await.unwrap();
    let bound = host
        .action(
            "w",
            &root,
            "s",
            HostAction::Attach {
                instance_id: instance.activation.instance_id.clone(),
                view_type: "studio".into(),
                resource: None,
            },
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(axum::serve(listener, host.data_router()).into_future());
    let url = format!(
        "{base}{}/native/files/scene%2Fintro.html?optional=1&x=a%2Bb",
        bound["app"]["relayPath"]
            .as_str()
            .expect("Attach must return an application relay")
    );
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let request = || {
        client
            .put(&url)
            .header("x-gewu-app-key", bound["app"]["relayKey"].as_str().unwrap())
            .header("if-match", "\"old\"")
            .header("sec-fetch-site", "same-origin")
            .header("cookie", "secret=1")
            .header("authorization", "Bearer secret")
            .body("updated")
    };
    let reply = request().send().await.unwrap();
    assert_eq!(reply.status(), 200);
    assert_eq!(reply.headers()["etag"], "\"new\"");
    assert_eq!(
        reply.text().await.unwrap(),
        "/native/files/scene%2Fintro.html?optional=1&x=a%2Bb:updated"
    );
    let relay = bound["app"]["relayPath"].as_str().unwrap();
    for method in [Method::GET, Method::POST, Method::PUT] {
        let response = client.request(method.clone(), format!("{base}{relay}/native/redirect"))
            .header("x-gewu-app-key", bound["app"]["relayKey"].as_str().unwrap())
            .header("if-match", "\"old\"").header("sec-fetch-site", "same-origin")
            .body("retained").send().await.unwrap();
        assert_eq!(response.status(), 200, "authenticated redirect failed for {method}");
        let expected_body = if method == Method::GET { "" } else { "retained" };
        assert_eq!(response.text().await.unwrap(), format!("{method} /native/final?next=1:{expected_body}"));
    }
    for (path, expected) in [("outside", 502), ("forbidden", 403)] {
        let response = client.get(format!("{base}{relay}/native/{path}"))
            .header("x-gewu-app-key", bound["app"]["relayKey"].as_str().unwrap())
            .header("if-match", "\"old\"").header("sec-fetch-site", "same-origin")
            .send().await.unwrap();
        assert_eq!(response.status(), expected);
    }
    let view = bound["viewId"].as_str().unwrap().to_string();
    host.action(
        "w",
        &root,
        "s",
        HostAction::RenewView {
            view_id: view.clone(),
        },
    )
    .await
    .unwrap();
    host.action("w", &root, "s", HostAction::Detach { view_id: view })
        .await
        .unwrap();
    assert_eq!(request().send().await.unwrap().status(), 404);
    host.close_all().await.unwrap();
    task.abort();
    native_task.abort();
}
