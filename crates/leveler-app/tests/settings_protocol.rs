//! The settings projection through the client protocol.
//!
//! The answer is built from the runtime's current configuration generation and
//! must never carry a secret. A reload changes the generation, and the next
//! answer must describe the new one.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::settings::UiSettingSource;
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, SessionId,
};
use leveler_execution::PermissionProfile;
use leveler_model::{ModelRef, ThinkingLevel};
use leveler_project::Layout;

fn write_home_config(home: &std::path::Path) -> std::path::PathBuf {
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        r#"
default_model = "deepseek/m"
thinking = "high"

[ui]
theme = "dark"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models."m"]
provider = "deepseek"
"#,
    )
    .unwrap();
    config
}

async fn ask_settings(
    client: &Arc<InProcessRuntimeClient>,
    session: &SessionId,
) -> leveler_client_protocol::settings::UiSettingsSnapshot {
    let query_id = CommandId::generate();
    let mut events = client.subscribe();
    client
        .send(ClientCommand::QuerySettings {
            requester_session_id: session.clone(),
            query_id: query_id.clone(),
        })
        .await
        .expect("send QuerySettings");
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await {
            Ok(Ok(RuntimeEvent::SettingsLoaded {
                query_id: answered,
                settings,
                ..
            })) if answered == query_id => return *settings,
            Ok(Ok(_)) | Ok(Err(_)) => continue,
            Err(error) => panic!("no settings answer: {error}"),
        }
    }
}

#[tokio::test]
async fn the_settings_projection_names_its_sources_without_secrets() {
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
        std::env::set_var("DEEPSEEK_API_KEY", "planted-settings-secret");
    }
    let config = write_home_config(home.path());
    let tmp = tempfile::tempdir().unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::parse("deepseek/m").unwrap();
    let session = app.create_session(&model, "settings").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
    ));

    let settings = ask_settings(&client, &session).await;
    assert_eq!(settings.config_version, 1);
    assert_eq!(settings.default_model.as_deref(), Some("deepseek/m"));
    assert_eq!(settings.resolved_default_model.as_ref(), Some(&model));
    assert_eq!(settings.thinking_default, Some(ThinkingLevel::High));
    assert_eq!(settings.theme_default.as_deref(), Some("dark"));
    assert_eq!(settings.context_soft_percent, None);
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == "deepseek")
        .expect("provider is projected");
    assert_eq!(provider.protocol, "openai_chat");
    assert_eq!(provider.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
    assert!(provider.api_key_configured);
    assert_eq!(provider.model_count, 1);
    assert_eq!(provider.source, UiSettingSource::GlobalConfig);
    assert!(
        settings
            .sources
            .iter()
            .any(|source| source.kind == "global" && source.path == config.display().to_string())
    );
    assert!(
        settings
            .sources
            .iter()
            .any(|source| source.kind == "project")
    );
    let wire = serde_json::to_string(&settings).unwrap();
    assert!(
        !wire.contains("planted-settings-secret"),
        "the settings answer leaked a key: {wire}"
    );

    // A reload changes the generation; the next answer describes the new one.
    std::fs::write(
        &config,
        r#"
default_model = "deepseek/m"
thinking = "low"

[ui]
theme = "dark"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models."m"]
provider = "deepseek"
"#,
    )
    .unwrap();
    let query_id = CommandId::generate();
    let mut events = client.subscribe();
    client
        .send(ClientCommand::ReloadConfig {
            query_id: Some(query_id.clone()),
        })
        .await
        .expect("send ReloadConfig");
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await {
            Ok(Ok(RuntimeEvent::ConfigReloaded {
                query_id: answered,
                outcome,
                ..
            })) if answered.as_ref() == Some(&query_id) => {
                assert!(
                    outcome == "applied" || outcome == "unchanged",
                    "reload outcome: {outcome}"
                );
                break;
            }
            Ok(Ok(_)) | Ok(Err(_)) => continue,
            Err(error) => panic!("no reload answer: {error}"),
        }
    }
    let settings = ask_settings(&client, &session).await;
    assert_eq!(settings.thinking_default, Some(ThinkingLevel::Low));
    assert!(settings.config_version >= 2);

    unsafe {
        std::env::remove_var("DEEPSEEK_API_KEY");
    }
}
