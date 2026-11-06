//! Rule computation for persisted triggers.
//!
//! Every trigger computes its next occurrence from the rule and a caller
//! supplied `now`, never from the previous run's completion time. That keeps a
//! delayed run from shifting the grid: a daily 09:00 rule that finishes at
//! 09:07 still answers the next 09:00 slot.
//!
//! Local-clock triggers (daily, weekly, cron) are evaluated in their stored
//! IANA timezone. DST follows two explicit rules:
//!
//! - a wall-clock time that does not exist (spring forward) runs at the first
//!   valid wall-clock instant after the gap;
//! - a wall-clock time that exists twice (fall back) runs once, at the earlier
//!   instant. The repeated instant is not a second occurrence.
//!
//! Both rules are implemented by `resolve_local` and by the cron engine, and
//! are locked by tests in this module.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::ScheduleError;

/// Longest cron expression accepted from a user.
pub const MAX_CRON_BYTES: usize = 512;

/// A wall-clock time of day with minute precision. Seconds are fixed at zero:
/// the OS wake interval is 60 seconds, so sub-minute schedules would only
/// create multiple rows for one wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LocalTime {
    hour: u8,
    minute: u8,
}

impl LocalTime {
    pub fn new(hour: u8, minute: u8) -> Result<Self, ScheduleError> {
        if hour > 23 {
            return Err(ScheduleError::Invalid(format!(
                "hour {hour} is outside 0..=23"
            )));
        }
        if minute > 59 {
            return Err(ScheduleError::Invalid(format!(
                "minute {minute} is outside 0..=59"
            )));
        }
        Ok(Self { hour, minute })
    }

    /// Parse `HH:MM`.
    pub fn parse(raw: &str) -> Result<Self, ScheduleError> {
        let trimmed = raw.trim();
        let (hour, minute) = trimmed
            .split_once(':')
            .ok_or_else(|| ScheduleError::Invalid(format!("`{raw}` is not a HH:MM local time")))?;
        if hour.len() != 2 || minute.len() != 2 {
            return Err(ScheduleError::Invalid(format!(
                "`{raw}` is not a two-digit HH:MM local time"
            )));
        }
        let hour: u8 = hour
            .parse()
            .map_err(|_| ScheduleError::Invalid(format!("invalid hour in `{raw}`")))?;
        let minute: u8 = minute
            .parse()
            .map_err(|_| ScheduleError::Invalid(format!("invalid minute in `{raw}`")))?;
        Self::new(hour, minute)
    }

    pub const fn hour(self) -> u8 {
        self.hour
    }

    pub const fn minute(self) -> u8 {
        self.minute
    }

    fn resolve_on(self, date: NaiveDate, timezone: Tz) -> Option<DateTime<Utc>> {
        resolve_local(date, self, timezone)
    }
}

impl std::fmt::Display for LocalTime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// A non-empty set of weekdays. Monday is bit 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeekdaySet(u8);

impl WeekdaySet {
    pub fn new(weekdays: impl IntoIterator<Item = Weekday>) -> Result<Self, ScheduleError> {
        let mut mask = 0u8;
        for weekday in weekdays {
            mask |= 1 << weekday.num_days_from_monday();
        }
        if mask == 0 {
            return Err(ScheduleError::Invalid(
                "a weekly schedule needs at least one weekday".into(),
            ));
        }
        Ok(Self(mask))
    }

