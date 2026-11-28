//! Reference-aware media-store maintenance at daemon startup.
//!
//! The store is content-addressed, but content addressing alone cannot tell a
//! live image from an orphan: user attachments and tool images are persisted as
//! base64 parts inside transcripts/turns/events, not as a separate ref table.
//! This module derives the live hash set from the durable payloads and hands it
//! to [`leveler_media::MediaStore::sweep`].
//!
//! Safety rules:
//! - a payload that cannot be parsed or decoded makes the caller skip the sweep;
//!   a corrupt row must never make a live object look unreferenced;
//! - the sweep only removes objects older than the grace window, which protects
//!   a just-staged attachment that has no durable row yet;
//! - the daemon runs this before accepting work, so no turn can import a new
//!   object concurrently.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use leveler_media::{MediaStore, SweepPolicy, SweepReport};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Live hashes derived from durable payloads, plus the count of payloads that
/// could not be read. A non-zero `unreadable` forbids the sweep.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MediaReferences {
    pub hashes: HashSet<String>,
    pub unreadable: usize,
}

/// Collect every media hash a durable payload references.
pub(crate) fn collect_media_references(payloads: &[String]) -> MediaReferences {
    let mut references = MediaReferences::default();
    for payload in payloads {
        match serde_json::from_str::<Value>(payload) {
            Ok(value) => visit_value(&value, &mut references),
            Err(_) => references.unreadable += 1,
        }
    }
    references
}

fn visit_value(value: &Value, references: &mut MediaReferences) {
    match value {
        Value::Object(object) => {
            // A persisted image part: {"type":"image","source":{"kind":"base64",...}}.
            if object.get("type").and_then(Value::as_str) == Some("image")
                && let Some(Value::Object(source)) = object.get("source")
                && source.get("kind").and_then(Value::as_str) == Some("base64")
                && let Some(data) = source.get("data").and_then(Value::as_str)
            {
                match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(bytes) => {
                        references
                            .hashes
                            .insert(format!("{:x}", Sha256::digest(&bytes)));
                    }
                    Err(_) => references.unreadable += 1,
                }
            }
            // A stored ref (tool-image event, context snapshot projection):
            // {"sha256": "...", "mime_type": "..."}.
            if let (Some(sha256), Some(_)) = (
                object.get("sha256").and_then(Value::as_str),
                object.get("mime_type").and_then(Value::as_str),
            ) && is_sha256_hex(sha256)
            {
                references.hashes.insert(sha256.to_owned());
            }
            for child in object.values() {
                visit_value(child, references);
            }
        }
        Value::Array(items) => {
            for item in items {
                visit_value(item, references);
            }
        }
        _ => {}
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The sweep policy a daemon applies. `[storage] media_max_bytes` and
/// `media_grace_hours` override the built-in policy; an invalid or absent
/// global config falls back to the conservative defaults rather than enabling
/// a more aggressive sweep.
pub(crate) fn sweep_policy() -> SweepPolicy {
    let defaults = SweepPolicy::default();
    let Ok(config) = crate::global_config::GlobalConfig::load() else {
        return defaults;
    };
    SweepPolicy {
        max_bytes: config.media_max_bytes().or(defaults.max_bytes),
        min_age: config
            .media_grace_hours()
            .map(|hours| Duration::from_secs(hours.saturating_mul(3600)))
            .unwrap_or(defaults.min_age),
    }
}

/// Run one sweep from `root`. Kept separate from the application so the
/// scanner and the store policy can be exercised without a daemon.
pub(crate) fn sweep(
    root: PathBuf,
    references: &MediaReferences,
    policy: SweepPolicy,
) -> Result<SweepReport, String> {
    if references.unreadable > 0 {
        return Err(format!(
            "{} durable media payload(s) could not be read; refusing to treat them as unreferenced",
            references.unreadable
        ));
    }
    MediaStore::new(root)
        .sweep(&references.hashes, SystemTime::now(), policy)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(content: &str) -> String {
        format!(r#"{{"role":"user","content":[{content}]}}"#)
    }

    #[test]
    fn reads_a_base64_image_part() {
        let data = "aGVsbG8=";
        let hash = format!("{:x}", Sha256::digest(b"hello"));
        let references = collect_media_references(&[payload(&format!(
            r#"{{"type":"image","source":{{"kind":"base64","media_type":"image/png","data":"{data}"}}}}"#
        ))]);
        assert_eq!(references.unreadable, 0);
        assert_eq!(references.hashes, HashSet::from([hash]));
    }

    #[test]
    fn reads_a_stored_ref_and_ignores_an_unrelated_hash() {
        let hash = "ab".repeat(32);
        let message = format!(
            r#"{{"images":[{{"sha256":"{hash}","mime_type":"image/png"}}],"note":"deadbeef"}}"#
        );
        let references = collect_media_references(&[message]);
        assert_eq!(references.hashes, HashSet::from([hash]));
        assert_eq!(references.unreadable, 0);
    }

    #[test]
    fn reads_a_nested_context_snapshot() {
        let data = "aGVsbG8=";
        let event = format!(
            r#"{{"payload":{{"messages":[{{"role":"user","content":[{{"type":"image","source":{{"kind":"base64","media_type":"image/png","data":"{data}"}}}}]}}]}},"type":"context_snapshot"}}"#
        );
        let references = collect_media_references(&[event]);
        assert_eq!(
            references.hashes,
            HashSet::from([format!("{:x}", Sha256::digest(b"hello"))])
        );
    }

    #[test]
    fn malformed_or_undecodable_payloads_forbid_a_sweep() {
        let references = collect_media_references(&[
            "{not json".to_string(),
            payload(
                r#"{"type":"image","source":{"kind":"base64","media_type":"image/png","data":"!"}}"#,
            ),
        ]);
        assert_eq!(references.unreadable, 2);
        let error = sweep(
            std::env::temp_dir().join("leveler-media-gc-unused"),
            &references,
            SweepPolicy::default(),
        )
        .unwrap_err();
        assert!(error.contains("refusing"), "{error}");
    }

    #[test]
    fn a_referenced_object_survives_and_an_orphan_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let stored = store
            .import_rgba(2, 2, &[10, 20, 30, 255].repeat(4))
            .unwrap();
        let orphan = store
            .import_rgba(3, 2, &[30, 20, 10, 255].repeat(6))
            .unwrap();
        age_file(&stored.path, Duration::from_secs(3600));
        age_file(&orphan.path, Duration::from_secs(3600));
        let data =
            base64::engine::general_purpose::STANDARD.encode(std::fs::read(&stored.path).unwrap());
        let payload = format!(
            r#"{{"role":"user","content":[{{"type":"image","source":{{"kind":"base64","media_type":"image/png","data":"{data}"}}}}]}}"#
        );
        let references = collect_media_references(&[payload]);

        let report = sweep(
            dir.path().to_path_buf(),
            &references,
            SweepPolicy {
                max_bytes: None,
                min_age: Duration::from_secs(60),
            },
        )
        .unwrap();

        assert!(stored.path.exists());
        assert!(!orphan.path.exists());
        assert_eq!(report.removed_objects, 1);
    }

    fn age_file(path: &std::path::Path, age: Duration) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
    }
}
