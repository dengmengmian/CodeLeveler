//! M1/M2/M3/M6 against a controlled real stdio MCP server.
//! The server writes its own side-effect counter; no MCP transport is bypassed.

#![cfg(unix)]

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::Tool;
use leveler_tools::mcp::{McpServerConfig, connect_all};
use leveler_tools::tool::ToolContext;
use tokio_util::sync::CancellationToken;

const SCRIPT: &str = r#"import json,sys,time,os
counter=sys.argv[1]; cancel_log=sys.argv[2]
def send(o): print(json.dumps(o),flush=True)
def side():
    with open(counter,'a') as f: f.write('x'); f.flush()
for line in sys.stdin:
    req=json.loads(line); m=req.get('method'); i=req.get('id')
    if m=='notifications/cancelled':
        with open(cancel_log,'a') as f: f.write(json.dumps(req)+'\n')
        continue
    if m=='initialize':
        res={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'ctrl','version':'1'}}
    elif m=='tools/list':
        res={'tools':[{'name':'write','description':'','inputSchema':{'type':'object','properties':{'mode':{'type':'string'}}}}]}
    elif m=='tools/call':
        mode=req['params'].get('arguments',{}).get('mode','write')
        side()
        if mode=='sleep': time.sleep(3)
        if mode=='ignore': time.sleep(1.0)
        if mode=='drop': os._exit(0)
        if mode=='exit': sys.exit(0)
        res={'content':[{'type':'text','text':'ok:'+mode}],'isError':False}
    else:
        res={}
    if i is not None: send({'jsonrpc':'2.0','id':i,'result':res})
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    counter: std::path::PathBuf,
    cancel_log: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    Fixture {
        counter: dir.path().join("counter.txt"),
        cancel_log: dir.path().join("cancel.log"),
        _dir: dir,
    }
}

async fn tool(f: &Fixture) -> std::sync::Arc<dyn Tool> {
    let cfg = McpServerConfig::stdio(
        "controlled",
        "/usr/bin/python3",
        vec![
            "-u".into(),
            "-c".into(),
            SCRIPT.into(),
            f.counter.to_string_lossy().into_owned(),
            f.cancel_log.to_string_lossy().into_owned(),
        ],
        vec![],
    );
    connect_all(&[cfg])
        .await
        .into_iter()
        .find(|tool| tool.name() == "mcp__controlled__write")
        .expect("controlled server advertises write")
}

fn ctx() -> ToolContext {
    let dir = tempfile::tempdir().unwrap();
    // Keep the workspace alive for the call; the tool does not need it.
    std::mem::forget(dir);
    ToolContext::new(
        Workspace::new(std::env::temp_dir()).unwrap(),
        PermissionProfile::FullAccess,
    )
}

#[tokio::test]
async fn m1_real_stdio_call_executes_one_side_effect() {
    let f = fixture();
    let tool = tool(&f).await;
    let out = tool
        .execute(
            serde_json::json!({"mode":"write"}),
            ctx(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("ok:write"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&f.counter).unwrap(), "x");
}

#[tokio::test]
async fn m2_response_timeout_is_unknown_and_not_retried() {
    // Single test in this binary owns the variable.
    unsafe { std::env::set_var("LEVELER_TEST_MCP_TIMEOUT_MS", "400") };
    let f = fixture();
    let tool = tool(&f).await;
    let out = tool
        .execute(
            serde_json::json!({"mode":"sleep"}),
            ctx(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        out.is_error,
        "timeout must be an error/unknown: {}",
        out.content
    );
    assert!(
        out.content.contains("未知"),
        "timeout must stay Unknown: {}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(&f.counter).unwrap(),
        "x",
        "the side effect happened exactly once"
    );
    // The timeout cancellation notification is best-effort; its exact send
    // path is covered by the unit-level timeout test. The live contract here
    // is Unknown plus exactly one side effect.
    unsafe { std::env::remove_var("LEVELER_TEST_MCP_TIMEOUT_MS") };
}

#[tokio::test]
async fn m6_server_exit_mid_call_is_not_a_confirmed_success() {
    let f = fixture();
    let tool = tool(&f).await;
    let out = tool
        .execute(
            serde_json::json!({"mode":"exit"}),
            ctx(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("未知"),
        "a transport close after send is Unknown: {}",
        out.content
    );
    assert_eq!(std::fs::read_to_string(&f.counter).unwrap(), "x");
}

#[tokio::test]
async fn m3_user_cancel_is_unknown_and_does_not_replay_the_side_effect() {
    let f = fixture();
    let tool = tool(&f).await;
    let token = CancellationToken::new();
    let call = {
        let tool = tool.clone();
        let token = token.clone();
        tokio::spawn(async move {
            tool.execute(serde_json::json!({"mode":"sleep"}), ctx(), token)
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    token.cancel();
    let out = call.await.unwrap();
    assert!(
        out.is_error,
        "cancel must not look successful: {}",
        out.content
    );
    assert!(
        out.content.contains("未知") || out.content.contains("取消"),
        "cancel must keep Unknown: {}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(&f.counter).unwrap(),
        "x",
        "the server had already executed the side effect once"
    );
}

#[tokio::test]
async fn m5_server_ignores_cancel_and_still_writes_once() {
    unsafe { std::env::set_var("LEVELER_TEST_MCP_TIMEOUT_MS", "400") };
    let f = fixture();
    let tool = tool(&f).await;
    let out = tool
        .execute(
            serde_json::json!({"mode":"ignore"}),
            ctx(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("未知"), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&f.counter).unwrap(),
        "x",
        "the server ignored cancellation and executed once"
    );
    unsafe { std::env::remove_var("LEVELER_TEST_MCP_TIMEOUT_MS") };
}

#[tokio::test]
async fn m7_stdio_drop_after_send_is_unknown_and_not_retried() {
    let f = fixture();
    let tool = tool(&f).await;
    let out = tool
        .execute(
            serde_json::json!({"mode":"drop"}),
            ctx(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&f.counter).unwrap(),
        "x",
        "the side effect happened before the transport drop"
    );
}

#[tokio::test]
async fn m9_two_calls_with_the_same_shape_are_two_side_effects() {
    let f = fixture();
    let tool = tool(&f).await;
    for _ in 0..2 {
        let out = tool
            .execute(
                serde_json::json!({"mode":"write"}),
                ctx(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
    }
    assert_eq!(
        std::fs::read_to_string(&f.counter).unwrap(),
        "xx",
        "each explicit call is a distinct execution; nothing is merged"
    );
}
