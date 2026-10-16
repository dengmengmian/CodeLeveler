//! UTC timestamps without a date library.
//!
//! Release state is read by a human and compared against the dogfood lab's
//! timestamps, which are ISO-8601 UTC. One representation, rendered the same
//! way every time, so `--status` and the report never disagree.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_secs_f64(),
        Err(err) => -(err.duration().as_secs_f64()),
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the same shape the dogfood lab writes.
pub fn iso8601(epoch: f64) -> String {
    let total = epoch.floor() as i64;
    let seconds_of_day = total.rem_euclid(86_400);
    let days = (total - seconds_of_day) / 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

pub fn parse_iso8601(text: &str) -> Option<f64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> {
        std::str::from_utf8(bytes.get(from..to)?).ok()?.parse().ok()
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    // Free-form fractional seconds and offsets are tolerated on read: a
    // timestamp we cannot place exactly still orders correctly to the second.
    let fraction = text
        .get(19..)
        .and_then(|rest| rest.strip_prefix('.'))
        .map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                0.0
            } else {
                let value: f64 = format!("0.{digits}").parse().unwrap_or(0.0);
                value
            }
        })
        .unwrap_or(0.0);
    let days = days_from_civil(year, month, day);
    Some(days as f64 * 86_400.0 + (hour * 3600 + minute * 60 + second) as f64 + fraction)
}

/// Howard Hinnant's `days_from_civil`, with the Unix epoch as day 0.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `1h 03m 04s` — a duration that reads the same in a status line and a report.
pub fn human_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let rest = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {rest:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {rest:02}s")
    } else {
        format!("{rest}s")
    }
}
