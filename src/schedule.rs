//! Decide which pages are due to print, and remember which already printed.
//!
//! launchd wakes `calprint run` hourly. A page is due once its print moment
//! (Sunday evening for the week ahead; the last evening of the month for the
//! month ahead) has passed, it has not printed yet, and we are still within
//! the catch-up window. This makes runs idempotent and tolerant of the Mac
//! sleeping or being offline at the scheduled minute.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::render::{month_last, week_start_of};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// The week beginning on this date.
    Week(NaiveDate),
    Month(i32, u32),
}

impl Job {
    pub fn key(&self) -> String {
        match self {
            Job::Week(d) => format!("week {d}"),
            Job::Month(y, m) => format!("month {y}-{m:02}"),
        }
    }

    pub fn file_name(&self) -> String {
        match self {
            Job::Week(d) => format!("{d}-week.pdf"),
            Job::Month(y, m) => format!("{y}-{m:02}-month.pdf"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Keys of jobs that printed successfully, oldest first.
    pub printed: BTreeSet<String>,
}

pub fn state_path(cfg: &Config) -> PathBuf {
    cfg.state_dir.join("state.json")
}

impl State {
    pub fn load(path: &Path) -> Result<State> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&mut self, path: &Path) -> Result<()> {
        // Keys sort chronologically within each kind; keep the newest 40.
        while self.printed.len() > 40 {
            let oldest = self.printed.iter().min_by_key(|k| k.split_once(' ').map(|x| x.1.to_string())).cloned();
            self.printed.remove(&oldest.unwrap());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn at(tz: Tz, day: NaiveDate, time: NaiveTime) -> DateTime<Tz> {
    let naive = day.and_time(time);
    tz.from_local_datetime(&naive).earliest().unwrap_or_else(|| tz.from_utc_datetime(&naive))
}

/// Most recent weekly print moment at or before `now`, and the week it prints.
pub fn last_weekly(cfg: &Config, now: DateTime<Tz>) -> (DateTime<Tz>, Job) {
    let s = &cfg.schedule;
    let today = now.date_naive();
    let back = (today.weekday().num_days_from_monday() + 7 - s.weekly_day.num_days_from_monday()) % 7;
    let mut day = today - Duration::days(back as i64);
    if at(cfg.timezone, day, s.weekly_time) > now {
        day -= Duration::days(7);
    }
    // The first week that begins after the print day.
    let target = week_start_of(day, cfg.week_start) + Duration::days(7);
    (at(cfg.timezone, day, s.weekly_time), Job::Week(target))
}

/// Most recent monthly print moment (last day of a month) at or before `now`, and the month it prints.
pub fn last_monthly(cfg: &Config, now: DateTime<Tz>) -> (DateTime<Tz>, Job) {
    let s = &cfg.schedule;
    let today = now.date_naive();
    let mut last = month_last(today.year(), today.month());
    if at(cfg.timezone, last, s.monthly_time) > now {
        last = today.with_day(1).unwrap() - Duration::days(1);
    }
    let next = last + Duration::days(1);
    (at(cfg.timezone, last, s.monthly_time), Job::Month(next.year(), next.month()))
}

pub struct Due {
    pub job: Job,
    pub moment: DateTime<Tz>,
}

/// Jobs that should print now. Also returns jobs that were missed beyond the catch-up window.
pub fn due_jobs(cfg: &Config, now: DateTime<Tz>, state: &State) -> (Vec<Due>, Vec<Due>) {
    let mut candidates = Vec::new();
    if cfg.schedule.monthly {
        let (moment, job) = last_monthly(cfg, now);
        candidates.push(Due { job, moment });
    }
    if cfg.schedule.weekly {
        let (moment, job) = last_weekly(cfg, now);
        candidates.push(Due { job, moment });
    }
    let window = Duration::hours(cfg.schedule.catch_up_hours);
    let (mut due, mut missed) = (Vec::new(), Vec::new());
    for c in candidates {
        if state.printed.contains(&c.job.key()) {
            continue;
        }
        if now - c.moment <= window {
            due.push(c);
        } else {
            missed.push(c);
        }
    }
    (due, missed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        crate::config::parse(include_str!("../config.example.toml")).unwrap()
    }

    fn la(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
        chrono_tz::America::Los_Angeles.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn keys(v: &[Due]) -> Vec<String> {
        v.iter().map(|d| d.job.key()).collect()
    }

    #[test]
    fn sunday_evening_prints_next_week() {
        let cfg = cfg();
        let st = State::default();
        // Sunday Oct 18 2026, 17:59: nothing yet (last week's moment is long past the window).
        let (due, _) = due_jobs(&cfg, la(2026, 10, 18, 17, 59), &st);
        assert!(due.is_empty());
        // 18:00 sharp: the week of Monday Oct 19.
        let (due, _) = due_jobs(&cfg, la(2026, 10, 18, 18, 0), &st);
        assert_eq!(keys(&due), vec!["week 2026-10-19"]);
        // Still due at 05:00 Monday if the Mac slept through it (within 12h).
        let (due, _) = due_jobs(&cfg, la(2026, 10, 19, 5, 0), &st);
        assert_eq!(keys(&due), vec!["week 2026-10-19"]);
        // Too late by Monday afternoon: reported as missed instead.
        let (due, missed) = due_jobs(&cfg, la(2026, 10, 19, 13, 0), &st);
        assert!(due.is_empty());
        // (October's page, due Sep 30, is also long missed.)
        assert_eq!(keys(&missed), vec!["month 2026-10", "week 2026-10-19"]);
    }

    #[test]
    fn already_printed_is_skipped() {
        let cfg = cfg();
        let mut st = State::default();
        st.printed.insert("week 2026-10-19".into());
        let (due, missed) = due_jobs(&cfg, la(2026, 10, 18, 19, 0), &st);
        assert!(due.is_empty());
        assert!(!keys(&missed).contains(&"week 2026-10-19".to_string()));
    }

    #[test]
    fn last_day_of_month_prints_next_month() {
        let cfg = cfg();
        let st = State::default();
        let (due, _) = due_jobs(&cfg, la(2026, 10, 31, 18, 5), &st);
        assert_eq!(keys(&due), vec!["month 2026-11"]);
        let (due, _) = due_jobs(&cfg, la(2026, 10, 30, 18, 5), &st);
        assert!(due.is_empty());
        // December 31 rolls into the next year.
        let (due, _) = due_jobs(&cfg, la(2026, 12, 31, 20, 0), &st);
        assert_eq!(keys(&due), vec!["month 2027-01"]);
        // Feb 28 2027 is the last day of the month (and a Sunday): both print.
        let (due, _) = due_jobs(&cfg, la(2027, 2, 28, 18, 0), &st);
        assert_eq!(keys(&due), vec!["month 2027-03", "week 2027-03-01"]);
    }

    #[test]
    fn early_morning_catch_up_crosses_month_boundary() {
        let cfg = cfg();
        // 12:30am Nov 1: Oct 31's 6pm print is 6.5h old, still due.
        let (due, _) = due_jobs(&cfg, la(2026, 11, 1, 0, 30), &State::default());
        assert!(keys(&due).contains(&"month 2026-11".to_string()));
    }

    #[test]
    fn sunday_start_weeks() {
        let mut cfg = cfg();
        cfg.week_start = chrono::Weekday::Sun;
        let (_, job) = last_weekly(&cfg, la(2026, 10, 18, 18, 0));
        assert_eq!(job, Job::Week(NaiveDate::from_ymd_opt(2026, 10, 25).unwrap()));
        cfg.schedule.weekly_day = chrono::Weekday::Sat;
        let (_, job) = last_weekly(&cfg, la(2026, 10, 17, 18, 0));
        assert_eq!(job, Job::Week(NaiveDate::from_ymd_opt(2026, 10, 18).unwrap()));
    }
}
