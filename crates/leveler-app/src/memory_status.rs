//! Read-only memory health for `/memory status`.
//!
//! The report joins two truth sources that already exist: the file-backed
//! store (active/pending/archived) and the durable SQLite inbox (what the
//! background extractor has processed, rejected, skipped or must retry). It
//! adds no counters of its own, so a status line cannot drift from the worker.

use leveler_client_protocol::{
    UiMemoryExperience, UiMemoryExtraction, UiMemoryExtractionTotals, UiMemoryStatus,
};
use leveler_memory::{MemoryView, ServiceScope};
use leveler_storage::{ExperienceInboxRepository, MemoryInboxRepository};

use crate::{AppError, Application};

/// How many retained processed batches the totals cover. The worker prunes
/// beyond this window, so the number is a diagnostic window, not all history.
const PROCESSED_WINDOW: u32 = 256;
const MAX_ERROR_CHARS: usize = 240;

#[derive(serde::Deserialize, Default)]
struct StagedTurnResult {
    #[serde(default)]
    accepted: Vec<serde_json::Value>,
    #[serde(default)]
    rejected: usize,
    #[serde(default)]
    skipped: usize,
}

#[derive(serde::Deserialize, Default)]
struct StagedExperienceResult {
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    accepted: Vec<serde_json::Value>,
    #[serde(default)]
    rejected: usize,
    #[serde(default)]
    created: usize,
    #[serde(default)]
    updated: usize,
    #[serde(default)]
    pending: usize,
    #[serde(default)]
    skipped: usize,
}

fn experience_outcome(at: String, result_json: Option<&str>) -> Option<UiMemoryExperience> {
    let staged: StagedExperienceResult = serde_json::from_str(result_json?).ok()?;
    Some(UiMemoryExperience {
        at,
        outcome: if staged.outcome.is_empty() {
            "skipped".to_string()
        } else {
            staged.outcome
        },
        reason: staged.reason,
        accepted: count_u32(staged.accepted.len() as u64),
        rejected: count_u32(staged.rejected as u64),
        created: count_u32(staged.created as u64),
        updated: count_u32(staged.updated as u64),
        pending: count_u32(staged.pending as u64),
        skipped: count_u32(staged.skipped as u64),
    })
}

fn staged_totals(result_json: Option<&str>) -> (u32, u32, u32) {
    let Some(raw) = result_json else {
        return (0, 0, 0);
    };
    let Ok(staged) = serde_json::from_str::<Vec<StagedTurnResult>>(raw) else {
        return (0, 0, 0);
    };
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut skipped = 0usize;
    for item in staged {
        accepted += item.accepted.len();
        rejected += item.rejected;
        skipped += item.skipped;
    }
    (
        u32::try_from(accepted).unwrap_or(u32::MAX),
        u32::try_from(rejected).unwrap_or(u32::MAX),
        u32::try_from(skipped).unwrap_or(u32::MAX),
    )
}

/// Provider/extractor errors may carry upstream text. Show the class unless it
/// itself looks like credential material, and never more than one short line.
fn sanitize_error(raw: &str) -> String {
    if leveler_memory::looks_like_secret(raw) {
        return "[redacted]".to_string();
    }
    let single_line: String = raw
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_ERROR_CHARS)
        .collect();
    single_line.trim().to_string()
}

