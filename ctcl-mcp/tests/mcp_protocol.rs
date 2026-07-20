//! Real end-to-end MCP protocol test: spawns the actual compiled `ctcl-mcp`
//! binary as a child process and speaks real MCP (JSON-RPC over stdio) to it
//! via rmcp's own client transport - not a mock of the internal logic. Same
//! "real socket, not mocks" discipline as ctcl-desktop's local_api.rs tests,
//! applied to the protocol boundary this crate adds.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::transport::TokioChildProcess;
use std::process::Stdio;

#[tokio::test]
async fn real_stdio_server_lists_tools_and_answers_ctcl_now() {
    let db_path = std::env::temp_dir().join(format!("ctcl-mcp-test-{}.sqlite3", std::process::id()));
    let _ = std::fs::remove_file(&db_path);

    let bin = env!("CARGO_BIN_EXE_ctcl-mcp");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--db").arg(&db_path);
    cmd.stderr(Stdio::null());

    let transport = TokioChildProcess::new(cmd).expect("spawn ctcl-mcp");
    let client = ().serve(transport).await.expect("MCP initialize handshake");

    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"ctcl.now"), "tools: {names:?}");
    assert!(names.contains(&"ctcl.create_trigger"), "tools: {names:?}");
    assert!(names.contains(&"ctcl.schedule_pulse"), "tools: {names:?}");
    assert_eq!(tools.len(), 15, "expected exactly the 15 implemented tools, got: {names:?}");
    // the three CTCL Web-only tools must NOT be silently claimed as available
    assert!(!names.contains(&"ctcl.inspect_boundary"));
    assert!(!names.contains(&"ctcl.resolve_temporal_context"));
    assert!(!names.contains(&"ctcl.plan_shared_instant"));

    let result = client.call_tool(CallToolRequestParams::new("ctcl.now")).await.expect("tools/call ctcl.now");
    assert_ne!(result.is_error, Some(true), "instant.read is granted by default, ctcl.now should succeed: {result:?}");
    let text = match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(text.contains("unix_ns"), "got: {text}");

    // triggers.write is off by default - a real refusal, not a mocked one.
    let refused = client
        .call_tool(
            CallToolRequestParams::new("ctcl.create_trigger").with_arguments(
                serde_json::json!({
                    "id": "trigger:e2e", "kind": "common_instant", "operator": ">=",
                    "target_value": 1.0, "action_kind": "notification", "action_target": "x"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("tools/call ctcl.create_trigger");
    assert_eq!(refused.is_error, Some(true), "triggers.write is off by default: {refused:?}");

    drop(client);
    let _ = std::fs::remove_file(&db_path);
}
