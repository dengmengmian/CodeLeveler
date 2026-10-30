//! Durable usage reporting: the ONE aggregation of the runtime's model-call
//! ledger into a [`UiUsageReport`].
//!
//! The terminal and the desktop both read this projection; neither computes
//! token arithmetic of its own. The rules live here because they are facts
//! about the runtime's own records:
//!
//! - every figure is summed from `model_requests` rows, so replaying a query
//!   cannot inflate it and a compaction cannot reset it;
//! - `cached_input_tokens` and `reasoning_tokens` are SUBSETS of input and
//!   output and are never added to the total again;
//! - a row whose provider reported nothing stays an unmeasured request instead
//!   of a measured zero;
//! - cost sums only priced rows and says how many rows are in that sum, so a
//!   partial total is never presented as the bill.
//!
//! Provider health probes are excluded: they are diagnostics the runtime runs
//! on its own account, never a session's cost.

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use leveler_client_protocol::{UiModelUsage, UiUsageReport, UiUsageScope};
use leveler_core::{SessionId, Timestamp};
use leveler_storage::{Database, ModelCallKind, ModelRequestRecord, ModelRequestRepository};

use crate::AppError;

/// How far a caller's UTC offset may be from UTC before the runtime refuses the
/// query. Real offsets are within ±14h; the bound keeps arithmetic sane.
const MAX_UTC_OFFSET_MINUTES: i32 = 24 * 60;

/// Compute the usage report for one scope.
///
/// `utc_offset_minutes` is the CALLER's offset: "today", "last 7 days" and
/// "last 30 days" are calendar-day windows in the user's own timezone even when
/// the runtime runs elsewhere. The resolved range travels back in the report.
pub async fn query_usage(
    db: &Database,
    session_id: &SessionId,
    scope: UiUsageScope,
    utc_offset_minutes: i32,
) -> Result<UiUsageReport, AppError> {
    if !(-MAX_UTC_OFFSET_MINUTES..=MAX_UTC_OFFSET_MINUTES).contains(&utc_offset_minutes) {
        return Err(AppError::UnsupportedCombination(format!(
            "utc_offset_minutes {utc_offset_minutes} is outside ±{MAX_UTC_OFFSET_MINUTES}"
        )));
    }
    let now = leveler_core::now();
    let Some(range) = resolve_range(db, session_id, scope, utc_offset_minutes, now).await else {
        // The scope names an interval that does not exist yet (a turn that was
        // never started). An empty report is the truth; falling back to "the
        // whole session" would relabel unrelated spend as this turn's.
        return Ok(aggregate(scope, now, None, None, &[]));
    };
    let rows = ModelRequestRepository::new(db)
        .load_window(range.session.as_ref(), range.from, range.to)
        .await
        .map_err(AppError::from)?;
    Ok(aggregate(scope, now, range.from, range.to, &rows))
}

/// The concrete filter a scope resolves to. `session: None` means the window
/// spans every session the runtime's ledger holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRange {
    pub session: Option<SessionId>,
    pub from: Option<Timestamp>,
    pub to: Option<Timestamp>,
}

/// `None` when the scope covers an interval that does not exist (the turn
/// scope before the session has a turn).
async fn resolve_range(
    db: &Database,
    session_id: &SessionId,
    scope: UiUsageScope,
    utc_offset_minutes: i32,
    now: Timestamp,
) -> Option<UsageRange> {
    match scope {
        UiUsageScope::Turn => {
            // The turn's own wall-clock window: from its start to its finish
            // (or to now while it still runs). An auxiliary call the runtime
            // makes after the turn ended is not part of that turn.
            let turn = leveler_storage::TurnRepository::new(db)
                .list(session_id)
                .await
                .ok()
                .and_then(|turns| turns.into_iter().last())?;
            let from = turn.created_at.parse::<Timestamp>().ok();
            let to = turn
                .finished_at
                .as_deref()
                .and_then(|at| at.parse::<Timestamp>().ok());
            Some(UsageRange {
                session: Some(session_id.clone()),
                from,
                to,
            })
        }
        UiUsageScope::Session => Some(UsageRange {
            session: Some(session_id.clone()),
            from: None,
            to: None,
        }),
        UiUsageScope::Today | UiUsageScope::Last7Days | UiUsageScope::Last30Days => {
            Some(UsageRange {
                session: None,
                from: local_day_start(now, utc_offset_minutes, scope),
                to: None,
            })
        }
        UiUsageScope::All => Some(UsageRange {
            session: None,
            from: None,
            to: None,
        }),
    }
}

/// Local-day boundary for a day-span scope, expressed in UTC.
///
/// `today` is the current local day's midnight; `last 7 days` reaches back six
/// whole days before it, so the window covers seven calendar days including
/// today. A fixed offset is used because the caller supplied one; there is no
/// DST transition to guess at server-side.
fn local_day_start(
    now: Timestamp,
    utc_offset_minutes: i32,
    scope: UiUsageScope,
) -> Option<Timestamp> {
    let span = scope.day_span()?;
    let offset = FixedOffset::east_opt(utc_offset_minutes * 60)?;
    let local_date = now.with_timezone(&offset).date_naive();
    let start_date = local_date - chrono::Duration::days(i64::from(span.saturating_sub(1)));
    let naive = start_date.and_hms_opt(0, 0, 0)?;
    let local: DateTime<FixedOffset> = offset.from_local_datetime(&naive).single()?;
    Some(local.with_timezone(&Utc))
}