fn count_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Build the status surface from the store and the durable inbox.
pub async fn report(app: &Application) -> Result<UiMemoryStatus, AppError> {
    // Status reads the same service writes go to, so the numbers describe the
    // store the project is actually using. Current truth only: crash residue
    // or an expired entry must not inflate the active count.
    let service = app.memory_service();
    let actor = leveler_memory::v2::Actor::User {
        invocation: leveler_memory::v2::UserInvocationId::mint("ui:memory-status"),
    };
    let active = service
        .listing_for_user(ServiceScope::Project, leveler_memory::MemoryLanes::ALL)
        .map_err(|error| AppError::Engine(format!("memory: {error}")))?;
    let pending = service
        .pending(&actor, ServiceScope::Project)
        .map_err(|error| AppError::Engine(format!("memory: {error}")))?;
    let archived = service
        .archived(
            &actor,
            ServiceScope::Project,
            leveler_memory::MemoryLanes::ALL,
            None,
        )
        .map_err(|error| AppError::Engine(format!("memory: {error}")))?;
    let sensitive = active.iter().filter(|view| view.sensitive).count()
        + archived.iter().filter(|view| view.sensitive).count()
        + pending.iter().filter(|view| view.sensitive).count();

    let db = app.open_database().await?;
    let repo = MemoryInboxRepository::new(&db);
    let counts = repo.counts().await?;
    let processed = repo.processed_results(PROCESSED_WINDOW).await?;
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut skipped = 0u32;
    for row in &processed {
        let (a, r, s) = staged_totals(row.result_json.as_deref());
        accepted = accepted.saturating_add(a);
        rejected = rejected.saturating_add(r);
        skipped = skipped.saturating_add(s);
    }
    let last_extraction = repo.last_processed().await?.map(|row| {
        let (accepted, rejected, skipped) = staged_totals(row.result_json.as_deref());
        UiMemoryExtraction {
            at: row.processed_at,
            accepted,
            rejected,
            skipped,
            zero_candidates: accepted == 0 && rejected == 0 && skipped == 0,
        }
    });
    let failure = repo.last_failure().await?;
    let last_error = failure
        .as_ref()
        .and_then(|row| row.last_error.as_deref())
        .map(sanitize_error)
        .filter(|error| !error.is_empty());

    let is_experience = |view: &&MemoryView| view.is_experience();
    let experience_active = active.iter().filter(is_experience).collect::<Vec<_>>();
    let experience_verified = experience_active
        .iter()
        .filter(|view| {
            view.experience
                .as_ref()
                .is_some_and(|experience| experience.standing == "verified")
        })
        .count();
    let experience_pending = pending.iter().filter(is_experience).count();
    let experience_archived = archived.iter().filter(is_experience).count();
    let experience_repo = ExperienceInboxRepository::new(&db);
    let experience_counts = experience_repo.counts().await?;
    let last_experience = experience_repo
        .last_processed()
        .await?
        .and_then(|row| experience_outcome(row.processed_at, row.result_json.as_deref()));
    let experience_failure = experience_repo.last_failure().await?;

    Ok(UiMemoryStatus {
        total_enabled: app.memory_policy().enabled(),
        user_enabled: app.memory_policy().user_enabled(),
        experience_enabled: app.memory_policy().experience_enabled(),
        experience_experimental: true,
        active: u32::try_from(active.len()).unwrap_or(u32::MAX),
        pending: u32::try_from(pending.len()).unwrap_or(u32::MAX),
        archived: u32::try_from(archived.len()).unwrap_or(u32::MAX),
        user_active: count_u32(active.len().saturating_sub(experience_active.len()) as u64),
        user_pending: count_u32(pending.len().saturating_sub(experience_pending) as u64),
        user_archived: count_u32(archived.len().saturating_sub(experience_archived) as u64),
        sensitive: u32::try_from(sensitive).unwrap_or(u32::MAX),
        inbox_pending: count_u32(counts.pending),
        inbox_processing: count_u32(counts.processing),
        inbox_failed_retryable: count_u32(counts.failed_retryable),
        inbox_processed: count_u32(counts.processed),
        next_retry_at: failure.as_ref().and_then(|row| row.next_attempt_at.clone()),
        retry_attempt: failure
            .as_ref()
            .map(|row| u32::try_from(row.attempt).unwrap_or(u32::MAX)),
        extraction_totals: UiMemoryExtractionTotals {
            accepted,
            rejected,
            skipped,
        },
        last_extraction,
        last_error,
        experience_active: count_u32(experience_active.len() as u64),
        experience_verified: count_u32(experience_verified as u64),
        experience_unverified: count_u32(
            experience_active.len().saturating_sub(experience_verified) as u64,
        ),
        experience_pending: count_u32(experience_pending as u64),
        experience_archived: count_u32(experience_archived as u64),
        experience_inbox_pending: count_u32(experience_counts.pending),
        experience_inbox_processing: count_u32(experience_counts.processing),
        experience_inbox_failed_retryable: count_u32(experience_counts.failed_retryable),
        experience_inbox_processed: count_u32(experience_counts.processed),
        experience_next_retry_at: experience_failure
            .as_ref()
            .and_then(|row| row.next_attempt_at.clone()),
        last_experience,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_distinguishes_rejected_and_skipped_from_zero_candidates() {
        let zero = staged_totals(Some(r#"[{"accepted":[],"rejected":0,"skipped":0}]"#));
        assert_eq!(zero, (0, 0, 0));
        let filtered = staged_totals(Some(
            r#"[{"accepted":[],"rejected":2,"skipped":1},{"accepted":[],"rejected":0,"skipped":3}]"#,
        ));
        assert_eq!(filtered, (0, 2, 4));
        let accepted = staged_totals(Some(
            r#"[{"accepted":[{"id":"cand-1"},{"id":"cand-2"}],"rejected":0,"skipped":0}]"#,
        ));
        assert_eq!(accepted, (2, 0, 0));
        assert_eq!(staged_totals(None), (0, 0, 0));
        assert_eq!(staged_totals(Some("{not-json")), (0, 0, 0));
    }

    #[test]
    fn a_provider_error_is_a_short_single_line_and_never_a_credential() {
        let sanitized = sanitize_error("connect failed\nretry later");
        assert_eq!(sanitized, "connect failed retry later");
        assert_eq!(
            sanitize_error("provider rejected key sk-live-abcdefghijklmnopqrstuvwxyz012345"),
            "[redacted]"
        );
        assert!(sanitize_error(&"x".repeat(500)).chars().count() <= MAX_ERROR_CHARS);
    }
}
