//! Keeps `schemas/*.json` in step with the Rust protocol types.
//!
//! The schemas are the contract non-Rust clients consume — the mobile APP
//! generates its command and event types from them rather than hand-writing a
//! second dialect that drifts. Nothing regenerates them automatically, so this
//! test is what makes drift loud: add a `ClientCommand` variant without
//! re-exporting and it fails here, naming the command to run.
//!
//! Regenerate with:
//!
//! ```text
//! UPDATE_SCHEMAS=1 cargo test -p leveler-client-protocol --features schema
//! ```
#![cfg(feature = "schema")]

use std::path::{Path, PathBuf};

use leveler_client_protocol::{ClientCommand, RuntimeEvent, UiSessionSnapshot};
use schemars::schema_for;

/// Repository-root `schemas/`, resolved from this crate's manifest directory
/// so the test does not depend on the working directory `cargo test` uses.
fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("schemas")
}

/// Pretty-printed so a schema diff is reviewable in a pull request. schemars
/// orders its maps deterministically, so byte comparison is stable.
fn render(schema: &schemars::schema::RootSchema) -> String {
    let mut json = serde_json::to_string_pretty(schema).expect("schema serializes");
    json.push('\n');
    json
}

fn check(file_name: &str, generated: String) {
    let path = schemas_dir().join(file_name);

    if std::env::var_os("UPDATE_SCHEMAS").is_some() {
        std::fs::create_dir_all(schemas_dir()).expect("create schemas dir");
        std::fs::write(&path, generated).expect("write schema");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} is missing ({error}).\n\
             Regenerate: UPDATE_SCHEMAS=1 cargo test -p leveler-client-protocol --features schema",
            path.display()
        )
    });

    assert_eq!(
        committed,
        generated,
        "\n{} is stale: the Rust types changed but the committed schema did not.\n\
         Regenerate and commit: UPDATE_SCHEMAS=1 cargo test -p leveler-client-protocol --features schema\n",
        path.display()
    );
}

#[test]
fn client_command_schema_is_current() {
    check(
        "client_command.schema.json",
        render(&schema_for!(ClientCommand)),
    );
}

#[test]
fn runtime_event_schema_is_current() {
    check(
        "runtime_event.schema.json",
        render(&schema_for!(RuntimeEvent)),
    );
}

#[test]
fn ui_session_snapshot_schema_is_current() {
    check(
        "ui_session_snapshot.schema.json",
        render(&schema_for!(UiSessionSnapshot)),
    );
}
