//! Stage 3: the SQLite FTS5 index is derived state over Markdown current.
//!
//! These tests go through the production entry points: writes go through the
//! v2 service, retrieval through the unified facade. The index may disappear
//! at any moment because Markdown is the only content authority.

mod memory_v2_common;

use leveler_memory::v2::{
    Actor, MemoryId, MemoryKind, MemoryScope, MemoryService as V2Service, UserInvocationId,
    WriteIntent,
};
use leveler_memory::{MemoryLanes, MemoryRoots, MemoryService, ServiceError, ServiceScope};
use memory_v2_common::{Fixture, op};

fn facade(fixture: &Fixture) -> MemoryService {
    MemoryService::new(MemoryRoots::markdown().with_project(fixture.roots()))
}

fn user() -> Actor {
    Actor::User {
        invocation: UserInvocationId::mint(format!("inv-{}", uuid::Uuid::new_v4().simple())),
    }
}

fn fact(fixture: &Fixture, subject: &str, title: &str, body: &str) -> MemoryId {
    let service = V2Service::open(fixture.roots()).unwrap();
    let intent = WriteIntent {
        op_id: op(),
        kind: MemoryKind::UserFact,
        scope: fixture.scope,
        subject: Some(subject.into()),
        subject_key: Some(subject.into()),
        value: Some(body.into()),
        title: title.into(),
        body: body.into(),
        tags: Vec::new(),
        statement: leveler_memory::v2::StatementKind::Assertion,
        relation: leveler_memory::v2::Relation::None,
        expires_at: None,
        evidence: Vec::new(),
        experience: None,
        proposed_authority: leveler_memory::v2::MemoryAuthority::UserExplicit,
        expected: None,
    };
    match service.admit(&user(), intent).unwrap() {
        leveler_memory::v2::Admission::Applied { id, .. }
        | leveler_memory::v2::Admission::Replayed { id, .. } => id,
        other => panic!("expected an applied write, got {other:?}"),
    }
}

fn search(fixture: &Fixture, scope: ServiceScope, query: &str) -> Vec<String> {
    facade(fixture)
        .search(&user(), scope, MemoryLanes::ALL, query, 10, false)
        .unwrap()
        .into_iter()
        .map(|(view, _)| view.id)
        .collect()
}

#[test]
fn chinese_one_two_three_character_queries_match() {
    let fixture = Fixture::new(MemoryScope::Project);
    let id = fact(
        &fixture,
        "billing",
        "计费模式",
        "本项目采用按量计费，不采用订阅制。",
    );

    for query in ["计", "计费", "计费模式"] {
        let hits = search(&fixture, ServiceScope::Project, query);
        assert!(
            hits.contains(&id.to_string()),
            "query {query:?} did not return {id}: {hits:?}"
        );
    }
}

#[test]
fn english_and_code_identifiers_match() {
    let fixture = Fixture::new(MemoryScope::Project);
    let id = fact(
        &fixture,
        "runtime",
        "PermissionProfile",
        "Runtime applies the PermissionProfile before a tool call; the struct is PermissionProfile.",
    );

    for query in [
        "permission",
        "PermissionProfile",
        "permission profile",
        "runtime",
    ] {
        let hits = search(&fixture, ServiceScope::Project, query);
        assert!(
            hits.contains(&id.to_string()),
            "query {query:?} did not return {id}: {hits:?}"
        );
    }
}

#[test]
fn mixed_chinese_and_english_matches() {
    let fixture = Fixture::new(MemoryScope::Project);
    let id = fact(
        &fixture,
        "deploy",
        "发布流程",
        "发布前先运行 cargo test -p leveler-memory，然后打包。",
    );

    let hits = search(&fixture, ServiceScope::Project, "发布 cargo test");
    assert!(
        hits.contains(&id.to_string()),
        "mixed query did not return {id}: {hits:?}"
    );
}

#[test]
fn scope_isolation_holds() {
    let project = Fixture::new(MemoryScope::Project);
    let global = Fixture::new(MemoryScope::Global);
    let project_id = fact(&project, "rate", "费率", "项目费率是每千 token 一元。");
    let global_id = fact(&global, "rate", "费率", "全球费率是每千 token 二元。");

    let service = MemoryService::new(
        MemoryRoots::markdown()
            .with_project(project.roots())
            .with_global(global.roots()),
    );
    let hits = service
        .search(
            &user(),
            ServiceScope::Project,
            MemoryLanes::ALL,
            "费率",
            10,
            false,
        )
        .unwrap();
    let ids: Vec<String> = hits.into_iter().map(|(view, _)| view.id).collect();
    assert!(ids.contains(&project_id.to_string()));
    assert!(!ids.contains(&global_id.to_string()));

    let hits = service
        .search(
            &user(),
            ServiceScope::Global,
            MemoryLanes::ALL,
            "费率",
            10,
            false,
        )
        .unwrap();
    let ids: Vec<String> = hits.into_iter().map(|(view, _)| view.id).collect();
    assert!(ids.contains(&global_id.to_string()));
    assert!(!ids.contains(&project_id.to_string()));
}