    /// Parse `mon,wed,fri` or `mon-fri`. Names are case-insensitive.
    pub fn parse(raw: &str) -> Result<Self, ScheduleError> {
        let mut mask = 0u8;
        let mut any = false;
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((start, end)) = part.split_once('-') {
                let start = weekday(start)?;
                let end = weekday(end)?;
                let mut day = start.num_days_from_monday();
                let end = end.num_days_from_monday();
                loop {
                    mask |= 1 << day;
                    any = true;
                    if day == end {
                        break;
                    }
                    day = (day + 1) % 7;
                }
            } else {
                mask |= 1 << weekday(part)?.num_days_from_monday();
                any = true;
            }
        }
        if !any {
            return Err(ScheduleError::Invalid(
                "a weekly schedule needs at least one weekday".into(),
            ));
        }
        Ok(Self(mask))
    }

    pub fn contains(self, weekday: Weekday) -> bool {
        self.0 & (1 << weekday.num_days_from_monday()) != 0
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub fn from_bits(bits: u8) -> Result<Self, ScheduleError> {
        if bits == 0 || bits & 0b1000_0000 != 0 {
            return Err(ScheduleError::Invalid(format!(
                "invalid weekday mask `{bits}`"
            )));
        }
        Ok(Self(bits))
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for WeekdaySet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        let mut first = true;
        for (index, name) in NAMES.iter().enumerate() {
            if self.0 & (1 << index) != 0 {
                if !first {
                    formatter.write_str(",")?;
                }
                formatter.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

fn weekday(raw: &str) -> Result<Weekday, ScheduleError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tues" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thur" | "thurs" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        other => Err(ScheduleError::Invalid(format!(
            "`{other}` is not a weekday name"
        ))),
    }
}

/// Parse an IANA timezone name and reject unknown names at the trust boundary.
pub fn parse_timezone(raw: &str) -> Result<Tz, ScheduleError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::Invalid("the timezone is empty".into()));
    }
    Tz::from_str(trimmed)
        .map_err(|_| ScheduleError::Invalid(format!("unknown IANA timezone `{trimmed}`")))
}

/// The host's current IANA timezone. A calendar rule that does not name a zone
/// uses this instead of assuming UTC, so "每天 09:00" means the user's wall
/// clock. Failing to resolve it is an explicit error, never a silent fallback.
pub fn host_timezone() -> Result<Tz, ScheduleError> {
    let raw = iana_time_zone::get_timezone().map_err(|error| {
        ScheduleError::Invalid(format!("could not resolve the local timezone: {error}"))
    })?;
    parse_timezone(&raw)
}

/// Parse a five-field cron expression. Seconds are rejected so an occurrence
/// can never be finer than the 60-second wake interval.
pub fn parse_cron(expression: &str) -> Result<croner::Cron, ScheduleError> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::Invalid(
            "the cron expression is empty".into(),
        ));
    }
    if trimmed.len() > MAX_CRON_BYTES {
        return Err(ScheduleError::Invalid(format!(
            "the cron expression exceeds {MAX_CRON_BYTES} bytes"
        )));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(ScheduleError::Invalid(
            "the cron expression contains a control character".into(),
        ));
    }
    if trimmed.split_whitespace().count() != 5 {
        return Err(ScheduleError::Invalid(
            "the cron expression must have five fields (minute granularity)".into(),
        ));
    }
    croner::Cron::from_str(trimmed)
        .map_err(|error| ScheduleError::Invalid(format!("invalid cron expression: {error}")))
}

/// When a definition generates occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleTrigger {
    /// Exactly one occurrence at this instant. A past instant is due now and
    /// never repeats.
    Once { at: DateTime<Utc> },
    /// One occurrence at `anchor`, then one every `every` from that anchor.
    /// Missed slots are not backfilled.
    Every {
        anchor: DateTime<Utc>,
        every: Duration,
    },
    /// One occurrence per local calendar day at `time`.
    Daily { time: LocalTime, timezone: Tz },
    /// One occurrence on each selected local weekday at `time`.
    Weekly {
        weekdays: WeekdaySet,
        time: LocalTime,
        timezone: Tz,
    },
    /// Five-field cron evaluated in `timezone`.
    Cron { expression: String, timezone: Tz },
}

