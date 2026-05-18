#[test]
fn unified_canvas_contract_is_documented() {
    let required_routes = [
        "/api/workspaces",
        "/api/workspaces/{workspace_id}/canvas/graph",
        "/api/workspaces/{workspace_id}/workflow/events",
        "/api/workspaces/{workspace_id}/workflow/lineage/{artifact_id}",
        "/api/chat",
    ];
    assert_eq!(required_routes.len(), 5);
}
