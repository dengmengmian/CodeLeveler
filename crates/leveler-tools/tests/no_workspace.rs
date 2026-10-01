use async_trait::async_trait;
use leveler_core::EnvSnapshot;
use leveler_execution::{PermissionProfile, RiskLevel, Workspace, WriteScope};
use leveler_tools::tool::{Tool, ToolError, ToolOutput};
use leveler_tools::{
    Capabilities, CapabilityPacks, ToolContext, ToolRegistry, core_surface, model_surface,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

struct ExternalTool(Arc<AtomicBool>);
#[async_trait]
impl Tool for ExternalTool {
    fn name(&self) -> &str {
        "external_local"
    }
    fn description(&self) -> &str {
        "A local extension with no independent capability declaration"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: ToolContext,
        _: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(ToolOutput::ok("executed"))
    }
}

fn context(cwd: &std::path::Path, home: &std::path::Path) -> ToolContext {
    let env = Arc::new(EnvSnapshot::new(
        [("HOME".into(), home.as_os_str().to_owned())],
        cwd.to_owned(),
        std::env::temp_dir(),
    ));
    ToolContext::without_workspace_with_environment(PermissionProfile::FullAccess, env)
}

#[tokio::test]
async fn no_workspace_dispatch_refuses_before_external_tool_body() {
    let dir = tempfile::tempdir().unwrap();
    let executed = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ExternalTool(executed.clone())));
    let result = registry
        .execute(
            "external_local",
            serde_json::json!({}),
            context(dir.path(), dir.path()),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, Err(ToolError::WorkspaceUnavailable)),
        "unbound dispatch must refuse: {result:?}"
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "no extension body may run without its required capability"
    );
}

#[tokio::test]
async fn no_workspace_refuses_real_fs_shell_and_elevation_without_inheriting_home_or_cwd() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("repo.txt"), "REPO_SENTINEL").unwrap();
    let secret = home.path().join("secret-test.txt");
    std::fs::write(&secret, "HOME_SENTINEL").unwrap();
    let mut ctx = context(repo.path(), home.path());
    ctx.policy.grant_unrestricted_fs();
    ctx.policy.grant_repository_git();
    assert_eq!(ctx.write_scope(), WriteScope::None);
    let registry = model_surface(
        CapabilityPacks::ALL,
        &Capabilities::in_process(ctx.execution.environment.clone()),
    );
    for (name, args) in [
        ("read_file", serde_json::json!({"path":secret})),
        ("read_file", serde_json::json!({"path":"repo.txt"})),
        ("list_files", serde_json::json!({})),
        ("find_files", serde_json::json!({"pattern":"*"})),
        ("grep", serde_json::json!({"pattern":"SENTINEL"})),
        (
            "write_file",
            serde_json::json!({"path":"created.txt","content":"x"}),
        ),
        (
            "run_command",
            serde_json::json!({"program":"sh","args":["-c","touch escaped.txt"]}),
        ),
        (
            "shell_command",
            serde_json::json!({"command":"touch escaped.txt"}),
        ),
        ("read_project_rules", serde_json::json!({})),
        ("git_status", serde_json::json!({})),
        ("git_diff", serde_json::json!({})),
        ("find_symbol", serde_json::json!({"query":"symbol"})),
        ("diagnostics", serde_json::json!({"path":"repo.txt"})),
        ("view_image", serde_json::json!({"path":secret})),
        ("load_skill", serde_json::json!({"name":"project"})),
    ] {
        assert!(
            matches!(
                registry
                    .execute(name, args, ctx.clone(), CancellationToken::new())
                    .await,
                Err(ToolError::WorkspaceUnavailable)
            ),
            "{name} must be structurally unavailable"
        );
    }
    assert!(!repo.path().join("escaped.txt").exists());
    assert!(!repo.path().join("created.txt").exists());
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "HOME_SENTINEL");
}

#[tokio::test]
async fn workspace_read_semantics_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "WORKSPACE_SENTINEL").unwrap();
    let ctx = ToolContext::new(
        Workspace::new(dir.path()).unwrap(),
        PermissionProfile::Assisted,
    );
    let registry = core_surface(&Capabilities::in_process(ctx.execution.environment.clone()));
    let output = registry
        .execute(
            "read_file",
            serde_json::json!({"path":"file.txt"}),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!output.is_error);
    assert!(output.content.contains("WORKSPACE_SENTINEL"));
}
