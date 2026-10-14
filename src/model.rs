//! Core event model shared by the calendar sources and the renderers.

use chrono::{DateTime, Duration, NaiveDate};
use chrono_tz::Tz;

#[derive(Debug, Clone)]
pub struct Event {
    pub title: String,
    /// In the display timezone.
    pub start: DateTime<Tz>,
    /// Exclusive.
    pub end: DateTime<Tz>,
    pub all_day: bool,
    /// Part of a repeating series (shown light; one-off events are bold).
    pub recurring: bool,
    /// Repeats every day (or at least five days a week); its time isn't worth printing.
    pub daily: bool,
    /// Indices into `Config::calendars` this event appears on (several after merging duplicates).
    pub calendars: Vec<usize>,
    pub location: Option<String>,
}

impl Event {
    pub fn first_day(&self) -> NaiveDate {
        self.start.date_naive()
    }

    /// Last local date this event touches (end is exclusive).
    pub fn last_day(&self) -> NaiveDate {
        let last = if self.all_day {
            self.end.date_naive() - Duration::days(1)
        } else {
            // An event ending exactly at midnight does not touch the next day.
            (self.end - Duration::nanoseconds(1)).date_naive()
        };
        last.max(self.first_day())
    }

    pub fn touches(&self, day: NaiveDate) -> bool {
        self.first_day() <= day && day <= self.last_day()
    }

    pub fn is_multi_day(&self) -> bool {
        self.last_day() > self.first_day()
    }
}

/// Whether an RRULE value (e.g. "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR") repeats daily
/// or on at least five days of the week.
pub fn is_daily_rule(rule: &str) -> bool {
    let rule = rule.trim_start_matches("RRULE:");
    let part = |key: &str| rule.split(';').find_map(|p| p.strip_prefix(key));
    let interval_one = part("INTERVAL=").is_none_or(|i| i.trim() == "1");
    match part("FREQ=") {
        Some("DAILY") => interval_one,
        Some("WEEKLY") => interval_one && part("BYDAY=").is_some_and(|d| d.split(',').count() >= 5),
        _ => false,
    }
}

/// Merge copies of the same event that appear on several calendars (e.g. a
/// family trip on each person's calendar) into one event listing every calendar.
pub fn merge_duplicates(events: Vec<Event>) -> Vec<Event> {
    let key = |x: &Event| (x.title.trim().to_lowercase(), x.start, x.end, x.all_day);
    let mut out: Vec<Event> = Vec::with_capacity(events.len());
    for e in events {
        match out.iter_mut().find(|o| key(o) == key(&e)) {
            Some(existing) => {
                for c in e.calendars {
                    if !existing.calendars.contains(&c) {
                        existing.calendars.push(c);
                    }
                }
                existing.recurring &= e.recurring;
                existing.daily &= e.daily;
                if existing.location.is_none() {
                    existing.location = e.location;
                }
            }
            None => out.push(e),
        }
    }
    for e in &mut out {
        e.calendars.sort();
    }
    out
}

/// Events touching `day`: all-day and multi-day first, then by start time.
pub fn events_on(events: &[Event], day: NaiveDate) -> Vec<&Event> {
    let mut hits: Vec<&Event> = events.iter().filter(|e| e.touches(day)).collect();
    hits.sort_by_key(|e| (!(e.all_day || e.is_multi_day()), e.start.max(crate::sources::midnight(e.start.timezone(), day)), e.title.to_lowercase()));
    hits
}

#[cfg(test)]
pub fn test_event(start: DateTime<Tz>, end: DateTime<Tz>, all_day: bool) -> Event {
    Event { title: "x".into(), start, end, all_day, recurring: false, daily: false, calendars: vec![0], location: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn day_span() {
        let tz = chrono_tz::America::Los_Angeles;
        let d = |m, d, h| tz.with_ymd_and_hms(2026, m, d, h, 0, 0).unwrap();
        // All-day on Oct 14 (end exclusive Oct 15).
        let e = test_event(d(10, 14, 0), d(10, 15, 0), true);
        assert!(!e.is_multi_day());
        assert_eq!(e.last_day(), NaiveDate::from_ymd_opt(2026, 10, 14).unwrap());
        // Timed ending at midnight stays on one day.
        let e = test_event(d(10, 14, 20), d(10, 15, 0), false);
        assert!(!e.is_multi_day());
        // Overnight timed event spans two days.
        let e = test_event(d(10, 14, 20), d(10, 15, 2), false);
        assert!(e.is_multi_day());
    }

    #[test]
    fn daily_rules() {
        assert!(is_daily_rule("FREQ=DAILY;UNTIL=20260702T065959Z"));
        assert!(is_daily_rule("RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"));
        assert!(!is_daily_rule("FREQ=WEEKLY;BYDAY=MO,WE,TH"));
        assert!(!is_daily_rule("FREQ=DAILY;INTERVAL=2"));
        assert!(!is_daily_rule("FREQ=MONTHLY"));
    }

    #[test]
    fn merges_shared_events() {
        let tz = chrono_tz::America::Los_Angeles;
        let d = |h| tz.with_ymd_and_hms(2026, 6, 6, h, 0, 0).unwrap();
        let mut a = test_event(d(9), d(10), false);
        a.title = "Fly to BOS".into();
        a.calendars = vec![3];
        let mut b = a.clone();
        b.title = "fly to BOS ".into();
        b.calendars = vec![1];
        b.location = Some("SFO".into());
        let mut other = a.clone();
        other.start = d(8);
        let merged = merge_duplicates(vec![a, b, other]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].calendars, vec![1, 3]);
        assert_eq!(merged[0].location.as_deref(), Some("SFO"));
    }
}
