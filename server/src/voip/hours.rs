//! Office hours, and a menu (spec 0118).
//!
//! Both decisions here are **pure**: they take their inputs and return an answer, with no
//! database, no provider and no clock of their own. That is what turns "is 23:59 on a
//! Sunday inside Monday's window" from an argument into a test, and it is the same
//! discipline `verify_webhook` already follows for exactly the same reason.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// A week of opening times, Monday first, as minutes from midnight.
///
/// Minutes rather than `TIME` because every bug this kind of feature has is arithmetic
/// across a boundary, and integers do not have the rest of `TIME`'s opinions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Week {
    /// Seven entries, Monday..Sunday. `None` means closed all day.
    pub opens_at: Vec<Option<i32>>,
    pub closes_at: Vec<Option<i32>>,
}

impl Week {
    /// Build from the two Postgres arrays, tolerating a short or missing week.
    ///
    /// A row with fewer than seven entries is treated as closed on the days it does not
    /// mention rather than as an error: the failure direction that leaves a caller hearing
    /// the closed-hours message is recoverable, and the one that rings a deserted office
    /// at 3am is not.
    pub fn from_arrays(opens: &[i32], closes: &[i32]) -> Self {
        let pick = |v: &[i32], i: usize| v.get(i).copied().filter(|m| *m >= 0);
        Self {
            opens_at: (0..7).map(|i| pick(opens, i)).collect(),
            closes_at: (0..7).map(|i| pick(closes, i)).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.opens_at.iter().all(Option::is_none)
    }
}

/// Is the office open?
///
/// `None` for the week means **always open**: an organisation that has not said otherwise
/// has not asked to be closed, and a number that silently stopped answering because
/// nobody filled in a form would be the worst possible default.
///
/// Evaluated in the ORGANISATION's timezone. Not the server's — a deployment in
/// Netherlands must not close a Milan office an hour early — and not the caller's, who
/// does not decide when somebody else's office is open.
pub fn is_open(now: DateTime<Utc>, timezone: &str, week: Option<&Week>) -> bool {
    let Some(week) = week else { return true };
    if week.is_empty() {
        return true;
    }

    // An unparseable timezone falls back to UTC rather than to "closed". A typo in a
    // configuration field must not take a company's telephone off the air.
    let tz: Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
    let local = tz.from_utc_datetime(&now.naive_utc());
    let index = local.weekday().num_days_from_monday() as usize;
    let minutes = (local.hour() * 60 + local.minute()) as i32;

    let (Some(open), Some(close)) = (
        week.opens_at.get(index).copied().flatten(),
        week.closes_at.get(index).copied().flatten(),
    ) else {
        return false;
    };

    if close > open {
        minutes >= open && minutes < close
    } else {
        // A window that crosses midnight — a support line open 22:00 to 06:00 is a real
        // thing, and reading it as "open from 22:00 until 06:00 the same morning", which
        // is never, would be a silent failure.
        minutes >= open || minutes < close
    }
}

/// One menu option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IvrOption {
    pub digit: String,
    pub label: String,
}

