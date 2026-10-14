//! Synthetic events for previewing layouts without touching any calendar.
//! Calendars are matched by their `initial`, falling back to no prefix.

use chrono::{Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Weekday};
use chrono_tz::Tz;

use crate::config::Config;
use crate::model::{Event, merge_duplicates};

fn at(tz: Tz, d: NaiveDate, h: u32, m: u32) -> chrono::DateTime<Tz> {
    tz.from_local_datetime(&d.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap())).earliest().unwrap()
}

pub fn events(cfg: &Config, first: NaiveDate, last: NaiveDate) -> Vec<Event> {
    let tz = cfg.timezone;
    let cal = |initial: &str| -> Vec<usize> { cfg.calendars.iter().position(|c| c.initial == initial).into_iter().collect() };
    let shade: Vec<usize> = cfg.calendars.iter().position(|c| c.shade_days).into_iter().collect();
    let mut out = Vec::new();
    let mut add = |title: &str, start, end, all_day, recurring, calendars: Vec<usize>, loc: Option<&str>| {
        out.push(Event { title: title.into(), start, end, all_day, recurring, daily: false, calendars, location: loc.map(String::from) });
    };
    let mut d = first;
    while d <= last {
        let day = |h, m| at(tz, d, h, m);
        let (midnight, next) = (at(tz, d, 0, 0), at(tz, d + Duration::days(1), 0, 0));
        match d.weekday() {
            Weekday::Mon => {
                add("Writing", day(15, 30), day(16, 30), false, true, cal("C"), None);
                add("B: Pick up C @ School", day(16, 30), day(17, 0), false, true, vec![], None);
                add("Swim", day(19, 15), day(20, 0), false, true, cal("D"), Some("Aquatic Center"));
            }
            Weekday::Tue => {
                add("PE", day(15, 30), day(16, 30), false, true, cal("C"), None);
                add("Swim", day(19, 0), day(19, 45), false, true, cal("D"), None);
            }
            Weekday::Wed => add("Writing", day(15, 30), day(16, 30), false, true, cal("C"), None),
            Weekday::Thu => {
                add("Chess", day(15, 30), day(16, 30), false, true, cal("C"), None);
                add("Swim", day(19, 0), day(19, 45), false, true, cal("D"), None);
            }
            Weekday::Fri => {
                add("Soccer", day(15, 0), day(16, 0), false, true, cal("C"), None);
                add("B: Pick up C @ School", day(16, 30), day(17, 0), false, true, vec![], None);
            }
            Weekday::Sun => {
                for c in cal("C").into_iter().chain(cal("D")) {
                    add("Sunday school", day(10, 15), day(12, 15), false, true, vec![c], None);
                }
            }
            Weekday::Sat => {}
        }
        match d.day() {
            4 => add("Parent-teacher conference", day(17, 0), day(17, 30), false, false, cal("A"), Some("School, Palo Alto")),
            9 => add("Taylor's Birthday", day(14, 30), day(15, 30), false, false, cal("C"), Some("San Francisco, CA")),
            11 => add("Veterans Day", midnight, next, true, false, shade.clone(), None),
            13 => add("No school", midnight, next, true, false, cal("D"), None),
            16 => {
                for c in ["A", "C", "D"] {
                    add("Grand Canyon trip", midnight, at(tz, d + Duration::days(2), 0, 0), true, false, cal(c), None);
                }
            }
            21 => add("B+C+D: Elise's 6th Birthday", day(16, 0), day(18, 0), false, false, vec![], None),
            24 => add("In London", midnight, at(tz, d + Duration::days(4), 0, 0), true, false, cal("A"), None),
            _ => {}
        }
        d += Duration::days(1);
    }
    merge_duplicates(out)
}
