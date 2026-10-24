//! Next occurrence of a recurring task. Pure functions over ClickUp's unix milliseconds.
//! Intervals are counted in the guild's time zone, so a task due at 09:00 stays at 09:00
//! across DST changes, and month steps keep the day of the month where it exists (Jan 31
//! plus one month is the last day of February).

use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, Zoned};

use super::Unit;

/// Upper bound on intervals stepped through while catching up, far beyond any backlog.
const MAX_STEPS: i64 = 1000;

fn zoned(ms: i64, tz: &TimeZone) -> Option<Zoned> {
    Some(Timestamp::from_millisecond(ms).ok()?.to_zoned(tz.clone()))
}

fn span(unit: Unit, n: i64) -> Option<Span> {
    match unit {
        Unit::Day => Span::new().try_days(n).ok(),
        Unit::Week => Span::new().try_weeks(n).ok(),
        Unit::Month => Span::new().try_months(n).ok(),
        Unit::Year => Span::new().try_years(n).ok(),
    }
}

/// `ms` moved by `steps` intervals of `every` `unit`s, counted from `ms` itself so month
/// ends do not drift within one move.
pub fn shift(ms: i64, steps: i64, every: u16, unit: Unit, tz: &TimeZone) -> Option<i64> {
    let n = steps.checked_mul(i64::from(every))?;
    let moved = zoned(ms, tz)?.checked_add(span(unit, n)?).ok()?;
    Some(moved.timestamp().as_millisecond())
}

/// The longest one interval can be, used to skip missed occurrences without stepping
/// through each of them.
fn max_len_ms(every: u16, unit: Unit) -> i64 {
    // A day with a DST change can have 25 hours.
    let day = 25 * 3600 * 1000;
    i64::from(every)
        * match unit {
            Unit::Day => day,
            Unit::Week => 7 * day,
            Unit::Month => 31 * day,
            Unit::Year => 366 * day,
        }
}

/// The first occurrence after `now_ms`: `due_ms` plus one or more intervals, together with
/// the number of intervals so the start date can move by the same amount. Occurrences
/// that passed while nothing ran are skipped. None when the date is out of range.
pub fn next_due(
    due_ms: i64,
    every: u16,
    unit: Unit,
    tz: &TimeZone,
    now_ms: i64,
) -> Option<(i64, i64)> {
    if every == 0 {
        return None;
    }
    // Never past the answer: due + first intervals is at most now.
    let first = (now_ms.saturating_sub(due_ms) / max_len_ms(every, unit)).max(1);
    for steps in first..first + MAX_STEPS {
        let next = shift(due_ms, steps, every, unit, tz)?;
        if next > now_ms {
            return Some((next, steps));
        }
    }
    None
}

