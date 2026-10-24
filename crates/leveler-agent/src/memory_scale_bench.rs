//! Ignored scale benchmark for the memory context builder.
//!
//! This is the measurement harness for the Stage 3.1 performance work. It
//! builds a Markdown store in the shape a long-running project actually
//! reaches — current documents, per-record history, trust receipts,
//! tombstones, pending proposals and a reconciled FTS5 index — and then drives
//! the production [`MemoryContextBuilder`] over it.
//!
//! Run it explicitly; it is not part of the normal test suite:
//!
//! ```text
//! cargo test -p leveler-agent --lib memory_scale -- --ignored --nocapture
//! ```
//!
//! `MEMORY_SCALE` overrides the record counts (default `100,1000,10000`).
//! Nothing here changes production behaviour, and nothing is asserted against a
//! wall-clock threshold: the numbers are evidence, not a gate. The gate lives in
//! the delivery report.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_memory::v2;
use leveler_memory::{MemoryLanes, MemoryRoots, MemoryService, ServiceScope};

use crate::coding::policy::ResolvedContextPolicy;
use crate::memory_context::MemoryContextBuilder;

/// A query that matches every synthetic record: the worst case for recall.
const HOT_QUERY: &str = "发布流程 PermissionProfile";
/// A query that matches one record: the cheap case.
const RARE_QUERY: &str = "rare_marker_7f31";

const RECALL_SAMPLES: usize = 60;
const COMPONENT_SAMPLES: usize = 12;

fn scales() -> Vec<usize> {
    match std::env::var("MEMORY_SCALE") {
        Ok(value) => value
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect(),
        Err(_) => vec![100, 1_000, 10_000],
    }
}

#[test]
#[ignore = "scale benchmark; run explicitly with --ignored --nocapture"]
fn memory_context_recall_scale() {
    for count in scales() {
        measure(count);
    }
}

/// How many synthetic agent-experience records the fixture contains.
///
/// Zero is the shipped state: experience extraction is off, so a store holds
/// user facts only. A non-zero value measures the real cost of a second
/// retrieval lane that actually has something to retrieve.
fn experience_records() -> usize {
    std::env::var("MEMORY_EXPERIENCE")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn measure(count: usize) {
    let directory = tempfile::tempdir().expect("temp dir");
    let roots = build_store(directory.path(), count);
    println!("\n=== memories: {count} ===");

    // A cold handle: exactly the work a fresh process pays for this scope.
    let cold = Instant::now();
    let cold_service = v2::MemoryService::open(roots.clone()).expect("cold open");
    let cold_open = cold.elapsed();
    drop(cold_service);
    let warm = Instant::now();
    let _warm_service = v2::MemoryService::open(roots.clone()).expect("warm open");
    let warm_open = warm.elapsed();

    let service = Arc::new(MemoryService::new(
        MemoryRoots::new().with_project(roots.clone()),
    ));
    let builder = MemoryContextBuilder::new(service.clone(), MemoryLanes::ALL);
    let policy = ResolvedContextPolicy::default();

    let catalog_cold = timed(|| {
        builder.catalog_text();
    });
    let catalog_warm = timed(|| {
        builder.catalog_text();
    });

    let first = timed(|| {
        builder.recall(HOT_QUERY, &policy);
    });

    let before = service.lifecycle_metrics();
    let hot_times: Vec<Duration> = (0..RECALL_SAMPLES)
        .map(|index| {
            let query = if index % 2 == 0 {
                HOT_QUERY
            } else {
                RARE_QUERY
            };
            timed(|| {
                builder.recall(query, &policy);
            })
        })
        .collect();
    let after = service.lifecycle_metrics();

    // The shipped configuration: agent experience is off by default, so only
    // the user lane is recalled. Measured separately so the delivery report
    // does not hide a second lexical pass inside one number.
    let user_only = MemoryContextBuilder::new(service.clone(), MemoryLanes::USER_ONLY);
    let before_user_only = service.lifecycle_metrics();
    let user_only_times: Vec<Duration> = (0..RECALL_SAMPLES)
        .map(|index| {
            let query = if index % 2 == 0 {
                HOT_QUERY
            } else {
                RARE_QUERY
            };
            timed(|| {
                user_only.recall(query, &policy);
            })
        })
        .collect();
    let after_user_only = service.lifecycle_metrics();

    let standing = median(COMPONENT_SAMPLES, || {
        service
            .standing_candidates(ServiceScope::Project, 8)
            .expect("standing")
    });
    let search = median(COMPONENT_SAMPLES, || search_hits(&service, 12));
    // The same query with a one-row answer isolates the fixed cost (FTS query
    // plus store lock) from the per-candidate Markdown revalidation.
    let search_one = median(COMPONENT_SAMPLES, || search_hits(&service, 1));
    let single_read = median(COMPONENT_SAMPLES, || {
        service
            .read_injectable(
                &v2::Actor::External,
                ServiceScope::Project,
                "mem_scale00000",
            )
            .expect("read")
    });
    let batch_read = median(COMPONENT_SAMPLES, || {
        let ids: Vec<String> = (0..48)
            .map(|sequence| format!("mem_scale{sequence:05}"))
            .collect();
        service
            .read_many_injectable(&v2::Actor::External, ServiceScope::Project, &ids)
            .expect("batch read")
    });

    let rebuild = Instant::now();
    service
        .refresh_index(ServiceScope::Project)
        .expect("index rebuild");
    let rebuild = rebuild.elapsed();

    println!(
        "cold_open={cold_open:?} warm_open={warm_open:?} catalog_cold={:?} catalog_warm={:?}",
        catalog_cold, catalog_warm
    );
    println!(
        "first_recall={first:?} recall_p50={:?} recall_p95={:?} recall_max={:?}",
        percentile(&hot_times, 50),
        percentile(&hot_times, 95),
        hot_times.iter().max().copied().unwrap_or_default()
    );
    println!(
        "user_only_recall_p50={:?} user_only_recall_p95={:?} user_only_reads={}",
        percentile(&user_only_times, 50),
        percentile(&user_only_times, 95),
        after_user_only.reads - before_user_only.reads
    );
    println!(
        "standing={standing:?} search12={search:?} search1={search_one:?} single_read={single_read:?} batch_read48={batch_read:?} index_rebuild={rebuild:?}"
    );
    println!(
        "store_opens={} journal_recoveries={} orphan_cleanups={} reads={} (during {} recalls)",
        after.opens - before.opens,
        after.journal_recoveries - before.journal_recoveries,
        after.orphan_cleanups - before.orphan_cleanups,
        after.reads - before.reads,
        RECALL_SAMPLES
    );
    println!(
        "store_bytes={} rss_kb={}",
        directory_size(&roots.state),
        resident_kb()
    );
}

fn search_hits(service: &MemoryService, limit: usize) -> Vec<(leveler_memory::MemoryView, f64)> {
    service
        .search(
            &v2::Actor::External,
            ServiceScope::Project,
            MemoryLanes::USER_ONLY,
            HOT_QUERY,
            limit,
        )
        .expect("search")
}

fn timed(run: impl FnOnce()) -> Duration {
    let started = Instant::now();
    run();
    started.elapsed()
}

fn median<T>(runs: usize, mut run: impl FnMut() -> T) -> Duration {
    run();
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        run();
        times.push(started.elapsed());
    }
    times.sort();
    times[times.len() / 2]
}

