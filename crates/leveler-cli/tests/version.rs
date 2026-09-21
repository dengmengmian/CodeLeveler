//! Process-level contract for the self-update bootstrap version output.

use std::process::Command;

#[test]
fn top_level_version_flags_report_bootstrap_compatible_provenance() {
    let identity = leveler_core::BuildIdentity::current();
    let revision = identity.revision.get(..12).unwrap_or(&identity.revision);

    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_leveler"))
            .arg(flag)
            .output()
            .unwrap();

        assert!(output.status.success(), "{flag}: {output:?}");
        assert!(output.stderr.is_empty(), "{flag}: {output:?}");

        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.split_whitespace().next(),
            Some(env!("CARGO_PKG_VERSION")),
            "{flag}: {stdout:?}"
        );
        assert!(
            stdout.contains(revision),
            "{flag} must report commit provenance: {stdout:?}"
        );
    }
}