impl ScheduleTrigger {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Once { .. } => "once",
            Self::Every { .. } => "every",
            Self::Daily { .. } => "daily",
            Self::Weekly { .. } => "weekly",
            Self::Cron { .. } => "cron",
        }
    }

    pub const fn is_recurring(&self) -> bool {
        !matches!(self, Self::Once { .. })
    }

    pub const fn timezone(&self) -> Option<Tz> {
        match self {
            Self::Daily { timezone, .. }
            | Self::Weekly { timezone, .. }
            | Self::Cron { timezone, .. } => Some(*timezone),
            Self::Once { .. } | Self::Every { .. } => None,
        }
    }

    /// Validate the trigger's own bounds before it reaches storage.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        match self {
            Self::Once { .. } => Ok(()),
            Self::Every { every, .. } => {
                let seconds = every.as_secs();
                if seconds == 0 {
                    return Err(ScheduleError::Invalid(
                        "an interval schedule must be at least one second".into(),
                    ));
                }
                if seconds > 366 * 24 * 60 * 60 {
                    return Err(ScheduleError::Invalid(
                        "an interval schedule may not exceed one year".into(),
                    ));
                }
                Ok(())
            }
            Self::Daily { .. } => Ok(()),
            Self::Weekly { weekdays, .. } => {
                if weekdays.is_empty() {
                    return Err(ScheduleError::Invalid(
                        "a weekly schedule needs at least one weekday".into(),
                    ));
                }
                Ok(())
            }
            Self::Cron { expression, .. } => {
                parse_cron(expression)?;
                Ok(())
            }
        }
    }

    /// The first occurrence created with a new definition.
    ///
    /// A one-off and an interval keep their historic semantics: their anchor
    /// is the first slot even when it is already in the past, so a definition
    /// created with a past instant is due immediately. Calendar rules start at
    /// their next future slot; they never fire retroactively on creation.
    pub fn first_occurrence(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, ScheduleError> {
        self.validate()?;
        match self {
            Self::Once { at } => Ok(*at),
            Self::Every { anchor, .. } => Ok(*anchor),
            _ => self.next_after(now)?.ok_or_else(|| {
                ScheduleError::Invalid("the trigger has no future occurrence".into())
            }),
        }
    }

    /// The slot a definition change must leave behind.
    ///
    /// An update re-declares the rule, so the replacement slot is the first
    /// slot the new rule offers at `now`. A one-off has no successor, but its
    /// declared instant is still its only slot: as on creation, a past instant
    /// is due now rather than absent. Recurring rules keep their update
    /// semantics and advance to the first slot strictly after `now`, so editing
    /// an interval never resurrects its old anchor as an immediate run.
    ///
    /// Returning an error instead of `None` is deliberate: a successful update
    /// that leaves the definition without a slot would silently stop the
    /// schedule while the stored rule claims to be runnable.
    pub fn replacement_occurrence(
        &self,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ScheduleError> {
        match self {
            Self::Once { at } => Ok(*at),
            _ => self.next_after(now)?.ok_or_else(|| {
                ScheduleError::Invalid("the trigger has no future occurrence".into())
            }),
        }
    }

    /// The first occurrence strictly after `now`. `None` only for a one-off,
    /// which has no successor.
    pub fn next_after(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        self.validate()?;
        match self {
            Self::Once { .. } => Ok(None),
            Self::Every { anchor, every } => {
                let every_secs = every.as_secs();
                if every_secs == 0 {
                    return Err(ScheduleError::Invalid(
                        "an interval schedule must be at least one second".into(),
                    ));
                }
                if now < *anchor {
                    return Ok(Some(*anchor));
                }
                // The first grid point strictly after now. Computed from now
                // rather than a slot index so a very old anchor cannot
                // overflow the interval multiplication.
                let remainder = (now - *anchor).num_seconds().rem_euclid(every_secs as i64);
                Ok(Some(
                    now + chrono::Duration::seconds(every_secs as i64 - remainder),
                ))
            }
            Self::Daily { time, timezone } => {
                let mut best: Option<DateTime<Utc>> = None;
                for offset in 0..=8 {
                    let date = local_date(now, *timezone).checked_add_days(Days::new(offset));
                    let Some(date) = date else { continue };
                    let Some(candidate) = time.resolve_on(date, *timezone) else {
                        continue;
                    };
                    if candidate > now && best.is_none_or(|best| candidate < best) {
                        best = Some(candidate);
                    }
                }
                Ok(best)
            }
            Self::Weekly {
                weekdays,
                time,
                timezone,
            } => {
                let mut best: Option<DateTime<Utc>> = None;
                for offset in 0..=8 {
                    let date = local_date(now, *timezone).checked_add_days(Days::new(offset));
                    let Some(date) = date else { continue };
                    if !weekdays.contains(date.weekday()) {
                        continue;
                    }
                    let Some(candidate) = time.resolve_on(date, *timezone) else {
                        continue;
                    };
                    if candidate > now && best.is_none_or(|best| candidate < best) {
                        best = Some(candidate);
                    }
                }
                Ok(best)
            }
            Self::Cron {
                expression,
                timezone,
            } => {
                let cron = parse_cron(expression)?;
                // Cron has minute granularity and the OS wake fires at the
                // start of a minute. Floor the query instant so the returned
                // slot does not inherit an arbitrary sub-second offset.
                let local_now = now
                    .with_timezone(timezone)
                    .with_second(0)
                    .and_then(|time| time.with_nanosecond(0))
                    .unwrap_or_else(|| now.with_timezone(timezone));
                let next = cron
                    .find_next_occurrence(&local_now, false)
                    .map_err(|error| {
                        ScheduleError::Invalid(format!(
                            "cron expression `{expression}` has no next occurrence: {error}"
                        ))
                    })?;
                Ok(Some(next.with_timezone(&Utc)))
            }
        }
    }

    /// Compact human description used by the CLI.
    pub fn describe(&self) -> String {
        match self {
            Self::Once { at } => format!("once at {}", at.to_rfc3339()),
            Self::Every { anchor, every } => {
                format!("every {}s from {}", every.as_secs(), anchor.to_rfc3339())
            }
            Self::Daily { time, timezone } => format!("daily at {time} ({timezone})"),
            Self::Weekly {
                weekdays,
                time,
                timezone,
            } => format!("weekly on {weekdays} at {time} ({timezone})"),
            Self::Cron {
                expression,
                timezone,
            } => format!("cron `{expression}` ({timezone})"),
        }
    }
}

fn local_date(now: DateTime<Utc>, timezone: Tz) -> NaiveDate {
    now.with_timezone(&timezone).date_naive()
}

/// Resolve one wall-clock time in a zone under the documented DST policy.
fn resolve_local(date: NaiveDate, time: LocalTime, timezone: Tz) -> Option<DateTime<Utc>> {
    let naive = date.and_hms_opt(u32::from(time.hour), u32::from(time.minute), 0)?;
    match timezone.from_local_datetime(&naive) {
        chrono::LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
        // Fall back: the wall-clock time happens twice. Run the earlier one
        // once; the later repetition is not a second occurrence.
        chrono::LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        // Spring forward: the wall-clock time does not exist. Use the first
        // valid instant after the gap instead of skipping the day.
        chrono::LocalResult::None => {
            for minutes in 1..=180u32 {
                let candidate =
                    naive.checked_add_signed(chrono::Duration::minutes(i64::from(minutes)))?;
                match timezone.from_local_datetime(&candidate) {
                    chrono::LocalResult::Single(value) => {
                        return Some(value.with_timezone(&Utc));
                    }
                    chrono::LocalResult::Ambiguous(earliest, _) => {
                        return Some(earliest.with_timezone(&Utc));
                    }
                    chrono::LocalResult::None => {}
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .expect("test timestamp")
            .with_timezone(&Utc)
    }

    fn daily(raw: &str, zone: &str) -> ScheduleTrigger {
        ScheduleTrigger::Daily {
            time: LocalTime::parse(raw).expect("time"),
            timezone: parse_timezone(zone).expect("zone"),
        }
    }

    fn next(trigger: &ScheduleTrigger, now: &str) -> DateTime<Utc> {
        trigger
            .next_after(at(now))
            .expect("next")
            .expect("a future occurrence")
    }

    #[test]
    fn daily_uses_the_rules_timezone_not_utc() {
        let trigger = daily("09:30", "Asia/Shanghai");
        assert_eq!(
            next(&trigger, "2026-03-01T00:00:00Z"),
            at("2026-03-01T01:30:00Z")
        );
    }

    #[test]
    fn daily_moves_to_tomorrow_after_todays_slot() {
        let trigger = daily("09:30", "Asia/Shanghai");
        assert_eq!(
            next(&trigger, "2026-03-01T02:00:00Z"),
            at("2026-03-02T01:30:00Z")
        );
    }

    #[test]
    fn daily_spring_forward_runs_after_the_gap() {
        // 2026-03-08 02:00 America/New_York jumps to 03:00. A 02:30 rule must
        // run at the first valid instant, 03:00 EDT == 07:00Z, not be skipped.
        let trigger = daily("02:30", "America/New_York");
        assert_eq!(
            next(&trigger, "2026-03-08T00:00:00Z"),
            at("2026-03-08T07:00:00Z")
        );
    }

    #[test]
    fn daily_fall_back_ambiguous_time_runs_once_at_the_earlier_instant() {
        // 2026-11-01 01:30 happens at 05:30Z (EDT) and 06:30Z (EST).
        let trigger = daily("01:30", "America/New_York");
        assert_eq!(
            next(&trigger, "2026-10-31T12:00:00Z"),
            at("2026-11-01T05:30:00Z")
        );
        // After the first one ran, the repeated wall clock is not a second
        // occurrence; the next one is the following day in EST.
        assert_eq!(
            next(&trigger, "2026-11-01T05:30:00Z"),
            at("2026-11-02T06:30:00Z")
        );
    }

    #[test]
    fn weekly_crosses_month_and_year_boundaries() {
        let trigger = ScheduleTrigger::Weekly {
            weekdays: WeekdaySet::parse("mon,wed").expect("weekdays"),
            time: LocalTime::parse("09:00").expect("time"),
            timezone: parse_timezone("Europe/Berlin").expect("zone"),
        };
        // 2026-12-30 is a Wednesday; its 09:00 slot already passed.
        assert_eq!(
            next(&trigger, "2026-12-30T12:00:00Z"),
            at("2027-01-04T08:00:00Z")
        );
    }

    #[test]
    fn weekly_accepts_ranges_and_rejects_empty_sets() {
        let set = WeekdaySet::parse("mon-fri").expect("range");
        for day in [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ] {
            assert!(set.contains(day));
        }
        assert!(!set.contains(Weekday::Sat));
        assert!(WeekdaySet::parse("  ").is_err());
        assert!(WeekdaySet::parse("funday").is_err());
    }

    #[test]
    fn cron_next_occurrence_is_minute_aligned() {
        // The OS wake fires at the start of a minute. A slot carrying the
        // creation instant's sub-second part would be missed by that wake and
        // run one interval late, so cron slots are floored to the minute.
        let trigger = ScheduleTrigger::Cron {
            expression: "* * * * *".into(),
            timezone: parse_timezone("UTC").expect("zone"),
        };
        assert_eq!(
            next(&trigger, "2026-11-04T16:25:00.752Z"),
            at("2026-11-04T16:26:00Z")
        );
        assert_eq!(
            next(&trigger, "2026-11-04T16:25:30.120Z"),
            at("2026-11-04T16:26:00Z")
        );
    }

    #[test]
    fn cron_handles_month_end_and_leap_day() {
        let month_end = ScheduleTrigger::Cron {
            expression: "0 9 31 * *".into(),
            timezone: parse_timezone("UTC").expect("zone"),
        };
        assert_eq!(
            next(&month_end, "2026-04-01T00:00:00Z"),
            at("2026-05-31T09:00:00Z")
        );

        let leap = ScheduleTrigger::Cron {
            expression: "0 9 29 2 *".into(),
            timezone: parse_timezone("UTC").expect("zone"),
        };
        assert_eq!(
            next(&leap, "2026-03-01T00:00:00Z"),
            at("2028-02-29T09:00:00Z")
        );
    }

    #[test]
    fn cron_spring_forward_uses_the_same_gap_policy() {
        let trigger = ScheduleTrigger::Cron {
            expression: "30 2 8 3 *".into(),
            timezone: parse_timezone("America/New_York").expect("zone"),
        };
        assert_eq!(
            next(&trigger, "2026-03-01T00:00:00Z"),
            at("2026-03-08T07:00:00Z")
        );
    }

    #[test]
    fn cron_rejects_invalid_and_sub_minute_expressions() {
        assert!(parse_cron("not a cron").is_err());
        assert!(parse_cron("0 9 * *").is_err());
        assert!(parse_cron("0 0 9 * * *").is_err());
        assert!(
            ScheduleTrigger::Cron {
                expression: "not a cron".into(),
                timezone: parse_timezone("UTC").expect("zone"),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn one_off_has_no_successor() {
        let trigger = ScheduleTrigger::Once {
            at: at("2026-01-01T00:00:00Z"),
        };
        assert_eq!(
            trigger
                .next_after(at("2026-01-02T00:00:00Z"))
                .expect("next"),
            None
        );
        assert_eq!(
            trigger
                .first_occurrence(at("2026-01-02T00:00:00Z"))
                .expect("first"),
            at("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn replacement_keeps_a_one_off_at_its_declared_instant() {
        let trigger = ScheduleTrigger::Once {
            at: at("2026-01-01T00:00:00Z"),
        };
        assert_eq!(
            trigger
                .replacement_occurrence(at("2025-12-31T00:00:00Z"))
                .expect("future replacement"),
            at("2026-01-01T00:00:00Z")
        );
        // A past declaration is still the one-off's only slot: it is due now,
        // exactly as it is when the definition is created.
        assert_eq!(
            trigger
                .replacement_occurrence(at("2026-01-02T00:00:00Z"))
                .expect("past replacement"),
            at("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn replacement_advances_a_recurring_rule_past_its_old_anchor() {
        let trigger = ScheduleTrigger::Every {
            anchor: at("2026-01-01T00:00:00Z"),
            every: Duration::from_secs(3600),
        };
        // The anchor is already in the past. An update must schedule the next
        // grid point, not resurrect the anchor as an immediate run.
        assert_eq!(
            trigger
                .replacement_occurrence(at("2026-01-01T05:30:00Z"))
                .expect("replacement"),
            at("2026-01-01T06:00:00Z")
        );
    }

    #[test]
    fn replacement_rejects_a_rule_with_no_future_occurrence() {
        // February 30 never exists: the update must refuse rather than store a
        // definition whose only slot was silently dropped.
        let never = ScheduleTrigger::Cron {
            expression: "0 0 30 2 *".into(),
            timezone: parse_timezone("UTC").expect("zone"),
        };
        assert!(
            never
                .replacement_occurrence(at("2026-01-01T00:00:00Z"))
                .is_err()
        );
    }

    #[test]
    fn calendar_rules_never_create_a_past_first_occurrence() {
        let trigger = daily("09:00", "UTC");
        assert_eq!(
            trigger
                .first_occurrence(at("2026-01-01T12:00:00Z"))
                .expect("first"),
            at("2026-01-02T09:00:00Z")
        );
    }

    #[test]
    fn interval_next_is_strictly_after_now_on_the_anchor_grid() {
        let trigger = ScheduleTrigger::Every {
            anchor: at("2026-01-01T00:00:00Z"),
            every: Duration::from_secs(3600),
        };
        assert_eq!(
            next(&trigger, "2026-01-01T00:00:00Z"),
            at("2026-01-01T01:00:00Z")
        );
        assert_eq!(
            next(&trigger, "2026-01-01T00:00:01Z"),
            at("2026-01-01T01:00:00Z")
        );
        assert_eq!(
            next(&trigger, "2026-01-01T05:30:00Z"),
            at("2026-01-01T06:00:00Z")
        );
    }

    #[test]
    fn timezone_names_are_validated() {
        assert!(parse_timezone("Mars/Olympus").is_err());
        assert!(parse_timezone("").is_err());
        assert!(parse_timezone("Asia/Shanghai").is_ok());
    }
}
