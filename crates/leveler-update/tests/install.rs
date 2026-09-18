//! Mechanical install test: a fixture archive replaces a fixture executable
//! in a temp directory. Nothing here touches the developer's real install.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use leveler_update::install::install_from_archive;
use leveler_update::version::parse_version;

fn host_triple() -> &'static str {
    leveler_update::host_target_triple().expect("tests run on a supported host")
}

/// Build a `.tar.gz` shaped exactly like a release: a top-level
/// `leveler-v{version}-{triple}/` directory holding the `leveler` executable.
fn fixture_archive(dir: &Path, version: &str, triple: &str) -> std::path::PathBuf {
    let name = format!("leveler-v{version}-{triple}");
    let inner = dir.join(&name);
    std::fs::create_dir_all(&inner).unwrap();
    let binary = inner.join("leveler");
    std::fs::write(
        &binary,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{version}\"; fi\n"),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&binary).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&binary, perms).unwrap();
    // README inside the archive must never be installed over anything.
    std::fs::write(inner.join("README.md"), "docs").unwrap();

    let archive = dir.join(format!("{name}.tar.gz"));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir)
        .arg(&name)
        .status()
        .expect("tar runs");
    assert!(status.success());
    archive
}

fn installed_script(dir: &Path, body: &str) {
    let path = dir.join("leveler");
    std::fs::write(&path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

#[test]
fn a_verified_archive_replaces_the_installed_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let install_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&install_dir).unwrap();
    // The "currently installed" binary.
    installed_script(&install_dir, "#!/bin/sh\necho 1.0.0\n");

    let triple = host_triple();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let archive = fixture_archive(&staging, "1.0.1", triple);

    let expected = parse_version("1.0.1").unwrap();
    install_from_archive(&archive, &install_dir, triple, &expected).unwrap();

    let installed = std::fs::read_to_string(install_dir.join("leveler")).unwrap();
    assert!(
        installed.contains("1.0.1"),
        "the installed binary must be the new one: {installed}"
    );
    // README from the archive never lands beside the binary.
    assert!(!install_dir.join("README.md").exists());
}

#[test]
fn an_archive_that_does_not_report_the_target_version_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let install_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&install_dir).unwrap();
    installed_script(&install_dir, "#!/bin/sh\necho 1.0.0\n");

    let triple = host_triple();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    // Archive says 1.0.1, but the binary inside reports 1.0.0.
    let archive = fixture_archive(&staging, "1.0.0", triple);

    let expected = parse_version("1.0.1").unwrap();
    let err = install_from_archive(&archive, &install_dir, triple, &expected).unwrap_err();
    assert!(
        matches!(err, leveler_update::UpdateError::Validation(_)),
        "{err}"
    );
    // The installed executable is untouched.
    let installed = std::fs::read_to_string(install_dir.join("leveler")).unwrap();
    assert!(installed.contains("1.0.0"), "{installed}");
}

#[test]
fn an_archive_without_the_binary_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let install_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&install_dir).unwrap();
    installed_script(&install_dir, "#!/bin/sh\necho 1.0.0\n");

    let triple = host_triple();
    let staging = tmp.path().join("staging");
    let inner = staging.join(format!("leveler-v1.0.1-{triple}"));
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("README.md"), "docs").unwrap();
    let archive = staging.join("empty.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg(format!("leveler-v1.0.1-{triple}"))
        .status()
        .unwrap();
    assert!(status.success());

    let expected = parse_version("1.0.1").unwrap();
    let err = install_from_archive(&archive, &install_dir, triple, &expected).unwrap_err();
    assert!(
        matches!(err, leveler_update::UpdateError::MissingBinary { .. }),
        "{err}"
    );
}

#[test]
fn restarting_a_missing_binary_fails_rather_than_succeeding() {
    // `exec` on a nonexistent path returns an error. The mechanical contract
    // is that restart never silently reports success.
    let missing = std::path::Path::new("/nonexistent/leveler-does-not-exist");
    let err = leveler_update::restart_with(missing, &[]).unwrap_err();
    assert!(matches!(err, leveler_update::UpdateError::Io(_)), "{err}");
}