fn percentile(times: &[Duration], percent: usize) -> Duration {
    let mut sorted = times.to_vec();
    sorted.sort();
    let index = (sorted.len() * percent / 100).min(sorted.len() - 1);
    sorted[index]
}

fn directory_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total += directory_size(&entry.path());
        } else {
            total += metadata.len();
        }
    }
    total
}

fn resident_kb() -> u64 {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Write a store in the shape a long-running project reaches.
fn build_store(directory: &Path, count: usize) -> v2::ScopeRoots {
    let roots = v2::ScopeRoots::new(
        v2::MemoryScope::Project,
        directory.join("current"),
        directory.join("state"),
    );
    let paths = v2::StorePaths::prepare(&roots).expect("store layout");
    let seed = seed_record(&roots);

    let mut effective = Vec::with_capacity(count);
    let experience_count = experience_records();
    for sequence in 0..count {
        let mut record = seed.clone();
        record.frontmatter.id = v2::MemoryId::parse(format!("mem_scale{sequence:05}")).expect("id");
        record.frontmatter.title = format!("合成记忆 {sequence} 发布流程");
        record.frontmatter.subject = Some(format!("subject {sequence}"));
        record.frontmatter.subject_key = Some(format!("subject_{sequence}"));
        record.frontmatter.revision = 2;
        record.frontmatter.pinned = sequence % 97 == 0;
        record.frontmatter.pinned_by = record.frontmatter.pinned.then_some(v2::PinOwner::User);
        if sequence < experience_count {
            record.frontmatter.kind = v2::MemoryKind::Experience;
            record.frontmatter.authority = v2::MemoryAuthority::RuntimeObserved;
            record.frontmatter.pinned = false;
            record.frontmatter.pinned_by = None;
            record.frontmatter.subject_key = None;
            record.frontmatter.experience = Some(v2::Experience {
                category: "root cause".into(),
                applies_when: "发布流程校验失败时".into(),
                invalid_if: None,
                environment: Default::default(),
                evidence: vec![
                    v2::ExperienceEvidence {
                        kind: "failure".into(),
                        reference: "failure-1".into(),
                    },
                    v2::ExperienceEvidence {
                        kind: "resolution".into(),
                        reference: "resolution-1".into(),
                    },
                ],
            });
        }
        record.body = if sequence == 0 {
            format!(
                "第 {sequence} 条记忆：发布流程使用 cargo test 校验 PermissionProfile。\n{}",
                RARE_QUERY
            )
        } else {
            format!(
                "第 {sequence} 条记忆：发布流程使用 cargo test 校验 PermissionProfile，\
                 构建脚本位于 scripts/ 下，产物写入 target/。"
            )
        };
        if let Some(value) = record.frontmatter.value.as_mut() {
            *value = record.body.clone();
        }

        // History: every record keeps the revision it replaced.
        let mut previous = record.clone();
        previous.frontmatter.revision = 1;
        previous.body = format!("第 {sequence} 条记忆的旧版本。");
        let history = paths
            .history_dir
            .join(record.frontmatter.id.as_str())
            .join("1.md");
        write(
            &history,
            &v2::frontmatter::render(&previous).expect("history"),
        );

        let bytes = v2::frontmatter::render(&record).expect("render");
        let digest = v2::sha256_hex(&bytes);
        let effective_authority = record.frontmatter.authority;
        write(&paths.current_file(&record.frontmatter.id), &bytes);
        let receipt = v2::Receipt::from_record(&record, &digest);
        write(
            &paths.receipt_file(&record.frontmatter.id),
            &receipt.to_json().expect("receipt"),
        );
        effective.push(v2::EffectiveRecord {
            record,
            digest: digest.clone(),
            cas: v2::Cas {
                revision: 2,
                digest,
            },
            effective_authority,
        });
    }

    // Retired identities and proposals: the store shape a project accumulates.
    for sequence in 0..(count / 100).max(1) {
        let id = v2::MemoryId::parse(format!("mem_gone{sequence:05}")).expect("id");
        let tombstone = v2::Tombstone {
            id,
            kind: v2::MemoryKind::UserFact,
            scope: v2::MemoryScope::Project,
            subject_key_sha256: None,
            last_revision: 3,
            content_sha256: v2::sha256_hex_text("forgotten"),
            forgotten_at: "2026-01-03T00:00:00Z".into(),
        };
        write(
            &paths.tombstone_file(&tombstone.id),
            &serde_json::to_vec(&tombstone).expect("tombstone"),
        );
        let mut pending = seed.clone();
        pending.frontmatter.id = v2::MemoryId::parse(format!("mem_wait{sequence:05}")).expect("id");
        pending.frontmatter.status = v2::MemoryStatus::Pending;
        pending.frontmatter.proposal = Some(v2::Proposal {
            operation: v2::Operation::Create,
            target_ids: Vec::new(),
            expected_revision: None,
            expected_digest: None,
            proposed_authority: Some(v2::MemoryAuthority::ModelInferred),
            reason: "awaiting review".into(),
        });
        write(
            &paths.pending_file(&pending.frontmatter.id),
            &v2::frontmatter::render(&pending).expect("pending"),
        );
    }

    let index = v2::MemoryIndex::open(&paths.index_dir).expect("index");
    index.reconcile(&effective).expect("reconcile");
    roots
}

/// One record written by the real store, used as the fixture template.
///
/// Taking the template from a real write keeps the fixture's timestamps,
/// schema and defaults exactly what production writes.
fn seed_record(roots: &v2::ScopeRoots) -> v2::MemoryRecord {
    let service = v2::MemoryService::open(roots.clone()).expect("seed open");
    let actor = v2::Actor::User {
        invocation: v2::UserInvocationId::mint("memory-scale-seed"),
    };
    let admission = service
        .admit(
            &actor,
            v2::WriteIntent {
                op_id: v2::OpId::parse("memory-scale-seed").expect("op"),
                kind: v2::MemoryKind::UserFact,
                scope: v2::MemoryScope::Project,
                subject: Some("template".into()),
                subject_key: Some("template".into()),
                value: Some("模板正文：发布流程使用 cargo test 校验 PermissionProfile。".into()),
                title: "模板记忆".into(),
                body: "模板正文：发布流程使用 cargo test 校验 PermissionProfile。".into(),
                tags: Vec::new(),
                statement: v2::StatementKind::Assertion,
                relation: v2::Relation::None,
                expires_at: None,
                evidence: Vec::new(),
                experience: None,
                proposed_authority: v2::MemoryAuthority::UserExplicit,
                expected: None,
            },
        )
        .expect("seed write");
    let (id, _, _) = admission.applied_id().expect("seed applied");
    let record = service.read(&actor, id).expect("seed read").record;
    // The template itself is not part of the measured population.
    let paths = v2::StorePaths::inspect(roots).expect("seed layout");
    std::fs::remove_file(paths.current_file(id)).expect("seed current removal");
    std::fs::remove_file(paths.receipt_file(id)).expect("seed receipt removal");
    record
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture dir");
    }
    std::fs::write(path, bytes).expect("fixture file");
}