/// Which option a keypress chose.
///
/// A digit with nothing behind it returns `None`, and the caller is told and the menu
/// repeats once — not silence, and not an immediate hang-up. Somebody who mistypes on a
/// telephone keypad has done nothing wrong.
pub fn route_for<'a>(digit: &str, options: &'a [IvrOption]) -> Option<&'a IvrOption> {
    let key = digit.trim();
    if key.is_empty() {
        return None;
    }
    options.iter().find(|o| o.digit == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn week(open: i32, close: i32) -> Week {
        Week {
            opens_at: vec![Some(open); 7],
            closes_at: vec![Some(close); 7],
        }
    }

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn no_hours_configured_means_always_open() {
        // The failure direction matters: a number that silently stopped answering because
        // nobody filled in a form is the worst possible default.
        assert!(is_open(at("2026-09-12T03:00:00Z"), "Europe/Rome", None));
        assert!(is_open(
            at("2026-09-12T03:00:00Z"),
            "Europe/Rome",
            Some(&Week::default())
        ));
    }

    #[test]
    fn the_organisations_timezone_decides_not_the_servers() {
        // 09:00 in Milan is 07:00 UTC. A process running in UTC must not close a Milan
        // office two hours early.
        let nine_to_five = week(9 * 60, 17 * 60);
        assert!(
            is_open(
                at("2026-09-11T07:30:00Z"),
                "Europe/Rome",
                Some(&nine_to_five)
            ),
            "09:30 in Milan should be open"
        );
        assert!(
            !is_open(at("2026-09-11T07:30:00Z"), "UTC", Some(&nine_to_five)),
            "the same instant is 07:30 in UTC, which is before nine"
        );
    }

    #[test]
    fn the_edges_of_a_window_are_where_this_goes_wrong() {
        let nine_to_five = week(9 * 60, 17 * 60);
        // 09:00 local, exactly.
        assert!(is_open(
            at("2026-09-11T07:00:00Z"),
            "Europe/Rome",
            Some(&nine_to_five)
        ));
        // 08:59 local.
        assert!(!is_open(
            at("2026-09-11T06:59:00Z"),
            "Europe/Rome",
            Some(&nine_to_five)
        ));
        // 17:00 local is CLOSED: the window is half-open, so "open until five" means the
        // last call can start at 16:59 rather than at five o'clock exactly.
        assert!(!is_open(
            at("2026-09-11T15:00:00Z"),
            "Europe/Rome",
            Some(&nine_to_five)
        ));
        // 16:59 local.
        assert!(is_open(
            at("2026-09-11T14:59:00Z"),
            "Europe/Rome",
            Some(&nine_to_five)
        ));
    }

    #[test]
    fn a_window_that_crosses_midnight_is_a_real_support_line() {
        // 22:00 to 06:00. Reading this as "open from 22:00 until 06:00 the same morning" —
        // which is never — would be a silent failure nobody notices until a customer does.
        let overnight = week(22 * 60, 6 * 60);
        // 21:00 is before the line opens, and the wrap must not swallow the hour before it.
        assert!(!is_open(
            at("2026-09-11T21:00:00Z"),
            "UTC",
            Some(&overnight)
        ));
        assert!(
            is_open(at("2026-09-11T22:00:00Z"), "UTC", Some(&overnight)),
            "opens at 22:00"
        );
        assert!(is_open(at("2026-09-11T23:30:00Z"), "UTC", Some(&overnight)));
        assert!(is_open(at("2026-09-12T02:00:00Z"), "UTC", Some(&overnight)));
        assert!(!is_open(
            at("2026-09-12T06:00:00Z"),
            "UTC",
            Some(&overnight)
        ));
        assert!(!is_open(
            at("2026-09-12T12:00:00Z"),
            "UTC",
            Some(&overnight)
        ));
    }

    #[test]
    fn a_day_with_no_window_is_closed_all_day() {
        let mut weekdays = week(9 * 60, 17 * 60);
        // Sunday is index 6.
        weekdays.opens_at[6] = None;
        weekdays.closes_at[6] = None;
        // 2026-09-13 is a Sunday.
        assert!(!is_open(at("2026-09-13T10:00:00Z"), "UTC", Some(&weekdays)));
        assert!(
            is_open(at("2026-09-14T10:00:00Z"), "UTC", Some(&weekdays)),
            "Monday"
        );
    }

    #[test]
    fn a_typo_in_the_timezone_does_not_take_the_phone_off_the_air() {
        let nine_to_five = week(9 * 60, 17 * 60);
        assert!(is_open(
            at("2026-09-11T10:00:00Z"),
            "Europe/Atlantis",
            Some(&nine_to_five)
        ));
    }

    #[test]
    fn a_short_week_closes_the_days_it_does_not_mention() {
        // Recoverable in the direction that matters: a caller hearing the closed message
        // is fixable, and a phone ringing in a deserted office at 3am is not.
        let partial = Week::from_arrays(&[9 * 60, 9 * 60], &[17 * 60, 17 * 60]);
        assert!(
            is_open(at("2026-09-14T10:00:00Z"), "UTC", Some(&partial)),
            "Monday is listed"
        );
        assert!(
            !is_open(at("2026-09-17T10:00:00Z"), "UTC", Some(&partial)),
            "Thursday is not"
        );
    }

    #[test]
    fn a_keypress_finds_its_option_and_a_stray_one_finds_nothing() {
        let options = vec![
            IvrOption {
                digit: "1".into(),
                label: "Sales".into(),
            },
            IvrOption {
                digit: "2".into(),
                label: "Support".into(),
            },
        ];
        assert_eq!(
            route_for("1", &options).map(|o| o.label.as_str()),
            Some("Sales")
        );
        assert_eq!(
            route_for(" 2 ", &options).map(|o| o.label.as_str()),
            Some("Support")
        );
        // Somebody who mistypes on a telephone keypad has done nothing wrong: they are
        // told, and the menu repeats once.
        assert!(route_for("9", &options).is_none());
        assert!(route_for("", &options).is_none());
        assert!(route_for("1", &[]).is_none());
    }
}
