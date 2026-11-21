#[cfg(unix)]
#[tokio::test]
async fn tickets_stream_ranges_and_expire_when_instance_closes() {
    use gewu_protocol::plugin::{HostAction, ResourceRef};
    use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
    use std::{collections::BTreeMap, path::Path};
    let tmp = tempfile::tempdir().unwrap();
    let package = tmp.path().join("package");
    let root = tmp.path().join("workspace");
    std::fs::create_dir_all(package.join("web")).unwrap();
    std::fs::create_dir(&root).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    for file in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(file), package.join(file)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        package.join("gewu_plugin.py"),
    )
    .unwrap();
    let host = PluginHost::from_config(
        &BTreeMap::from([(
            "demo".into(),
            PluginEntry {
                package,
                enabled: true,
                settings: Default::default(),
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
    let file = root.join("out.txt");
    std::fs::write(&file, b"abcdef").unwrap();
    std::os::unix::fs::symlink(tmp.path().join("package/plugin.json"), root.join("outside"))
        .unwrap();
    let resolve = |uri: &str| HostAction::Resource {
        instance_id: instance.activation.instance_id.clone(),
        view_id: None,
        resource: ResourceRef {
            uri: uri.into(),
            resource_type: "text".into(),
            mime_type: None,
            revision: None,
        },
    };
    assert!(
        host.action("w", &root, "session", resolve("workspace:///outside"))
            .await
            .is_err()
    );
    assert!(
        host.action(
            "w",
            &root,
            "session",
            resolve("workspace:///%2e%2e/package/plugin.json")
        )
        .await
        .is_err()
    );
    let ticket = host
        .action("w", &root, "session", resolve("workspace:///out.txt"))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        ticket["url"].as_str().unwrap()
    );
    let server = tokio::spawn(axum::serve(listener, host.data_router()).into_future());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let part = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["Content-Range"], "bytes 1-3/6");
    assert_eq!(part.text().await.unwrap(), "bcd");
    assert_eq!(
        client
            .get(&url)
            .header("Range", "bytes=6-")
            .send()
            .await
            .unwrap()
            .status(),
        416
    );
    let head = client.head(&url).send().await.unwrap();
    assert_eq!(head.headers()["Content-Length"], "6");
    assert!(head.bytes().await.unwrap().is_empty());
    host.close_all().await.unwrap();
    assert_eq!(client.get(&url).send().await.unwrap().status(), 404);
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn websocket_ticket_is_one_use_and_live_stream_is_revoked_on_detach() {
    use axum::{Router, extract::ws::WebSocketUpgrade, http::HeaderMap, response::IntoResponse, routing::get};
    use futures::{SinkExt,StreamExt};
    use gewu_protocol::plugin::{HostAction,HttpMethod};
    use gewu_server::plugins::{PluginHost,package::PluginEntry,stdio::Limits};
    use serde_json::json;
    use std::{collections::BTreeMap,path::Path,time::Duration};
    let native=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin=format!("http://{}",native.local_addr().unwrap());
    let echo=Router::new().route("/native/screen",get(|headers:HeaderMap,ws:WebSocketUpgrade| async move {
        assert_eq!(headers["x-demo-key"],"native-secret");
        ws.on_upgrade(|mut stream|async move {
            while let Some(Ok(msg))=stream.recv().await {if stream.send(msg).await.is_err(){break;}}
        }).into_response()
    }));
    let native_task=tokio::spawn(axum::serve(native,echo).into_future());
    let tmp=tempfile::tempdir().unwrap();let root=tmp.path().join("workspace");let package=tmp.path().join("package");
    std::fs::create_dir_all(package.join("web")).unwrap();std::fs::create_dir(&root).unwrap();
    let fixture=Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    for f in ["plugin.json","adapter.py","web/index.html"] {std::fs::copy(fixture.join(f),package.join(f)).unwrap();}
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),package.join("gewu_plugin.py")).unwrap();
    let host=PluginHost::from_config(&BTreeMap::from([("demo".into(),PluginEntry{package,enabled:true,
        settings:json!({"service":origin,"serviceProtocol":"websocket"}).as_object().unwrap().clone(),
        workspace_settings:Default::default(),service_origins:vec![],sources:Default::default()})]),None,Limits::default()).unwrap();
    let instance=host.open("demo","w",&root,None).await.unwrap();
    let attached=host.action("w",&root,"s",HostAction::Attach{instance_id:instance.activation.instance_id.clone(),view_type:"demo-view".into(),resource:None}).await.unwrap();
    let view=attached["viewId"].as_str().unwrap().to_owned();
    host.action("w",&root,"s",HostAction::ViewReady{view_id:view.clone()}).await.unwrap();
    let ticket=host.action("w",&root,"s",HostAction::Service{view_id:view.clone(),service_id:"native".into(),path:"/native/screen".into(),method:HttpMethod::Get}).await.unwrap();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("ws://{}{}",listener.local_addr().unwrap(),ticket["url"].as_str().unwrap());
    let server=tokio::spawn(axum::serve(listener,host.data_router()).into_future());
    let (mut ws,_)=tokio_tungstenite::connect_async(&url).await.expect("Declared WebSocket service must upgrade");
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(vec![0,1,255,4].into())).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap().into_data().as_ref(), &[0,1,255,4]);
    assert!(tokio_tungstenite::connect_async(&url).await.is_err(),"Consumed ticket must not reconnect");
    host.action("w",&root,"s",HostAction::Detach{view_id:view}).await.unwrap();
    let ended=tokio::time::timeout(Duration::from_secs(2),ws.next()).await.expect("Detach must revoke an existing stream");
    assert!(match ended {Some(Ok(m))=>m.is_close(),_=>true});
    host.close_all().await.unwrap();server.abort();native_task.abort();
}
