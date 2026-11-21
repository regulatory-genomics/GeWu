use gewu_protocol::plugin::ServiceEndpoint;
use serde_json::json;

#[test]
fn explicitly_declared_websocket_endpoint_roundtrips() {
    let value = json!({"id":"screen","baseUrl":"http://127.0.0.1:5123","protocol":"websocket","routes":[{"pathPrefix":"/screen","methods":["GET"]}]});
    let endpoint = serde_json::from_value::<ServiceEndpoint>(value.clone());
    assert!(
        endpoint.is_ok(),
        "Explicit WebSocket service endpoints must be accepted: {endpoint:?}"
    );
    assert_eq!(serde_json::to_value(endpoint.unwrap()).unwrap(), value);
}
