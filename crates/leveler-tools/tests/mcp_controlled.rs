//! M1/M2/M3/M4/M5/M6/M7/M9 against a controlled real stdio MCP server.
//! The server writes its own side-effect counter; no MCP transport is bypassed.
//!
//! M4 additionally needs the server to keep READING stdin while one tools/call
//! is in progress: a Python worker thread does the controlled work, the main
//! thread receives the client's `notifications/cancelled`, and both sides write
//! their own facts to a log. That is what proves the real cancellation reached
//! the server, carried the same JSON-RPC id, and stopped the side effect.

#![cfg(unix)]

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::Tool;
use leveler_tools::mcp::{McpServerConfig, connect_all, connect_all_with_authority};
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

/// M4: the main thread keeps reading stdin while a worker thread holds one
/// `tools/call` in controlled work. The worker commits a side effect only if
/// nobody stops it; a received `notifications/cancelled` stops it and records
/// the elapsed time. Both the request id the server saw on `tools/call` and the
/// requestId on the cancellation are written to the same append-only log.
const SCRIPT_M4: &str = r#"import json,sys,time,os,threading
counter=sys.argv[1]; log_path=sys.argv[2]
stop=threading.Event()
def send(o): print(json.dumps(o),flush=True)
def log(fact):
    with open(log_path,'a') as f: f.write(json.dumps(fact)+'\n'); f.flush()
