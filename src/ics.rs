//! Minimal RFC 5545 reader for Google Calendar's iCal feeds: VEVENTs with
//! RRULE/RDATE/EXDATE recurrence and RECURRENCE-ID overrides.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rrule::{RRule, Unvalidated};

use crate::model::{Event, is_daily_rule};

/// Safety valve for runaway rules; far more than any page shows.
const MAX_OCCURRENCES: u16 = 5000;

struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, key: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct RawEvent {
    uid: String,
    summary: Option<String>,
    location: Option<String>,
    status: Option<String>,
    start: Option<(DateTime<Tz>, bool)>,
    end: Option<DateTime<Tz>>,
    duration: Option<Duration>,
    rrules: Vec<String>,
    rdates: Vec<DateTime<Tz>>,
    exdates: Vec<DateTime<Tz>>,
    recurrence_id: Option<DateTime<Tz>>,
}

fn unfold(text: &str) -> String {
    text.replace("\r\n", "\n").replace("\n ", "").replace("\n\t", "")
}

fn parse_line(line: &str) -> Option<Prop> {
    // The value starts at the first ':' outside double quotes.
    let mut in_quotes = false;
    let colon = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            in_quotes = !in_quotes;
        }
        c == ':' && !in_quotes
    })?.0;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().trim_matches('"').to_string()))
        .collect();
    Some(Prop { name, params, value: value.to_string() })
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn resolve_tz(tzid: Option<&str>, fallback: Tz) -> Tz {
    let Some(id) = tzid else { return fallback };
    if let Ok(tz) = id.parse::<Tz>() {
        return tz;
    }
    // e.g. "/mozilla.org/20050126_1/America/New_York"
    let parts: Vec<&str> = id.split('/').collect();
    for i in 0..parts.len() {
        if let Ok(tz) = parts[i..].join("/").parse::<Tz>() {
            return tz;
        }
    }
    fallback
}

fn localize(tz: Tz, naive: NaiveDateTime) -> DateTime<Tz> {
    tz.from_local_datetime(&naive)
        .earliest()
        // Nonexistent local time (spring-forward gap): shift past the gap.
        .unwrap_or_else(|| tz.from_utc_datetime(&(naive - Duration::hours(1))) + Duration::hours(1))
}

/// Parse one DATE or DATE-TIME value. All-day dates become midnight in `display`.
fn parse_value(value: &str, tzid: Option<&str>, value_type: Option<&str>, display: Tz) -> Result<(DateTime<Tz>, bool)> {
    let v = value.trim();
    if value_type == Some("DATE") || (v.len() == 8 && !v.contains('T')) {
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").with_context(|| format!("bad DATE {v:?}"))?;
        return Ok((localize(display, d.and_hms_opt(0, 0, 0).unwrap()), true));
    }
    if let Some(utc) = v.strip_suffix('Z') {
        let n = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").with_context(|| format!("bad DATE-TIME {v:?}"))?;
        return Ok((Utc.from_utc_datetime(&n).with_timezone(&chrono_tz::UTC), false));
    }
    let n = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").with_context(|| format!("bad DATE-TIME {v:?}"))?;
    // Floating times (no TZID) are interpreted in the display timezone.
    Ok((localize(resolve_tz(tzid, display), n), false))
}

fn parse_values(p: &Prop, display: Tz) -> Vec<DateTime<Tz>> {
    p.value
        .split(',')
        .filter_map(|v| parse_value(v, p.param("TZID"), p.param("VALUE"), display).ok().map(|(d, _)| d))
        .collect()
}

