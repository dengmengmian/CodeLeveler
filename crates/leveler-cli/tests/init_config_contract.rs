//! `leveler init` and `leveler config show` are the two commands every scripted
//! setup touches, and they are the two that could write to `$HOME` without
//! being asked. Both contracts are asserted here as process behaviour, not as
//! an internal function call: the promise is about what the binary does to a
//! user's home directory.

use std::path::Path;
use std::process::Command;

/// Run the binary with every home-shaped environment variable pointed at an
/// empty directory, so a write is visible as a new file and never escapes.
fn leveler(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .args(args)
        .env("HOME", home)
        .env("LEVELER_HOME", home.join(".leveler"))
        .env("LEVELER_CONFIG", home.join(".leveler/config.toml"))
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("LEVELER_API_KEY")
        .output()
        .unwrap_or_else(|err| panic!("could not run leveler {args:?}: {err}"))
}

fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "leveler-init-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn init_without_a_terminal_prints_the_template_and_writes_nothing() {
    let home = scratch("print");
    // stdin and stdout are pipes here, which is exactly the non-interactive
    // case: `leveler init > somewhere`.
    let output = leveler(&home, &["init"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "init must succeed when it only prints: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("[providers.") || stdout.contains("default_model"),
        "the default template is the product of a non-interactive init: {stdout:?}"
    );
    assert!(
        !home.join(".leveler/config.toml").exists(),
        "a non-interactive init must not create a config file"
    );
    assert_eq!(
        std::fs::read_dir(&home).unwrap().count(),
        0,
        "a non-interactive init must not touch the home directory at all"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn init_refuses_to_overwrite_an_existing_config() {
    let home = scratch("existing");
    std::fs::create_dir_all(home.join(".leveler")).unwrap();
    let config = home.join(".leveler/config.toml");
    std::fs::write(&config, "default_model = \"kept/unchanged\"\n").unwrap();

    let output = leveler(&home, &["init"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(
        output.status.code(),
        Some(1),
        "an existing config is a refusal, not a rewrite: {stdout}"
    );
    assert!(
        stdout.contains("already exists") || stdout.contains("never overwrites"),
        "the refusal says why: {stdout:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "default_model = \"kept/unchanged\"\n",
        "the existing config must be byte-identical afterwards"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn config_show_prints_the_resolved_configuration_without_a_config_file() {
    let home = scratch("show");
    let output = leveler(&home, &["config", "show"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "config show must work with no config file present: {stdout}{stderr}"
    );
    let lowered = stdout.to_lowercase();
    assert!(
        lowered.contains("providers") && lowered.contains("models"),
        "the resolved configuration is printed: {stdout:?}"
    );
    assert!(
        lowered.contains("co-author") || lowered.contains("vcs"),
        "the VCS section is part of the resolved configuration: {stdout:?}"
    );
    assert!(
        !stdout.contains("sk-"),
        "config show must never print a secret value: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