#[test]
fn retired_forgotten_pending_and_expired_are_not_recalled() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = V2Service::open(fixture.roots()).unwrap();

    let forgotten = fact(&fixture, "old", "旧设置", "旧设置使用蓝色主题。");
    let discarded = fact(&fixture, "discard", "废弃设置", "废弃设置使用绿色主题。");
    let retired = fact(&fixture, "retire", "退休设置", "退休设置使用黄色主题。");

    let forget_record = service.read(&user(), &forgotten).unwrap();
    service
        .forget(&user(), op(), &forgotten, forget_record.cas)
        .unwrap();
    let discard_record = service.read(&user(), &discarded).unwrap();
    service
        .deprecate(
            &user(),
            op(),
            &discarded,
            discard_record.cas,
            leveler_memory::v2::Reason::new("no longer useful"),
        )
        .unwrap();
    let retire_record = service.read(&user(), &retired).unwrap();
    let patch = leveler_memory::v2::RecordPatch {
        expires_at: Some(Some(chrono::Utc::now() + chrono::Duration::hours(1))),
        ..Default::default()
    };
    service
        .update(&user(), op(), &retired, patch, retire_record.cas)
        .unwrap();
    let retired_now = service
        .expire_due(chrono::Utc::now() + chrono::Duration::hours(2))
        .unwrap();
    assert!(retired_now.contains(&retired));

    for query in ["主题"] {
        let hits = search(&fixture, ServiceScope::Project, query);
        for (label, id) in [
            ("forgotten", &forgotten),
            ("deprecated", &discarded),
            ("expired", &retired),
        ] {
            assert!(
                !hits.contains(&id.to_string()),
                "query {query:?} returned {label} memory {id}: {hits:?}"
            );
        }
    }
    assert!(service.read(&user(), &forgotten).is_err());
}

#[test]
fn modified_markdown_does_not_keep_injecting_the_stale_body() {
    let fixture = Fixture::new(MemoryScope::Project);
    let id = fact(
        &fixture,
        "billing",
        "计费模式",
        "本项目使用旧计费模式 monthly。",
    );
    assert!(search(&fixture, ServiceScope::Project, "monthly").contains(&id.to_string()));

    // A person edits the file outside the trusted write path. The store must
    // no longer treat the file as trusted, and a stale index row may not
    // surface the old body.
    let path = fixture.current_file(&id);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace(
            "本项目使用旧计费模式 monthly。",
            "本项目使用新计费模式 annual。",
        ),
    )
    .unwrap();

    let hits = search(&fixture, ServiceScope::Project, "monthly");
    assert!(
        !hits.contains(&id.to_string()),
        "stale index body was recalled after an external edit: {hits:?}"
    );
}

#[test]
fn deleting_the_index_rebuilds_from_markdown() {
    let fixture = Fixture::new(MemoryScope::Project);
    let id = fact(
        &fixture,
        "search",
        "检索",
        "Markdown 是唯一权威，索引可以删除后重建。",
    );
    assert!(search(&fixture, ServiceScope::Project, "索引").contains(&id.to_string()));

    let index = fixture.state.join("index");
    let mut removed = 0;
    for entry in std::fs::read_dir(&index).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_file() {
            std::fs::remove_file(entry.path()).unwrap();
            removed += 1;
        }
    }
    assert!(removed > 0, "expected an index database to remove");

    let hits = search(&fixture, ServiceScope::Project, "索引");
    assert!(
        hits.contains(&id.to_string()),
        "search after index deletion did not rebuild from Markdown: {hits:?}"
    );
}