/// RFC 5545 DURATION, e.g. "PT1H30M", "P1D", "-P1W".
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let s = s.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut num = String::new();
    let mut in_time = false;
    for c in s.chars() {
        match c {
            '0'..='9' => num.push(c),
            'T' => in_time = true,
            _ => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += match (c, in_time) {
                    ('W', false) => Duration::weeks(n),
                    ('D', false) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    if !num.is_empty() {
        return None;
    }
    Some(if neg { -total } else { total })
}

fn read_events(text: &str, display: Tz) -> Vec<RawEvent> {
    let mut events = Vec::new();
    let mut current: Option<RawEvent> = None;
    // Depth of components nested inside the VEVENT (e.g. VALARM), whose props we skip.
    let mut nested = 0usize;
    for line in unfold(text).lines() {
        let Some(p) = parse_line(line) else { continue };
        match (p.name.as_str(), current.as_mut()) {
            ("BEGIN", None) if p.value.eq_ignore_ascii_case("VEVENT") => current = Some(RawEvent::default()),
            ("BEGIN", Some(_)) => nested += 1,
            ("END", Some(_)) if nested > 0 => nested -= 1,
            ("END", Some(_)) if p.value.eq_ignore_ascii_case("VEVENT") => events.extend(current.take()),
            (_, Some(ev)) if nested == 0 => match p.name.as_str() {
                "UID" => ev.uid = p.value.clone(),
                "SUMMARY" => ev.summary = Some(unescape(&p.value)),
                "LOCATION" => ev.location = Some(unescape(&p.value)),
                "STATUS" => ev.status = Some(p.value.to_ascii_uppercase()),
                "DTSTART" => ev.start = parse_value(&p.value, p.param("TZID"), p.param("VALUE"), display).ok(),
                "DTEND" => ev.end = parse_value(&p.value, p.param("TZID"), p.param("VALUE"), display).ok().map(|v| v.0),
                "DURATION" => ev.duration = parse_duration(&p.value),
                "RRULE" => ev.rrules.push(p.value.clone()),
                "RDATE" => ev.rdates.extend(parse_values(&p, display)),
                "EXDATE" => ev.exdates.extend(parse_values(&p, display)),
                "RECURRENCE-ID" => {
                    ev.recurrence_id = parse_value(&p.value, p.param("TZID"), p.param("VALUE"), display).ok().map(|v| v.0)
                }
                _ => {}
            },
            _ => {}
        }
    }
    events
}

/// RFC 5545 requires UNTIL in UTC when DTSTART has a timezone, but feeds often
/// use a bare DATE (all-day events) or floating time. Convert those to UTC,
/// treating a DATE as inclusive of that whole local day.
fn normalize_until(rule: &str, tz: Tz) -> String {
    rule.split(';')
        .map(|part| {
            let Some(v) = part.strip_prefix("UNTIL=") else { return part.to_string() };
            let naive = if v.len() == 8 {
                NaiveDate::parse_from_str(v, "%Y%m%d").ok().and_then(|d| d.and_hms_opt(23, 59, 59))
            } else if !v.ends_with('Z') {
                NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()
            } else {
                None
            };
            match naive {
                Some(n) => format!("UNTIL={}", localize(tz, n).with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")),
                None => part.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn to_rr(d: DateTime<Tz>) -> DateTime<rrule::Tz> {
    d.with_timezone(&rrule::Tz::from(d.timezone()))
}

/// Occurrence start times of a recurring event that could overlap [from, to).
fn occurrences(ev: &RawEvent, start: DateTime<Tz>, span: Duration, from: DateTime<Tz>, to: DateTime<Tz>) -> Result<Vec<DateTime<Tz>>> {
    let tz = start.timezone();
    let dtstart = to_rr(start);
    let mut set = None;
    for rule in &ev.rrules {
        let rule = normalize_until(rule, tz);
        // Google ends a series early by setting UNTIL before DTSTART; that series has no occurrences.
        let until = rule.split(';').find_map(|p| p.strip_prefix("UNTIL="));
        if let Some(until) = until.and_then(|u| NaiveDateTime::parse_from_str(u, "%Y%m%dT%H%M%SZ").ok())
            && Utc.from_utc_datetime(&until) < start.with_timezone(&Utc) {
                continue;
            }
        let parsed: RRule<Unvalidated> = rule.parse().with_context(|| format!("RRULE {rule:?}"))?;
        set = Some(match set {
            None => parsed.build(dtstart).with_context(|| format!("RRULE {rule:?}"))?,
            Some(s) => {
                let s: rrule::RRuleSet = s;
                s.rrule(parsed.validate(dtstart).with_context(|| format!("RRULE {rule:?}"))?)
            }
        });
    }
    let Some(mut set) = set else { return Ok(Vec::new()) };
    for d in &ev.rdates {
        set = set.rdate(to_rr(*d));
    }
    for d in &ev.exdates {
        set = set.exdate(to_rr(*d));
    }
    let result = set.after(to_rr(from - span - Duration::seconds(1))).before(to_rr(to)).all(MAX_OCCURRENCES);
    Ok(result.dates.into_iter().map(|d| d.with_timezone(&tz)).collect())
}

/// Parse an iCal feed and return events overlapping [from, to), in `display` timezone.
pub fn parse_feed(text: &str, cal_index: usize, cal_name: &str, from: DateTime<Tz>, to: DateTime<Tz>, display: Tz) -> Vec<Event> {
    let raw = read_events(text, display);
    // Instances replaced by a RECURRENCE-ID override, keyed by UID.
    let mut overridden: HashMap<&str, HashSet<i64>> = HashMap::new();
    for ev in raw.iter().filter(|e| e.recurrence_id.is_some()) {
        overridden.entry(ev.uid.as_str()).or_default().insert(ev.recurrence_id.unwrap().timestamp());
    }

    // Whether each series repeats daily, so its overridden instances inherit it.
    let daily: HashMap<&str, bool> = raw
        .iter()
        .filter(|e| e.recurrence_id.is_none() && !e.rrules.is_empty())
        .map(|e| (e.uid.as_str(), e.rrules.iter().any(|r| is_daily_rule(r))))
        .collect();

    let mut out = Vec::new();
    for ev in &raw {
        if ev.status.as_deref() == Some("CANCELLED") {
            continue;
        }
        let Some((start, all_day)) = ev.start else { continue };
        let span = match (ev.end, ev.duration) {
            (Some(end), _) => end - start,
            (None, Some(d)) => d,
            (None, None) if all_day => Duration::days(1),
            (None, None) => Duration::zero(),
        };
        let starts = if ev.rrules.is_empty() || ev.recurrence_id.is_some() {
            vec![start]
        } else {
            match occurrences(ev, start, span, from, to) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("warning: {cal_name}: {:?}: {e:#}; showing first instance only", ev.summary);
                    vec![start]
                }
            }
        };
        let skip = overridden.get(ev.uid.as_str());
        for s in starts {
            if ev.recurrence_id.is_none() && skip.is_some_and(|set| set.contains(&s.timestamp())) {
                continue;
            }
            let s = s.with_timezone(&display);
            let e = if all_day {
                // Keep all-day events on whole local days regardless of DST.
                let days = span.num_days().max(1);
                localize(display, (s.date_naive() + Duration::days(days)).and_hms_opt(0, 0, 0).unwrap())
            } else {
                s + span
            };
            if e > from && s < to || (s == e && s >= from && s < to) {
                out.push(Event {
                    title: ev.summary.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "(No title)".into()),
                    start: s,
                    end: e,
                    all_day,
                    recurring: !ev.rrules.is_empty() || ev.recurrence_id.is_some(),
                    daily: daily.get(ev.uid.as_str()).copied().unwrap_or(false),
                    calendars: vec![cal_index],
                    location: ev.location.clone().filter(|l| !l.trim().is_empty()),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LA: Tz = chrono_tz::America::Los_Angeles;


    fn window() -> (DateTime<Tz>, DateTime<Tz>) {
        (LA.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(), LA.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap())
    }

    const FEED: &str = "BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:weekly@test\r
DTSTART;TZID=America/Los_Angeles:20261005T090000\r
DTEND;TZID=America/Los_Angeles:20261005T100000\r
RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20261130T170000Z\r
EXDATE;TZID=America/Los_Angeles:20261109T090000\r
SUMMARY:Standup\\, team\r
BEGIN:VALARM\r
ACTION:DISPLAY\r
SUMMARY:Alarm should be ignored\r
END:VALARM\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:weekly@test\r
RECURRENCE-ID;TZID=America/Los_Angeles:20261116T090000\r
DTSTART;TZID=America/Los_Angeles:20261116T110000\r
DTEND;TZID=America/Los_Angeles:20261116T120000\r
SUMMARY:Standup (moved)\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:allday@test\r
DTSTART;VALUE=DATE:20261126\r
DTEND;VALUE=DATE:20261128\r
SUMMARY:Thanksgiving break with a very long title that is long\r
 continued\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:yearly@test\r
DTSTART;VALUE=DATE:20101103\r
DTEND;VALUE=DATE:20101104\r
RRULE:FREQ=YEARLY\r
SUMMARY:Birthday\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:cancel@test\r
DTSTART:20261110T200000Z\r
DTEND:20261110T210000Z\r
STATUS:CANCELLED\r
SUMMARY:Cancelled\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:utc@test\r
DTSTART:20261110T200000Z\r
DURATION:PT90M\r
SUMMARY:UTC event\r
LOCATION:Kitchen\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn expands_feed() {
        let (from, to) = window();
        let mut evs = parse_feed(FEED, 0, "Test", from, to, LA);
        evs.sort_by_key(|e| e.start);
        let titles: Vec<String> = evs.iter().map(|e| format!("{} {}", e.start.format("%m-%d %H:%M"), e.title)).collect();
        assert_eq!(
            titles,
            vec![
                "11-02 09:00 Standup, team",
                "11-03 00:00 Birthday",
                "11-10 12:00 UTC event",
                // 11-09 excluded by EXDATE; 11-16 replaced by override
                "11-16 11:00 Standup (moved)",
                "11-23 09:00 Standup, team",
                "11-26 00:00 Thanksgiving break with a very long title that is longcontinued",
                "11-30 09:00 Standup, team",
            ]
        );
        let utc = evs.iter().find(|e| e.title == "UTC event").unwrap();
        assert_eq!((utc.end - utc.start).num_minutes(), 90);
        assert_eq!(utc.location.as_deref(), Some("Kitchen"));
        let tg = evs.iter().find(|e| e.title.starts_with("Thanksgiving")).unwrap();
        assert!(tg.all_day && tg.is_multi_day());
        assert_eq!(tg.last_day(), NaiveDate::from_ymd_opt(2026, 11, 27).unwrap());
    }

    #[test]
    fn dst_keeps_wall_clock() {
        // Weekly 9am across the Nov 1 2026 DST change stays at 9am local.
        let (from, to) = window();
        let evs = parse_feed(FEED, 0, "Test", from, to, LA);
        assert!(evs.iter().filter(|e| e.title.starts_with("Standup,")).all(|e| e.start.format("%H:%M").to_string() == "09:00"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1W"), Some(Duration::weeks(1)));
        assert_eq!(parse_duration("-P1DT2H"), Some(-(Duration::days(1) + Duration::hours(2))));
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn all_day_rule_with_date_until() {
        let feed = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nDTSTART;VALUE=DATE:20261102\nDTEND;VALUE=DATE:20261103\nRRULE:FREQ=WEEKLY;UNTIL=20261116;BYDAY=MO\nSUMMARY:Trash day\nEND:VEVENT\nEND:VCALENDAR\n";
        let (from, to) = window();
        let evs = parse_feed(feed, 0, "Test", from, to, LA);
        let days: Vec<String> = evs.iter().map(|e| e.start.format("%m-%d").to_string()).collect();
        assert_eq!(days, vec!["11-02", "11-09", "11-16"]);
        assert!(evs.iter().all(|e| e.all_day && !e.is_multi_day()));
    }

    #[test]
    fn series_ended_before_it_started_is_empty() {
        let feed = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nDTSTART;TZID=America/Los_Angeles:20261103T154500\nDTEND;TZID=America/Los_Angeles:20261103T163000\nRRULE:FREQ=DAILY;UNTIL=20261102T065959Z\nSUMMARY:Pickup\nEND:VEVENT\nEND:VCALENDAR\n";
        let (from, to) = window();
        assert!(parse_feed(feed, 0, "Test", from, to, LA).is_empty());
    }

    #[test]
    fn marks_daily_series() {
        let feed = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:camp\nDTSTART;TZID=America/Los_Angeles:20261102T090000\nDTEND;TZID=America/Los_Angeles:20261102T150000\nRRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;COUNT=5\nSUMMARY:Camp\nEND:VEVENT\nBEGIN:VEVENT\nUID:camp\nRECURRENCE-ID;TZID=America/Los_Angeles:20261104T090000\nDTSTART;TZID=America/Los_Angeles:20261104T100000\nDTEND;TZID=America/Los_Angeles:20261104T150000\nSUMMARY:Camp (late)\nEND:VEVENT\nBEGIN:VEVENT\nUID:chess\nDTSTART;TZID=America/Los_Angeles:20261105T153000\nDTEND;TZID=America/Los_Angeles:20261105T163000\nRRULE:FREQ=WEEKLY\nSUMMARY:Chess\nEND:VEVENT\nEND:VCALENDAR\n";
        let (from, to) = window();
        let evs = parse_feed(feed, 0, "Test", from, to, LA);
        assert!(evs.iter().filter(|e| e.title.starts_with("Camp")).count() == 5);
        assert!(evs.iter().filter(|e| e.title.starts_with("Camp")).all(|e| e.daily && e.recurring));
        assert!(evs.iter().filter(|e| e.title == "Chess").all(|e| !e.daily && e.recurring));
    }
}