/// 04:00 sharp in `tz`, which is how ClickUp stores a date without a time of day.
pub fn looks_dateless(ms: i64, tz: &TimeZone) -> bool {
    zoned(ms, tz)
        .is_some_and(|z| (z.hour(), z.minute(), z.second(), z.subsec_nanosecond()) == (4, 0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    /// Local time in `zone` as unix milliseconds.
    fn at(zone: &str, civil: &str) -> i64 {
        let dt: jiff::civil::DateTime = civil.parse().unwrap();
        dt.to_zoned(tz(zone)).unwrap().timestamp().as_millisecond()
    }

    fn local(ms: i64, zone: &str) -> String {
        zoned(ms, &tz(zone))
            .unwrap()
            .strftime("%Y-%m-%d %H:%M")
            .to_string()
    }

    #[test]
    fn steps_by_each_unit() {
        let utc = tz("UTC");
        let due = at("UTC", "2026-03-10T09:00");
        let now = due + 1;
        let cases = [
            (1, Unit::Day, "2026-03-11 09:00"),
            (3, Unit::Day, "2026-03-13 09:00"),
            (1, Unit::Week, "2026-03-17 09:00"),
            (2, Unit::Week, "2026-03-24 09:00"),
            (1, Unit::Month, "2026-04-10 09:00"),
            (1, Unit::Year, "2027-03-10 09:00"),
        ];
        for (every, unit, want) in cases {
            let (next, steps) = next_due(due, every, unit, &utc, now).unwrap();
            assert_eq!(local(next, "UTC"), want, "{every} {unit:?}");
            assert_eq!(steps, 1);
        }
        assert!(next_due(due, 0, Unit::Day, &utc, now).is_none());
    }

    #[test]
    fn month_ends_are_clamped() {
        let utc = tz("UTC");
        let jan31 = at("UTC", "2026-01-31T10:00");
        let (next, _) = next_due(jan31, 1, Unit::Month, &utc, jan31).unwrap();
        assert_eq!(local(next, "UTC"), "2026-02-28 10:00");
        let leap = at("UTC", "2028-01-31T10:00");
        let (next, _) = next_due(leap, 1, Unit::Month, &utc, leap).unwrap();
        assert_eq!(local(next, "UTC"), "2028-02-29 10:00");
        // Two months from the 31st, counted from the original date, keeps the 31st.
        let now = at("UTC", "2026-03-01T00:00");
        let (next, steps) = next_due(jan31, 1, Unit::Month, &utc, now).unwrap();
        assert_eq!(
            (local(next, "UTC").as_str(), steps),
            ("2026-03-31 10:00", 2)
        );
    }

    #[test]
    fn local_time_survives_dst() {
        // Europe/Warsaw leaves summer time on 2026-10-25.
        let due = at("Europe/Warsaw", "2026-10-22T09:00");
        let (next, _) = next_due(due, 1, Unit::Week, &tz("Europe/Warsaw"), due).unwrap();
        assert_eq!(local(next, "Europe/Warsaw"), "2026-10-29 09:00");
        assert_eq!(next - due, (7 * 24 + 1) * 3600 * 1000, "one hour longer");
        // In UTC the same step is exactly a week, so the local time moves.
        let (utc_next, _) = next_due(due, 1, Unit::Week, &tz("UTC"), due).unwrap();
        assert_eq!(local(utc_next, "Europe/Warsaw"), "2026-10-29 08:00");
    }

    #[test]
    fn catches_up_to_the_first_future_occurrence() {
        let utc = tz("UTC");
        let due = at("UTC", "2026-01-05T09:00");
        // Seven weeks and a bit later: weeks 1 to 7 passed, the 8th is next.
        let now = at("UTC", "2026-02-23T12:00");
        let (next, steps) = next_due(due, 1, Unit::Week, &utc, now).unwrap();
        assert_eq!(
            (local(next, "UTC").as_str(), steps),
            ("2026-03-02 09:00", 8)
        );
        // Exactly at an occurrence, that one counts as passed.
        let (next, _) = next_due(due, 1, Unit::Week, &utc, at("UTC", "2026-01-12T09:00")).unwrap();
        assert_eq!(local(next, "UTC"), "2026-01-19 09:00");
        // Daily, years behind.
        let (next, _) = next_due(due, 1, Unit::Day, &utc, at("UTC", "2029-06-01T08:00")).unwrap();
        assert_eq!(local(next, "UTC"), "2029-06-01 09:00");
        let (next, _) = next_due(due, 1, Unit::Day, &utc, at("UTC", "2029-06-01T10:00")).unwrap();
        assert_eq!(local(next, "UTC"), "2029-06-02 09:00");
    }

    #[test]
    fn early_completion_keeps_the_cadence() {
        let utc = tz("UTC");
        let due = at("UTC", "2026-03-13T17:00");
        // Done two days before it was due: the next one is a week after the due date.
        let now = at("UTC", "2026-03-11T10:00");
        let (next, steps) = next_due(due, 1, Unit::Week, &utc, now).unwrap();
        assert_eq!(
            (local(next, "UTC").as_str(), steps),
            ("2026-03-20 17:00", 1)
        );
    }

    #[test]
    fn start_dates_move_with_the_due_date() {
        let utc = tz("UTC");
        let start = at("UTC", "2026-01-31T08:00");
        assert_eq!(
            local(shift(start, 2, 1, Unit::Month, &utc).unwrap(), "UTC"),
            "2026-03-31 08:00"
        );
        assert_eq!(
            local(shift(start, 3, 2, Unit::Day, &utc).unwrap(), "UTC"),
            "2026-02-06 08:00"
        );
    }

    #[test]
    fn dateless_due_dates() {
        let warsaw = tz("Europe/Warsaw");
        assert!(looks_dateless(
            at("Europe/Warsaw", "2026-05-04T04:00"),
            &warsaw
        ));
        assert!(!looks_dateless(
            at("Europe/Warsaw", "2026-05-04T04:01"),
            &warsaw
        ));
        assert!(!looks_dateless(at("UTC", "2026-05-04T04:00"), &warsaw));
    }
}