#[test]
fn corrupt_index_reports_degraded_instead_of_empty_success() {
    let fixture = Fixture::new(MemoryScope::Project);
    fact(&fixture, "search", "检索", "索引损坏时不能静默返回空结果。");
    assert!(!search(&fixture, ServiceScope::Project, "索引").is_empty());

    // Replace the database with bytes no SQLite reader can interpret.
    let index = fixture.state.join("index").join("fts.sqlite");
    std::fs::write(&index, b"not a sqlite database").unwrap();

    let result = facade(&fixture).search(
        &user(),
        ServiceScope::Project,
        MemoryLanes::ALL,
        "索引",
        10,
        false,
    );
    match result {
        Err(ServiceError::Unavailable(reason)) => {
            assert!(
                reason.contains("index") || reason.contains("degraded"),
                "degraded reason was not explicit: {reason}"
            );
        }
        // A controlled rebuild may also recover; it must never return an
        // empty success while the index is broken.
        Ok(views) => assert!(!views.is_empty(), "corrupt index returned an empty success"),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

#[test]
fn status_and_lane_filters_apply_to_search_results() {
    let fixture = Fixture::new(MemoryScope::Project);
    let id = fact(&fixture, "status", "状态", "用户记忆应能在用户通道检索到。");
    let service = facade(&fixture);
    let hits = service
        .search(
            &user(),
            ServiceScope::Project,
            MemoryLanes::EXPERIENCE_ONLY,
            "用户记忆",
            10,
            false,
        )
        .unwrap();
    assert!(!hits.iter().any(|(view, _)| view.id == id.to_string()));

    let hits = service
        .search(
            &user(),
            ServiceScope::Project,
            MemoryLanes::USER_ONLY,
            "用户记忆",
            10,
            false,
        )
        .unwrap();
    assert!(hits.iter().any(|(view, _)| view.id == id.to_string()));
    assert_eq!(hits[0].0.status, "active");
}

fn remember(
    service: &MemoryService,
    title: &str,
    body: &str,
    subject: Option<&str>,
    replaces: Option<&str>,
) -> String {
    let mut request = leveler_memory::RememberRequest::new(
        format!("op-{}", uuid::Uuid::new_v4()),
        ServiceScope::Project,
        title,
        body,
        "note",
    );
    request.subject = subject.map(str::to_string);
    request.replaces_id = replaces.map(str::to_string);
    service
        .remember(&user(), request)
        .unwrap()
        .applied_id()
        .expect("write must apply")
        .to_string()
}

#[test]
fn facade_write_updates_the_derived_index_without_a_reconcile() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = facade(&fixture);
    service.refresh_index(ServiceScope::Project).unwrap();
    let id = remember(
        &service,
        "发布通道",
        "发布使用蓝色通道部署。",
        Some("deploy_channel"),
        None,
    );

    // A new facade stands in for a new process: it did not observe the write
    // in memory, and the index is not dirty, so only the incremental sync can
    // make this record searchable.
    let fresh = facade(&fixture);
    let hits = fresh
        .search(
            &user(),
            ServiceScope::Project,
            MemoryLanes::ALL,
            "蓝色通道",
            10,
            false,
        )
        .unwrap();
    assert!(hits.iter().any(|(view, _)| view.id == id), "{hits:?}");
}

#[test]
fn supersede_removes_the_old_record_from_retrieval() {
    let fixture = Fixture::new(MemoryScope::Project);
    let service = facade(&fixture);
    let old = remember(
        &service,
        "计费模式",
        "计费使用 monthly 模式。",
        Some("billing_model"),
        None,
    );
    let new = remember(
        &service,
        "计费模式",
        "计费使用 annual 模式。",
        Some("billing_model"),
        Some(&old),
    );

    let hits = service
        .search(
            &user(),
            ServiceScope::Project,
            MemoryLanes::ALL,
            "计费",
            10,
            false,
        )
        .unwrap();
    let ids: Vec<String> = hits.into_iter().map(|(view, _)| view.id).collect();
    assert!(ids.contains(&new), "new record missing: {ids:?}");
    assert!(!ids.contains(&old), "superseded record recalled: {ids:?}");
}

#[test]
fn pending_and_credential_shaped_records_never_enter_retrieval() {
    let fixture = Fixture::new(MemoryScope::Project);
    let v2 = V2Service::open(fixture.roots()).unwrap();
    let pending_service_id = fixture.scope;
    let _ = pending_service_id;
    let intent = WriteIntent {
        op_id: op(),
        kind: MemoryKind::UserFact,
        scope: fixture.scope,
        subject: Some("review_me".into()),
        subject_key: Some("review_me".into()),
        value: Some("候选记忆".into()),
        title: "待审记忆".into(),
        body: "待审记忆尚未批准。".into(),
        tags: Vec::new(),
        statement: leveler_memory::v2::StatementKind::Assertion,
        relation: leveler_memory::v2::Relation::None,
        expires_at: None,
        evidence: Vec::new(),
        experience: None,
        proposed_authority: leveler_memory::v2::MemoryAuthority::ModelInferred,
        expected: None,
    };
    v2.propose(
        &Actor::Model {
            session: leveler_core::SessionId::new("session-1"),
            turn: leveler_core::TurnId::new("turn-1"),
            tool_call: leveler_core::ToolCallId::new("call-1"),
        },
        intent,
        leveler_memory::v2::PendingReason::ScopeRequiresUserCommand,
    )
    .unwrap();

    let secret_transaction = v2.admit(
        &user(),
        WriteIntent {
            op_id: op(),
            kind: MemoryKind::UserFact,
            scope: fixture.scope,
            subject: Some("credentials".into()),
            subject_key: Some("credentials".into()),
            value: Some("普通值".into()),
            title: "凭据".into(),
            body: "api_key = sk-test-not-a-real-key".into(),
            tags: Vec::new(),
            statement: leveler_memory::v2::StatementKind::Assertion,
            relation: leveler_memory::v2::Relation::None,
            expires_at: None,
            evidence: Vec::new(),
            experience: None,
            proposed_authority: leveler_memory::v2::MemoryAuthority::UserExplicit,
            expected: None,
        },
    );
    assert!(
        secret_transaction.is_err(),
        "credential-shaped content must not become a Markdown record"
    );

    let hits = search(&fixture, ServiceScope::Project, "待审记忆");
    assert!(hits.is_empty(), "pending record was recalled: {hits:?}");
    let hits = search(&fixture, ServiceScope::Project, "凭据");
    assert!(
        hits.is_empty(),
        "credential-shaped record was recalled: {hits:?}"
    );
}

#[test]
#[ignore = "10k-file scale benchmark; run explicitly and record the numbers"]
fn ten_thousand_memories_index_scale() {
    use leveler_memory::v2::{
        Cas, EffectiveRecord, Frontmatter, MemoryAuthority, MemoryRecord, MemoryStatus,
    };
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let index_dir = dir.path().join("index");
    let now = chrono::Utc::now();
    let records: Vec<EffectiveRecord> = (0..10_000)
        .map(|sequence| {
            let id = MemoryId::parse(format!("mem_scale{sequence:05}")).unwrap();
            let body = format!(
                "第 {sequence} 条合成记忆。发布流程使用 cargo test 校验 PermissionProfile。"
            );
            let digest = format!("digest-{sequence}");
            EffectiveRecord {
                record: MemoryRecord {
                    frontmatter: Frontmatter {
                        schema: 1,
                        id,
                        kind: MemoryKind::UserFact,
                        scope: MemoryScope::Project,
                        status: MemoryStatus::Active,
                        revision: 1,
                        authority: MemoryAuthority::UserExplicit,
                        title: format!("合成记忆 {sequence}"),
                        subject: Some(format!("subject_{sequence}")),
                        subject_key: Some(format!("subject_{sequence}")),
                        value: Some(body.clone()),
                        statement: leveler_memory::v2::StatementKind::Assertion,
                        tags: Vec::new(),
                        pinned: sequence % 97 == 0,
                        pinned_by: None,
                        sensitive: false,
                        expires_at: None,
                        supersedes: Vec::new(),
                        superseded_by: None,
                        proposal: None,
                        experience: None,
                        source_refs: Vec::new(),
                        created_at: now,
                        updated_at: now,
                        extra: Default::default(),
                    },
                    body,
                },
                cas: Cas {
                    revision: 1,
                    digest: digest.clone(),
                },
                digest,
                effective_authority: MemoryAuthority::UserExplicit,
            }
        })
        .collect();

    let cold = Instant::now();
    let index = leveler_memory::v2::MemoryIndex::open(&index_dir).unwrap();
    let report = index.reconcile(&records).unwrap();
    let cold_build = cold.elapsed();
    assert_eq!(report.indexed, 10_000);

    let mut timings = Vec::new();
    for sequence in 0..200 {
        let query = if sequence % 2 == 0 {
            "发布流程".to_string()
        } else {
            format!("PermissionProfile {sequence}")
        };
        let started = Instant::now();
        let hits = index.search(&query, 10).unwrap();
        timings.push(started.elapsed());
        assert!(!hits.is_empty());
    }
    timings.sort();
    let p50 = timings[timings.len() / 2];
    let p95 = timings[(timings.len() * 95) / 100];

    let incremental = Instant::now();
    let mut updated = records[0].clone();
    updated.record.frontmatter.revision += 1;
    updated.record.body.push_str(" 增量更新");
    updated.digest = "digest-incremental".into();
    updated.cas.digest = updated.digest.clone();
    index.sync_record(&updated).unwrap();
    let incremental = incremental.elapsed();

    let reopen = Instant::now();
    let reopened = leveler_memory::v2::MemoryIndex::open(&index_dir).unwrap();
    let diagnostics = reopened.diagnostics().unwrap();
    let reopen_time = reopen.elapsed();

    let size = std::fs::metadata(index_dir.join("fts.sqlite"))
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    eprintln!(
        "scale: cold_build={cold_build:?} query_p50={p50:?} query_p95={p95:?} incremental={incremental:?} reopen={reopen_time:?} documents={} dirty={} bytes={size}",
        diagnostics.documents, diagnostics.dirty
    );
}
