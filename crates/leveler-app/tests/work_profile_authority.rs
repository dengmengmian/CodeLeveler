//! The retired compatibility column cannot regain runtime authority.
use std::path::Path;

/// A client-facing projection must never emit a raw persisted `work_profile`.
///
/// Every historical value reads as the `single` compatibility marker
/// through `canonical_work_profile`. A projection that reads the column
/// directly lets a value the product no longer offers reach a client — and, on
/// a fork, be written straight back. A source scan, because the defect is a
/// call site, not a function.
#[test]
fn no_projection_emits_a_raw_persisted_work_profile() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(&src).expect("src is readable") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("source is readable");
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.contains("canonical_work_profile") {
                continue;
            }
            if trimmed.contains("work_profile: record.work_profile")
                || trimmed.contains("work_profile: Some(record.work_profile")
                || trimmed.contains("work_profile: session.work_profile")
            {
                violations.push(format!(
                    "{}:{}: reads the raw persisted work profile; go through \
                     `canonical_work_profile`",
                    path.display(),
                    number + 1
                ));
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn retired_profile_has_no_runtime_authority() {
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(
        !source.contains("fn capability_selection(work_profile"),
        "Tool Surface must be owned by capability disclosure"
    );
    assert!(
        !source.contains("work_profile == WorkProfile::Balanced"),
        "retired profile cannot control delegation or memory"
    );
}
