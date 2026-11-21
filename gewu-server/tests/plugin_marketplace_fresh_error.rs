#![cfg(unix)]

#[tokio::test]
async fn fresh_home_reports_the_refresh_error_instead_of_a_missing_cache() {
    use gewu_protocol::plugin::HostAction;
    use gewu_server::plugins::{PluginHost,stdio::Limits};
    use std::collections::BTreeMap;
    let home=tempfile::tempdir().unwrap();let workspace=tempfile::tempdir().unwrap();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address=listener.local_addr().unwrap();
    let router=axum::Router::new().route("/index.v1.json",axum::routing::get(||async{axum::http::StatusCode::SERVICE_UNAVAILABLE}));
    let server=tokio::spawn(axum::serve(listener,router).into_future());
    // SAFETY: this integration-test binary has one test and starts no adapter subprocesses.
    unsafe {std::env::set_var("HOME",home.path());std::env::set_var("GEWU_MARKETPLACE_BASE_URL",format!("http://{address}"));}
    let host=PluginHost::from_config(&BTreeMap::new(),None,Limits::default()).unwrap();
    let error=host.action("w",workspace.path(),"owner",HostAction::MarketplaceSearch{query:None,refresh:true}).await.unwrap_err().to_string();
    assert!(error.contains("HTTP 503"),"The real refresh failure was hidden: {error}");
    server.abort();
}
