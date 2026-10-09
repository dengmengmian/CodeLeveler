//! The resident config (`$LEVELER_HOME/config.toml`) is the OTHER way a model
//! profile reaches the runtime. The repo's YAML catalog validates each file on
//! load; a profile declared in `config.toml` must not slip past the same fact
//! check, because a declaration that reserves the whole window for completion
//! leaves no input capacity — `hard_capacity()` then has no bound to report and
//! the mandatory-fold contract silently disappears instead of failing where the
//! misdeclaration was made.
//!
//! One test per target on purpose: the installed home is process-wide
//! (first-install-wins), so this file owns exactly one `config.toml`. The
//! consistent-declaration side is covered by the model/provider unit tests and
//! by every other `Application::assemble` in this crate.

use std::sync::OnceLock;

use leveler_app::Application;
use leveler_project::Layout;

fn home_with(config_toml: &str) -> &'static std::path::Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config_toml).unwrap();
        let _ = leveler_core::install_environment(leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                dir.path().as_os_str().to_os_string(),
            )],
            std::env::temp_dir(),
            std::env::temp_dir(),
        ));
        unsafe {
            std::env::set_var("LEVELER_HOME", dir.path());
        }
        dir
    })
    .path()
}

#[test]
fn a_resident_profile_with_no_input_capacity_is_refused() {
    let home = home_with(
        r#"
[providers.mock]
base_url = "http://127.0.0.1:9"

[models.bad]
provider = "mock"
model_id = "mock-bad"
context_window = 131072
reliable_context = 24000
max_output_tokens = 393216
"#,
    );
    let repo = home.join("repo");
    std::fs::create_dir_all(repo.join("configs/providers")).unwrap();
    std::fs::create_dir_all(repo.join("configs/models")).unwrap();
    let error = Application::assemble(Layout::from_parts(
        repo.clone(),
        repo.join("configs"),
        repo.join("state"),
    ))
    .err()
    .expect("a declaration that leaves no input capacity must be refused");
    let message = error.to_string();
    assert!(
        message.contains("393216") && message.contains("131072"),
        "the refusal must name the declared numbers: {message}"
    );
}