/// Sum recorded calls into one report. Pure: the same rows always produce the
/// same report.
fn aggregate(
    scope: UiUsageScope,
    now: Timestamp,
    from: Option<Timestamp>,
    to: Option<Timestamp>,
    rows: &[leveler_storage::ScopedModelRequest],
) -> UiUsageReport {
    let mut report = UiUsageReport {
        scope,
        from: from.map(|at| at.to_rfc3339()),
        to: to.map(|at| at.to_rfc3339()),
        generated_at: now.to_rfc3339(),
        requests: 0,
        unmeasured_requests: 0,
        input_tokens: 0,
        cached_input_tokens: None,
        output_tokens: 0,
        reasoning_tokens: None,
        cost_usd_micros: None,
        priced_requests: 0,
        models: Vec::new(),
    };
    // Groups keep first-seen order until the final sort, so a deterministic
    // tie-break exists for equal spend.
    let mut groups: Vec<((String, String), UiModelUsage)> = Vec::new();
    for scoped in rows {
        let record = &scoped.record;
        // Runtime diagnostics on its own account, never a session's spend.
        if record.kind == ModelCallKind::ProviderProbe {
            continue;
        }
        report.requests += 1;
        if is_unmeasured(record) {
            report.unmeasured_requests += 1;
        }
        report.input_tokens = report.input_tokens.saturating_add(record.input_tokens);
        report.output_tokens = report.output_tokens.saturating_add(record.output_tokens);
        if let Some(value) = record.cached_input_tokens {
            report.cached_input_tokens = Some(
                report
                    .cached_input_tokens
                    .unwrap_or(0)
                    .saturating_add(value),
            );
        }
        if let Some(value) = record.reasoning_tokens {
            report.reasoning_tokens =
                Some(report.reasoning_tokens.unwrap_or(0).saturating_add(value));
        }
        if let Some(value) = record.cost_usd_micros {
            report.cost_usd_micros =
                Some(report.cost_usd_micros.unwrap_or(0).saturating_add(value));
            report.priced_requests += 1;
        }

        let key = (record.provider.clone(), record.model.clone());
        let index = match groups.iter().position(|(k, _)| *k == key) {
            Some(index) => index,
            None => {
                groups.push((
                    key,
                    UiModelUsage {
                        provider: record.provider.clone(),
                        model: record.model.clone(),
                        requests: 0,
                        unmeasured_requests: 0,
                        input_tokens: 0,
                        cached_input_tokens: None,
                        output_tokens: 0,
                        reasoning_tokens: None,
                        cost_usd_micros: None,
                        priced_requests: 0,
                    },
                ));
                groups.len() - 1
            }
        };
        let group = &mut groups[index].1;
        group.requests += 1;
        if is_unmeasured(record) {
            group.unmeasured_requests += 1;
        }
        group.input_tokens = group.input_tokens.saturating_add(record.input_tokens);
        group.output_tokens = group.output_tokens.saturating_add(record.output_tokens);
        if let Some(value) = record.cached_input_tokens {
            group.cached_input_tokens =
                Some(group.cached_input_tokens.unwrap_or(0).saturating_add(value));
        }
        if let Some(value) = record.reasoning_tokens {
            group.reasoning_tokens =
                Some(group.reasoning_tokens.unwrap_or(0).saturating_add(value));
        }
        if let Some(value) = record.cost_usd_micros {
            group.cost_usd_micros = Some(group.cost_usd_micros.unwrap_or(0).saturating_add(value));
            group.priced_requests += 1;
        }
    }
    let mut models: Vec<UiModelUsage> = groups.into_iter().map(|(_, group)| group).collect();
    models.sort_by(|a, b| {
        b.total_tokens()
            .cmp(&a.total_tokens())
            .then(b.requests.cmp(&a.requests))
            .then(a.provider.cmp(&b.provider))
            .then(a.model.cmp(&b.model))
    });
    report.models = models;
    report
}

