//! When digests and reminders are due. Pure functions over unix seconds; local times are
//! resolved with the guild's IANA time zone, so a 09:00 digest stays at 09:00 across DST.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

/// A digest more than this late is skipped instead of posted.
pub const GRACE_SECS: i64 = 2 * 3600;
pub const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
pub const ALL_DAYS: u8 = 0b111_1111;

pub fn parse_tz(name: &str) -> Option<TimeZone> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    TimeZone::get(name).ok()
}

/// The canonical name of a time zone typed by a user, or None when it is unknown.
pub fn tz_name(raw: &str) -> Option<String> {
    let tz = parse_tz(raw)?;
    Some(
        tz.iana_name()
            .map(str::to_owned)
            .unwrap_or_else(|| raw.trim().to_owned()),
    )
}

/// Every time zone name the bundled database knows, sorted.
pub fn tz_names() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<String> = jiff::tz::db()
            .available()
            .map(|n| n.as_str().to_owned())
            .collect();
        names.sort();
        names
    })
}

fn zoned(ts: i64, tz: &TimeZone) -> Option<Zoned> {
    Some(Timestamp::from_second(ts).ok()?.to_zoned(tz.clone()))
}

/// `minute` minutes after local midnight on `date`. A time skipped by a DST change moves
/// forward (02:30 on a spring-forward night becomes 03:30).
fn at_local(date: Date, minute: u16, tz: &TimeZone) -> Option<i64> {
    let (h, m) = ((minute / 60).min(23) as i8, (minute % 60) as i8);
    let z = date.at(h, m, 0, 0).to_zoned(tz.clone()).ok()?;
    Some(z.timestamp().as_second())
}

fn on_day(days: u8, date: Date) -> bool {
    days & (1 << date.weekday().to_monday_zero_offset()) != 0
}

/// The most recent scheduled time at or before `now`, looking back a week.
pub fn latest_occurrence(now: i64, days: u8, minute: u16, tz: &TimeZone) -> Option<i64> {
    let mut date = zoned(now, tz)?.date();
    for _ in 0..8 {
        if on_day(days, date)
            && let Some(t) = at_local(date, minute, tz)
            && t <= now
        {
            return Some(t);
        }
        date = date.yesterday().ok()?;
    }
    None
}

