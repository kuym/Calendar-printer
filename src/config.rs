//! TOML configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{NaiveTime, Weekday};
use chrono_tz::Tz;
use serde::Deserialize;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

pub fn default_config_path() -> PathBuf {
    std::env::var("CALPRINT_CONFIG").map(PathBuf::from).unwrap_or_else(|_| home().join(".config/calprint/config.toml"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    timezone: Option<String>,
    week_starts_on: Option<String>,
    qr_url: Option<String>,
    #[serde(default)]
    people: BTreeMap<String, String>,
    #[serde(default)]
    calendars: Vec<RawCalendar>,
    #[serde(default)]
    printer: RawPrinter,
    #[serde(default)]
    schedule: RawSchedule,
    #[serde(default)]
    layout: RawLayout,
    #[serde(default)]
    google: RawGoogle,
    #[serde(default)]
    output: RawOutput,
    #[serde(default)]
    summarize: RawSummarize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSummarize {
    enabled: Option<bool>,
    url: Option<String>,
    model: Option<String>,
    examples_file: Option<String>,
    timeout_secs: Option<u64>,
    days: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCalendar {
    name: String,
    initial: Option<String>,
    person: Option<String>,
    shade_days: Option<bool>,
    ics_url: Option<String>,
    google_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrinter {
    name: Option<String>,
    copies: Option<u32>,
    options: Option<Vec<String>>,
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchedule {
    weekly: Option<bool>,
    weekly_day: Option<String>,
    weekly_time: Option<String>,
    monthly: Option<bool>,
    monthly_time: Option<String>,
    catch_up_hours: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayout {
    time_format: Option<String>,
    show_location: Option<bool>,
    weekly_details: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGoogle {
    credentials_file: Option<String>,
    token_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    archive_dir: Option<String>,
    state_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Source {
    Ics(String),
    Google(String),
}

#[derive(Debug, Clone)]
pub struct CalendarConfig {
    pub name: String,
    /// Prefix on the calendar grid, e.g. "C" in "C: Soccer". Empty = no prefix.
    pub initial: String,
    /// Name used in the weekly detail list, e.g. "Charlie". Empty = no prefix.
    pub person: String,
    /// Shade the whole day cell when this calendar has an all-day event (holidays, no-school days).
    pub shade_days: bool,
    pub source: Source,
}

#[derive(Debug, Clone)]
pub struct PrinterConfig {
    /// None = the system default destination.
    pub name: Option<String>,
    pub copies: u32,
    pub options: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    pub weekly: bool,
    pub weekly_day: Weekday,
    pub weekly_time: NaiveTime,
    pub monthly: bool,
    pub monthly_time: NaiveTime,
    pub catch_up_hours: i64,
}

#[derive(Debug, Clone)]
pub struct LayoutConfig {
    pub clock_24h: bool,
    pub show_location: bool,
    pub weekly_details: bool,
}

#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub credentials_file: PathBuf,
    pub token_file: PathBuf,
}

/// Optional LLM summaries for days too full for their grid cell.
#[derive(Debug, Clone)]
pub struct SummarizeConfig {
    pub enabled: bool,
    /// Ollama base URL.
    pub url: String,
    pub model: String,
    /// Few-shot examples (TOML); None uses the built-in generic examples.
    pub examples_file: Option<PathBuf>,
    pub timeout_secs: u64,
    /// Summarize only days that overflow their cell (otherwise every day with events).
    pub only_overfull: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub timezone: Tz,
    pub week_start: Weekday,
    pub qr_url: Option<String>,
    /// Initial -> full name, for prefixes typed directly into event titles ("B+C: ...").
    pub people: BTreeMap<String, String>,
    pub calendars: Vec<CalendarConfig>,
    pub printer: PrinterConfig,
    pub schedule: ScheduleConfig,
    pub layout: LayoutConfig,
    pub google: GoogleConfig,
    pub archive_dir: Option<PathBuf>,
    pub state_dir: PathBuf,
    pub summarize: SummarizeConfig,
}

fn weekday(v: &str, key: &str) -> Result<Weekday> {
    v.parse::<Weekday>().map_err(|_| anyhow::anyhow!("{key}: expected a weekday name, got {v:?}"))
}

fn time(v: &str, key: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(v, "%H:%M").with_context(|| format!("{key}: expected HH:MM, got {v:?}"))
}

pub fn local_timezone() -> Result<Tz> {
    let name = iana_time_zone::get_timezone().context("could not determine the system timezone")?;
    name.parse().map_err(|_| anyhow::anyhow!("unknown system timezone {name:?}"))
}

pub fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;
    if raw.calendars.is_empty() {
        bail!("at least one [[calendars]] entry is required");
    }
    let mut people = raw.people;
    let mut calendars = Vec::new();
    for c in raw.calendars {
        let source = match (c.ics_url, c.google_id) {
            (Some(u), None) => Source::Ics(u),
            (None, Some(g)) => Source::Google(g),
            _ => bail!("calendar {:?}: set exactly one of ics_url or google_id", c.name),
        };
        let initial = c.initial.unwrap_or_default().trim().to_string();
        let person = c.person.unwrap_or_else(|| if initial.is_empty() { String::new() } else { c.name.clone() });
        if !initial.is_empty() && !person.is_empty() {
            people.entry(initial.clone()).or_insert_with(|| person.clone());
        }
        calendars.push(CalendarConfig { name: c.name, initial, person, shade_days: c.shade_days.unwrap_or(false), source });
    }

    let timezone = match raw.timezone {
        Some(name) => name.parse().map_err(|_| anyhow::anyhow!("timezone: unknown zone {name:?}"))?,
        None => local_timezone()?,
    };

    let p = raw.printer;
    let printer = PrinterConfig {
        name: p.name.filter(|s| !s.is_empty()),
        copies: p.copies.unwrap_or(1).max(1),
        options: p.options.unwrap_or_else(|| vec!["media=Letter".into(), "sides=one-sided".into()]),
        enabled: p.enabled.unwrap_or(true),
    };

    let s = raw.schedule;
    let schedule = ScheduleConfig {
        weekly: s.weekly.unwrap_or(true),
        weekly_day: weekday(s.weekly_day.as_deref().unwrap_or("sunday"), "schedule.weekly_day")?,
        weekly_time: time(s.weekly_time.as_deref().unwrap_or("18:00"), "schedule.weekly_time")?,
        monthly: s.monthly.unwrap_or(true),
        monthly_time: time(s.monthly_time.as_deref().unwrap_or("18:00"), "schedule.monthly_time")?,
        catch_up_hours: s.catch_up_hours.unwrap_or(12).max(0),
    };

    let l = raw.layout;
    let clock_24h = match l.time_format.as_deref().unwrap_or("12h") {
        "12h" => false,
        "24h" => true,
        other => bail!("layout.time_format: expected \"12h\" or \"24h\", got {other:?}"),
    };
    let layout = LayoutConfig { clock_24h, show_location: l.show_location.unwrap_or(true), weekly_details: l.weekly_details.unwrap_or(true) };

    let g = raw.google;
    let google = GoogleConfig {
        credentials_file: expand(g.credentials_file.as_deref().unwrap_or("~/.config/calprint/credentials.json")),
        token_file: expand(g.token_file.as_deref().unwrap_or("~/.config/calprint/token.json")),
    };

    let sm = raw.summarize;
    let summarize = SummarizeConfig {
        enabled: sm.enabled.unwrap_or(false),
        url: sm.url.unwrap_or_else(|| "http://localhost:11434".into()).trim_end_matches('/').to_string(),
        model: sm.model.unwrap_or_else(|| "gemma4:26b".into()),
        examples_file: sm.examples_file.as_deref().map(expand),
        timeout_secs: sm.timeout_secs.unwrap_or(180),
        only_overfull: match sm.days.as_deref().unwrap_or("all") {
            "all" => false,
            "overfull" => true,
            other => bail!("summarize.days: expected \"all\" or \"overfull\", got {other:?}"),
        },
    };

    Ok(Config {
        timezone,
        week_start: weekday(raw.week_starts_on.as_deref().unwrap_or("monday"), "week_starts_on")?,
        qr_url: raw.qr_url.filter(|s| !s.is_empty()),
        people,
        calendars,
        printer,
        schedule,
        layout,
        google,
        archive_dir: raw.output.archive_dir.as_deref().map(expand),
        state_dir: expand(raw.output.state_dir.as_deref().unwrap_or("~/Library/Application Support/calprint")),
        summarize,
    })
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {} (copy config.example.toml there to start)", path.display()))?;
    parse(&text).with_context(|| format!("in config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg = parse(include_str!("../config.example.toml")).unwrap();
        assert!(!cfg.calendars.is_empty());
        assert_eq!(cfg.schedule.weekly_day, Weekday::Sun);
        assert_eq!(cfg.week_start, Weekday::Mon);
        assert_eq!(cfg.people.get("C").map(String::as_str), Some("Charlie"));
    }

    #[test]
    fn rejects_ambiguous_source() {
        let err = parse("timezone='UTC'\n[[calendars]]\nname='a'\nics_url='x'\ngoogle_id='y'\n").unwrap_err();
        assert!(err.to_string().contains("exactly one"));
    }
}