/// A row the provider answered without reporting usage. It is a request that
/// happened, and NOT a measured zero: the client must say "unavailable" rather
/// than print `0 tokens` for it.
fn is_unmeasured(record: &ModelRequestRecord) -> bool {
    record.input_tokens == 0 && record.output_tokens == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use leveler_storage::ModelRequestRecord;

    fn record(
        provider: &str,
        model: &str,
        input: u64,
        output: u64,
    ) -> leveler_storage::ScopedModelRequest {
        leveler_storage::ScopedModelRequest {
            session_id: SessionId::new("s1"),
            record: ModelRequestRecord {
                budget_scope: None,
                estimated_tokens: None,
                id: leveler_core::EventId::generate().into_inner(),
                provider_request_id: None,
                session_id: SessionId::new("s1"),
                provider: provider.into(),
                model: model.into(),
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: None,
                reasoning_tokens: None,
                projected_input_tokens: None,
                projected_reasoning_tokens: None,
                cost_usd_micros: None,
                agent_id: None,
                finish_reason: None,
                error_kind: None,
                latency_ms: None,
                attempt_ms: None,
                connect_ms: None,
                ttft_ms: None,
                max_event_gap_ms: None,
                retry_count: 0,
                kind: ModelCallKind::Round,
                created_at: Utc::now(),
                reasoning_effort: None,
                error_detail: None,
            },
        }
    }

    fn at(offset_minutes: i32) -> Timestamp {
        // 2026-03-05T02:30:00Z is still 2026-03-04 in UTC-5.
        DateTime::parse_from_rfc3339("2026-03-05T02:30:00Z")
            .unwrap()
            .with_timezone(&FixedOffset::east_opt(offset_minutes * 60).unwrap())
            .with_timezone(&Utc)
    }

    #[test]
    fn cached_and_reasoning_are_slices_not_additions() {
        let mut row = record("p", "m", 1_000, 200);
        row.record.cached_input_tokens = Some(900);
        row.record.reasoning_tokens = Some(150);
        let report = aggregate(
            UiUsageScope::Session,
            Utc::now(),
            None,
            None,
            std::slice::from_ref(&row),
        );
        assert_eq!(report.input_tokens, 1_000);
        assert_eq!(report.output_tokens, 200);
        assert_eq!(report.total_tokens(), 1_200);
        assert_eq!(report.cached_input_tokens, Some(900));
        assert_eq!(report.reasoning_tokens, Some(150));
    }

    #[test]
    fn an_unmeasured_row_is_counted_but_contributes_no_tokens() {
        let rows = [record("p", "m", 0, 0), record("p", "m", 10, 5)];
        let report = aggregate(UiUsageScope::Session, Utc::now(), None, None, &rows);
        assert_eq!(report.requests, 2);
        assert_eq!(report.unmeasured_requests, 1);
        assert_eq!(report.total_tokens(), 15);
    }

    #[test]
    fn partial_cost_says_it_is_partial() {
        let mut priced = record("p", "m", 10, 5);
        priced.record.cost_usd_micros = Some(2_000);
        let rows = [priced, record("q", "n", 10, 5)];
        let report = aggregate(UiUsageScope::Session, Utc::now(), None, None, &rows);
        assert_eq!(report.cost_usd_micros, Some(2_000));
        assert_eq!(report.priced_requests, 1);
        assert!(report.cost_is_partial());
    }

    #[test]
    fn models_are_grouped_and_ordered_by_spend() {
        let rows = [
            record("a", "m1", 10, 5),
            record("b", "m2", 1_000, 100),
            record("a", "m1", 10, 5),
        ];
        let report = aggregate(UiUsageScope::Session, Utc::now(), None, None, &rows);
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].provider, "b");
        assert_eq!(report.models[1].requests, 2);
        assert_eq!(report.models[1].total_tokens(), 30);
    }

    #[test]
    fn provider_probes_are_not_a_session_cost() {
        let mut probe = record("p", "m", 100, 10);
        probe.record.kind = ModelCallKind::ProviderProbe;
        let report = aggregate(
            UiUsageScope::Session,
            Utc::now(),
            None,
            None,
            std::slice::from_ref(&probe),
        );
        assert_eq!(report.requests, 0);
        assert!(report.is_empty());
    }

    #[test]
    fn local_day_boundaries_use_the_callers_offset() {
        // 02:30 UTC on Mar 5 is 21:30 on Mar 4 at UTC-5: "today" is Mar 4 there.
        let now = at(0);
        let from = local_day_start(now, -300, UiUsageScope::Today).unwrap();
        assert_eq!(from.to_rfc3339(), "2026-03-04T05:00:00+00:00");
        let from = local_day_start(now, -300, UiUsageScope::Last7Days).unwrap();
        assert_eq!(from.to_rfc3339(), "2026-02-26T05:00:00+00:00");
        let from = local_day_start(now, -300, UiUsageScope::Last30Days).unwrap();
        assert_eq!(from.to_rfc3339(), "2026-02-03T05:00:00+00:00");
    }

    #[test]
    fn local_day_start_of_today_is_that_days_midnight() {
        let now = DateTime::parse_from_rfc3339("2026-03-05T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        let from = local_day_start(now, 480, UiUsageScope::Today).unwrap();
        // Midnight in UTC+8 on Mar 6 is 16:00Z on Mar 5.
        assert_eq!(from.to_rfc3339(), "2026-03-05T16:00:00+00:00");
        assert_eq!(
            now.with_timezone(&Utc).day(),
            5,
            "the UTC day differs from the caller's day: the boundary must follow the caller"
        );
    }

    #[test]
    fn non_day_scopes_have_no_day_start() {
        assert!(local_day_start(Utc::now(), 0, UiUsageScope::All).is_none());
        assert!(local_day_start(Utc::now(), 0, UiUsageScope::Session).is_none());
        assert!(local_day_start(Utc::now(), 0, UiUsageScope::Turn).is_none());
    }
}