/// The first scheduled time after `now`, looking ahead a week.
pub fn next_occurrence(now: i64, days: u8, minute: u16, tz: &TimeZone) -> Option<i64> {
    let mut date = zoned(now, tz)?.date();
    for _ in 0..8 {
        if on_day(days, date)
            && let Some(t) = at_local(date, minute, tz)
            && t > now
        {
            return Some(t);
        }
        date = date.tomorrow().ok()?;
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Digest {
    /// Post now for this scheduled time.
    Run(i64),
    /// Too late to post for this scheduled time, mark it handled.
    Skip(i64),
    Wait,
}

pub fn digest_decision(now: i64, last_run_at: Option<i64>, occurrence: Option<i64>) -> Digest {
    let Some(occ) = occurrence else {
        return Digest::Wait;
    };
    if last_run_at.is_some_and(|last| last >= occ) {
        return Digest::Wait;
    }
    if now - occ > GRACE_SECS {
        Digest::Skip(occ)
    } else {
        Digest::Run(occ)
    }
}

/// True from `lead_secs` before the event until it starts.
pub fn reminder_due(now: i64, at: i64, lead_secs: i64) -> bool {
    at > now && at - lead_secs <= now
}

/// `Every day`, `Mon-Fri`, `Weekends` or a list like `Mon, Wed`.
pub fn describe_days(days: u8) -> String {
    match days & ALL_DAYS {
        ALL_DAYS => "Every day".into(),
        0b001_1111 => "Mon-Fri".into(),
        0b110_0000 => "Weekends".into(),
        0 => "Never".into(),
        d => DAY_NAMES
            .iter()
            .enumerate()
            .filter(|(i, _)| d & (1 << i) != 0)
            .map(|(_, n)| *n)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

pub fn fmt_minute(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// `HH:MM` (also `H:MM`) to minutes after midnight.
pub fn parse_hhmm(raw: &str) -> Option<u16> {
    let (h, m) = raw.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && (1..=2).contains(&raw.trim().split(':').next()?.len()))
        .then_some(h * 60 + m)
}

/// `15 minutes`, `2 hours`, `1 day`.
pub fn describe_lead(minutes: u32) -> String {
    let plural = |n: u32, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if minutes > 0 && minutes.is_multiple_of(1440) {
        plural(minutes / 1440, "day")
    } else if minutes > 0 && minutes.is_multiple_of(60) {
        plural(minutes / 60, "hour")
    } else {
        plural(minutes, "minute")
    }
}

/// Local date and time with the zone abbreviation, for dashboard previews.
pub fn fmt_local(ts: i64, tz: &TimeZone) -> String {
    fmt_local_as(ts, tz, "%Y-%m-%d %H:%M %Z")
}

/// `ts` in `tz`, formatted with strftime style `format`.
pub fn fmt_local_as(ts: i64, tz: &TimeZone, format: &str) -> String {
    zoned(ts, tz)
        .map(|z| z.strftime(format).to_string())
        .unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_second()
    }

    fn warsaw() -> TimeZone {
        parse_tz("Europe/Warsaw").unwrap()
    }

    #[test]
    fn time_zones() {
        assert!(parse_tz("UTC").is_some());
        assert!(parse_tz(" Europe/Warsaw ").is_some());
        assert!(parse_tz("Mars/Olympus").is_none());
        assert!(parse_tz("").is_none());
        assert_eq!(tz_name(" Europe/Warsaw ").as_deref(), Some("Europe/Warsaw"));
        assert!(tz_name("Mars/Olympus").is_none());
        assert!(tz_names().iter().any(|n| n == "Europe/Warsaw"));
    }

    #[test]
    fn latest_and_next_in_utc() {
        let utc = TimeZone::UTC;
        // Thursday 2026-10-15 10:00 UTC, digest at 09:00.
        let now = ts("2026-10-15T10:00:00Z");
        assert_eq!(
            latest_occurrence(now, ALL_DAYS, 540, &utc),
            Some(ts("2026-10-15T09:00:00Z"))
        );
        assert_eq!(
            next_occurrence(now, ALL_DAYS, 540, &utc),
            Some(ts("2026-10-16T09:00:00Z"))
        );
        // Mondays only: last Monday, next Monday.
        assert_eq!(
            latest_occurrence(now, 0b1, 540, &utc),
            Some(ts("2026-10-12T09:00:00Z"))
        );
        assert_eq!(
            next_occurrence(now, 0b1, 540, &utc),
            Some(ts("2026-10-19T09:00:00Z"))
        );
        // Exactly at the scheduled time counts as reached.
        let at = ts("2026-10-15T09:00:00Z");
        assert_eq!(latest_occurrence(at, ALL_DAYS, 540, &utc), Some(at));
        assert_eq!(latest_occurrence(now, 0, 540, &utc), None);
        assert_eq!(next_occurrence(now, 0, 540, &utc), None);
    }

    #[test]
    fn local_time_survives_dst() {
        let tz = warsaw();
        // CET (+1) on Saturday, CEST (+2) from Sunday 2026-03-29.
        let sat = ts("2026-03-28T12:00:00Z");
        assert_eq!(
            latest_occurrence(sat, ALL_DAYS, 540, &tz),
            Some(ts("2026-03-28T08:00:00Z"))
        );
        let mon = ts("2026-03-30T12:00:00Z");
        assert_eq!(
            latest_occurrence(mon, ALL_DAYS, 540, &tz),
            Some(ts("2026-03-30T07:00:00Z"))
        );
        // 02:30 does not exist on 2026-03-29 and moves to 03:30 CEST.
        let sun = ts("2026-03-29T12:00:00Z");
        assert_eq!(
            latest_occurrence(sun, ALL_DAYS, 150, &tz),
            Some(ts("2026-03-29T01:30:00Z"))
        );
        assert_eq!(
            fmt_local(ts("2026-03-30T07:00:00Z"), &tz),
            "2026-03-30 09:00 CEST"
        );
    }

    #[test]
    fn digest_decisions() {
        let occ = 1_000_000;
        assert_eq!(digest_decision(occ, None, None), Digest::Wait);
        assert_eq!(digest_decision(occ + 60, None, Some(occ)), Digest::Run(occ));
        assert_eq!(
            digest_decision(occ + 60, Some(occ - 1), Some(occ)),
            Digest::Run(occ)
        );
        assert_eq!(
            digest_decision(occ + 60, Some(occ), Some(occ)),
            Digest::Wait
        );
        assert_eq!(
            digest_decision(occ + 60, Some(occ + 30), Some(occ)),
            Digest::Wait
        );
        assert_eq!(
            digest_decision(occ + GRACE_SECS, None, Some(occ)),
            Digest::Run(occ)
        );
        assert_eq!(
            digest_decision(occ + GRACE_SECS + 1, None, Some(occ)),
            Digest::Skip(occ)
        );
    }

    #[test]
    fn reminder_window() {
        let at = 10_000;
        let lead = 900;
        assert!(!reminder_due(at - lead - 1, at, lead));
        assert!(reminder_due(at - lead, at, lead));
        assert!(reminder_due(at - 1, at, lead));
        assert!(
            !reminder_due(at, at, lead),
            "started events are not reminded"
        );
        assert!(!reminder_due(at + 60, at, lead));
    }

    #[test]
    fn descriptions_and_parsing() {
        assert_eq!(describe_days(ALL_DAYS), "Every day");
        assert_eq!(describe_days(0b001_1111), "Mon-Fri");
        assert_eq!(describe_days(0b110_0000), "Weekends");
        assert_eq!(describe_days(0b000_0101), "Mon, Wed");
        assert_eq!(fmt_minute(545), "09:05");
        assert_eq!(parse_hhmm("09:05"), Some(545));
        assert_eq!(parse_hhmm("9:05"), Some(545));
        assert_eq!(parse_hhmm(" 23:59 "), Some(1439));
        for bad in ["24:00", "12:60", "1205", "", "ab:cd", "123:00", "-1:00"] {
            assert_eq!(parse_hhmm(bad), None, "{bad}");
        }
        assert_eq!(describe_lead(15), "15 minutes");
        assert_eq!(describe_lead(1), "1 minute");
        assert_eq!(describe_lead(120), "2 hours");
        assert_eq!(describe_lead(1440), "1 day");
    }
}