def worker(req_id,mode,started):
    log({'event':'started','request_id':req_id,'pid':os.getpid(),'mode':mode})
    cancelled=stop.wait(8.0)
    elapsed=int((time.time()-started)*1000)
    if cancelled:
        log({'event':'stopped','request_id':req_id,'elapsed_ms':elapsed,'side_effect':False})
        return
    with open(counter,'a') as f: f.write('x'); f.flush()
    log({'event':'completed','request_id':req_id,'elapsed_ms':elapsed,'side_effect':True})
    send({'jsonrpc':'2.0','id':req_id,'result':{'content':[{'type':'text','text':'ok:'+mode}],'isError':False}})
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    req=json.loads(line); m=req.get('method'); i=req.get('id')
    if m=='notifications/cancelled':
        p=req.get('params',{})
        log({'event':'cancelled','request_id':p.get('requestId'),'reason':p.get('reason'),'raw':req})
        stop.set()
        continue
    if m=='notifications/initialized': continue
    if m=='initialize':
        res={'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'ctrl','version':'1'}}
    elif m=='tools/list':
        res={'tools':[{'name':'write','description':'','inputSchema':{'type':'object','properties':{'mode':{'type':'string'}}}}]}
    elif m=='tools/call':
        mode=req['params'].get('arguments',{}).get('mode','write')
        threading.Thread(target=worker,args=(i,mode,time.time()),daemon=True).start()
        continue
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

/// A stdio tool whose server can read stdin concurrently with a running
/// `tools/call`.
async fn m4_tool(f: &Fixture) -> std::sync::Arc<dyn Tool> {
    let cfg = McpServerConfig::stdio(
        "controlled",
        "/usr/bin/python3",
        vec![
            "-u".into(),
            "-c".into(),
            SCRIPT_M4.into(),
            f.counter.to_string_lossy().into_owned(),
            f.cancel_log.to_string_lossy().into_owned(),
        ],
        vec![],
    );
    // The application connects a FullAccess session's MCP tools under that same
    // launch authority (see `Application::engine_for_session`). This is the
    // production wiring: the cached client that listed the tools is the client
    // that executes `tools/call`, so `cancel_pending` addresses the server that
    // is actually running the call.
    connect_all_with_authority(&[cfg], leveler_core::environment(), true)
        .await
        .into_iter()
        .find(|tool| tool.name() == "mcp__controlled__write")
        .expect("controlled server advertises write")
}

/// Read the append-only server fact log, skipping a partial trailing line.
fn read_facts(path: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

async fn wait_for_fact(
    path: &std::path::Path,
    event: &str,
    subject: &'static str,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(fact) = read_facts(path)
            .into_iter()
            .find(|fact| fact["event"] == event)
        {
            return fact;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the MCP server never recorded `{event}` ({subject}); facts={:?}",
            read_facts(path)
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// M4: a real MCP server observes the client's `notifications/cancelled` for
/// the in-flight `tools/call`, records the same request id it saw on the call,
/// and stops before committing the side effect. The client-visible outcome is
/// Unknown/cancelled, never success. Server facts and client result agree.
#[tokio::test]
async fn m4_real_server_observes_cancellation_and_stops_work() {
    let f = fixture();
    let tool = m4_tool(&f).await;
    let token = CancellationToken::new();
    let call = {
        let tool = tool.clone();
        let token = token.clone();
        tokio::spawn(async move {
            tool.execute(serde_json::json!({"mode":"controlled"}), ctx(), token)
                .await
                .unwrap()
        })
    };

    // The server is now inside a real tools/call: it logged the JSON-RPC id it
    // received before the worker began its controlled work.
    let started = wait_for_fact(&f.cancel_log, "started", "tools/call began").await;
    let call_id = started["request_id"]
        .as_u64()
        .expect("the server saw a numeric tools/call id");
    assert!(call_id > 0, "{started}");
    assert_eq!(started["side_effect"], serde_json::Value::Null);
    assert!(
        !f.counter.exists(),
        "the worker must not have committed the side effect at `started`"
    );

    // The production cancellation path: dropping the cancellation token makes
    // `Tool::execute` call `cancel_pending`, which sends
    // `notifications/cancelled` with the in-flight request id.
    token.cancel();
    let out = call.await.unwrap();

    // The client-visible outcome is Unknown/cancelled, never a success.
    assert!(
        out.is_error,
        "a cancelled call must not look successful: {}",
        out.content
    );
    assert!(
        out.content.contains("取消") || out.content.contains("未知"),
        "cancel must stay Unknown/cancelled: {}",
        out.content
    );

    // Server-side evidence: the cancellation arrived, named the SAME request
    // id the server saw on `tools/call`, and the worker stopped before the
    // side effect.
    let cancelled = wait_for_fact(&f.cancel_log, "cancelled", "notifications/cancelled").await;
    assert_eq!(
        cancelled["request_id"].as_u64(),
        Some(call_id),
        "the cancellation requestId must equal the tools/call id: {cancelled}"
    );
    assert_eq!(cancelled["reason"], "user cancelled", "{cancelled}");
    let stopped = wait_for_fact(&f.cancel_log, "stopped", "worker observed cancel").await;
    assert_eq!(stopped["request_id"].as_u64(), Some(call_id), "{stopped}");
    assert_eq!(
        stopped["side_effect"],
        serde_json::Value::Bool(false),
        "the worker must record that it did not commit a side effect: {stopped}"
    );
    assert!(
        stopped["elapsed_ms"].as_u64().unwrap_or(0) > 0,
        "the worker must record that it actually did controlled work: {stopped}"
    );
    assert!(
        !f.counter.exists() || std::fs::read_to_string(&f.counter).unwrap().is_empty(),
        "cancellation must prevent the side effect"
    );

    if let Some(dir) = std::env::var_os("LEVELER_M4_EVIDENCE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&f.cancel_log, dir.join("m4_server_facts.jsonl")).unwrap();
        let summary = serde_json::json!({
            "server_pid": started["pid"],
            "tools_call_request_id": call_id,
            "cancelled_request_id": cancelled["request_id"],
            "stopped_request_id": stopped["request_id"],
            "stopped_side_effect": stopped["side_effect"],
            "stopped_elapsed_ms": stopped["elapsed_ms"],
            "client_is_error": out.is_error,
            "client_content": out.content,
            "side_effect_counter_exists": f.counter.exists(),
        });
        std::fs::write(
            dir.join("m4_summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
    }
}
