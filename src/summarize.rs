//! Condense an overfull day into a few short lines for its grid cell, using a
//! local Ollama model. Few-shot examples pair real past days' events with the
//! cell text the family wrote by hand for them.
//!
//! Only days whose events don't fit their cell are sent to the model, results
//! are cached, and any failure falls back to the full event text, so printing
//! never depends on the model being available.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Config, SummarizeConfig};
use crate::model::{Event, events_on};
use crate::render;

/// One line of a summarized cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellLine {
    pub text: String,
    pub bold: bool,
}

/// Summaries for the days that needed one.
pub type Summaries = HashMap<NaiveDate, Vec<CellLine>>;

#[derive(Debug, Clone, Deserialize)]
pub struct Example {
    pub events: String,
    pub cell: String,
}

#[derive(Deserialize)]
struct ExamplesFile {
    #[serde(default)]
    example: Vec<Example>,
}

/// Generic examples used when no examples file is configured.
const BUILTIN_EXAMPLES: &str = include_str!("../examples.default.toml");

pub fn load_examples(path: Option<&Path>) -> Result<Vec<Example>> {
    let text = match path.filter(|p| p.exists()) {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => BUILTIN_EXAMPLES.to_string(),
    };
    let file: ExamplesFile = toml::from_str(&text).context("parsing few-shot examples")?;
    Ok(file.example)
}

/// The model's input for a day: one numbered line per event, e.g.
/// `#2 [repeats] Charlie After-school: Chess (3:30pm)`.
pub fn day_input(cfg: &Config, events: &[Event], day: NaiveDate) -> String {
    events_on(events, day)
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let kind = match (e.recurring, e.daily) {
                (false, _) => "one-off",
                (true, false) => "repeats",
                (true, true) => "repeats daily",
            };
            let when = match render::time_note(e, day, cfg.layout.clock_24h, render::TimeStyle::Full) {
                Some(t) => format!(" ({t})"),
                None if e.all_day => " (all day)".to_string(),
                None => String::new(),
            };
            format!("#{} [{kind}] {}{when}", i + 1, render::cell_label(cfg, e))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn system_prompt(cfg: &Config) -> String {
    let people: Vec<String> = cfg.people.iter().map(|(i, n)| format!("{i}={n}")).collect();
    let people = if people.is_empty() { String::new() } else { format!("\n- People's initials: {}.", people.join(", ")) };
    format!(
        "You condense one day of a family calendar into a few very short lines for one cell of a printed wall calendar.
The events are numbered #1, #2, ...
Rules:
- Reply with only the cell lines: no commentary, no bullets, no blank lines.
- Start every line with the number of the event it shows, like \"#2 C: Chess\". A line that merges events lists them all, like \"#1 #3 A+C+D: L.A. trip\".
- Do not write times; they are added to each line automatically from the events.
- Prefix each item with the initials of the people involved, like \"C: \" or \"A+C+D: \".
- Initials always go before the colon and the activity after it, even when the event title is written the other way around: for \"Art class: Charlie & Denise\" write \"C+D: Art class\", never \"Art class: C+D\". Never put an activity, group or place before the colon.
- Drop filler words, room numbers, and details a family member already knows; merge items that belong together.
- Prefer keeping one-off events over routine ones if space is short.
- If nothing is worth printing (only routine drop-offs, pick-ups or bus times), reply with exactly: {NOTHING}{people}"
    )
}

/// The model's reply for a day with nothing worth printing.
const NOTHING: &str = "(nothing)";

fn user_prompt(input: &str, max_lines: usize) -> String {
    format!("Fit this day in at most {max_lines} lines of about 15 characters each:\n{input}")
}

/// One reply line: the events it covers (1-based #numbers) and its short label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyLine {
    pub refs: Vec<usize>,
    pub label: String,
}

/// Remove times the model wrote anyway ("C: Chess 3:30pm" -> "C: Chess").
fn strip_times(label: &str) -> String {
    let mut words: Vec<&str> = label.split_whitespace().collect();
    while words.len() > 1 && words.last().is_some_and(|w| looks_like_time(w)) {
        words.pop();
    }
    while words.len() > 1 && words.last().is_some_and(|w| matches!(*w, "@" | "at" | "-" | "\u{2013}")) {
        words.pop();
    }
    words.join(" ")
}

fn looks_like_time(w: &str) -> bool {
    let w = w.trim_matches(|c: char| matches!(c, '(' | ')' | ','));
    let lower = w.to_ascii_lowercase();
    let core = ["am", "pm", "a", "p"].iter().find_map(|s| lower.strip_suffix(s)).map(str::to_string);
    let has_suffix = core.is_some();
    let core = core.unwrap_or(lower.clone());
    let digits_ok = !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || matches!(c, ':' | '-' | '\u{2013}')) && core.starts_with(|c: char| c.is_ascii_digit());
    digits_ok && (has_suffix || core.contains(':'))
}

/// Parse the model's reply. Some(empty) for "(nothing)"; None if any line lacks
/// a valid #event reference or the reply has no lines.
pub fn parse_reply(reply: &str, n_events: usize) -> Option<Vec<ReplyLine>> {
    if reply.trim().trim_matches('`').trim().eq_ignore_ascii_case(NOTHING) {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for line in reply.lines().map(str::trim) {
        let line = line.trim_start_matches(['-', '•']).trim().trim_matches('*').trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let mut refs = Vec::new();
        let mut rest = line;
        loop {
            rest = rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '+' | '&'));
            let Some(after) = rest.strip_prefix('#') else { break };
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            let Ok(n) = digits.parse::<usize>() else { break };
            if (1..=n_events).contains(&n) && !refs.contains(&n) {
                refs.push(n);
            }
            rest = &after[digits.len()..];
        }
        let label = strip_times(rest.trim_start_matches([':', '-', ')', '.']).trim().trim_matches('*').trim());
        if refs.is_empty() || label.is_empty() {
            return None;
        }
        out.push(ReplyLine { refs, label });
    }
    (!out.is_empty()).then_some(out)
}

/// Turn reply lines into cell lines, adding each line's time from its events:
/// bold if any covered event is one-off; the earliest timed event's time, unless
/// they are all all-day or daily.
pub fn assemble(cfg: &Config, items: &[&Event], day: NaiveDate, lines: &[ReplyLine]) -> Vec<CellLine> {
    lines
        .iter()
        .map(|l| {
            let evs: Vec<&Event> = l.refs.iter().map(|&r| items[r - 1]).collect();
            let bold = evs.iter().any(|e| !e.recurring);
            let time = evs.iter().filter(|e| render::grid_time(cfg, e, day).is_some()).min_by_key(|e| e.start).and_then(|e| render::grid_time(cfg, e, day));
            let label = initials_first(cfg, &l.label);
            let text = match time {
                Some(t) => format!("{label} {t}"),
                None => label,
            };
            CellLine { text, bold }
        })
        .collect()
}

/// The initial for a known initial or person's name ("C", "Charlie" -> "C"), from
/// the calendars and `[people]`.
fn initial_for(cfg: &Config, word: &str) -> Option<String> {
    let known = cfg.calendars.iter().filter(|c| !c.initial.is_empty()).map(|c| (c.initial.as_str(), c.person.as_str())).chain(cfg.people.iter().map(|(i, n)| (i.as_str(), n.as_str())));
    let mut hits: Vec<&str> = known.filter(|(i, n)| *i == word || (!n.is_empty() && n.eq_ignore_ascii_case(word))).map(|(i, _)| i).collect();
    hits.sort_by_key(|i| i.len());
    hits.first().map(|i| i.to_string())
}

/// Initials joined by "+", if every part of `s` ("C+D", "Charlie & Denise") is a known person.
fn as_initials(cfg: &Config, s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split(['+', '&', ',', '/']).flat_map(|p| p.split(" and ")).map(str::trim).filter(|p| !p.is_empty()).collect();
    let initials: Vec<String> = parts.iter().map(|p| initial_for(cfg, p)).collect::<Option<_>>()?;
    (!initials.is_empty()).then(|| initials.join("+"))
}

/// Put the people before the colon: "Art class: C+D" -> "C+D: Art class".
fn initials_first(cfg: &Config, label: &str) -> String {
    match label.split_once(": ") {
        Some((head, tail)) if as_initials(cfg, head).is_none() => match as_initials(cfg, tail) {
            Some(initials) => format!("{initials}: {}", head.trim()),
            None => label.to_string(),
        },
        _ => label.to_string(),
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// The model's parsed reply; times and bold are applied when rendering.
    entries: BTreeMap<String, Vec<ReplyLine>>,
}

pub struct Summarizer<'a> {
    cfg: &'a Config,
    sc: &'a SummarizeConfig,
    examples: Vec<Example>,
    model: String,
    cache_path: PathBuf,
    cache: Cache,
}

impl<'a> Summarizer<'a> {
    pub fn new(cfg: &'a Config) -> Result<Summarizer<'a>> {
        let sc = &cfg.summarize;
        let examples = load_examples(sc.examples_file.as_deref())?;
        let model = resolve_model(sc)?;
        let cache_path = cfg.state_dir.join("summaries-v2.json");
        let cache = std::fs::read_to_string(&cache_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Ok(Summarizer { cfg, sc, examples, model, cache_path, cache })
    }

    fn messages(&self, input: &str, max_lines: usize) -> Vec<Value> {
        let mut m = vec![json!({"role": "system", "content": system_prompt(self.cfg)})];
        for ex in &self.examples {
            m.push(json!({"role": "user", "content": user_prompt(ex.events.trim(), max_lines.max(ex.cell.trim().lines().count()))}));
            m.push(json!({"role": "assistant", "content": ex.cell.trim()}));
        }
        m.push(json!({"role": "user", "content": user_prompt(input, max_lines)}));
        m
    }

    fn chat(&self, messages: &[Value]) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
            "think": false,
            "options": {"temperature": 0.2},
        });
        let resp: Value = ureq::post(&format!("{}/api/chat", self.sc.url))
            .timeout(StdDuration::from_secs(self.sc.timeout_secs))
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => anyhow!("Ollama HTTP {code}: {}", r.into_string().unwrap_or_default()),
                other => anyhow!("Ollama request failed: {other}"),
            })?
            .into_json()?;
        resp["message"]["content"].as_str().map(String::from).ok_or_else(|| anyhow!("unexpected Ollama reply: {resp}"))
    }

    /// Summarize one day into at most `max_lines` wrapped cell lines.
    pub fn summarize(&mut self, events: &[Event], day: NaiveDate, max_lines: usize, fits: impl Fn(&[CellLine]) -> bool) -> Result<Vec<CellLine>> {
        let items = events_on(events, day);
        let mut messages = self.messages(&day_input(self.cfg, events, day), max_lines);
        let key = cache_key(&self.model, &messages);
        if let Some(hit) = self.cache.entries.get(&key) {
            return Ok(assemble(self.cfg, &items, day, hit));
        }
        let mut lines: Option<Vec<ReplyLine>> = None;
        for attempt in 0..3 {
            let reply = self.chat(&messages)?;
            let parsed = parse_reply(&reply, items.len());
            let feedback = match &parsed {
                None => format!("Every line must start with the # numbers of the events it shows, like \"#2 C: Chess\", or reply {NOTHING}."),
                Some(p) if !fits(&assemble(self.cfg, &items, day, p)) => {
                    format!("Too long for the cell. Shorten to at most {} short lines.", max_lines.saturating_sub(attempt + 1).max(1))
                }
                Some(p) => {
                    lines = Some(p.clone());
                    break;
                }
            };
            if let Some(p) = parsed {
                // Too long: keep the best effort in case retries don't help.
                lines = Some(p);
            }
            messages.push(json!({"role": "assistant", "content": reply}));
            messages.push(json!({"role": "user", "content": feedback}));
        }
        let lines = lines.ok_or_else(|| anyhow!("model reply did not reference the events by number"))?;
        let cells = assemble(self.cfg, &items, day, &lines);
        self.cache.entries.insert(key, lines);
        Ok(cells)
    }

    pub fn save_cache(&mut self) {
        // Keep the cache from growing without bound.
        while self.cache.entries.len() > 2000 {
            let first = self.cache.entries.keys().next().cloned().unwrap();
            self.cache.entries.remove(&first);
        }
        if let Some(dir) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec(&self.cache) {
            let _ = std::fs::write(&self.cache_path, bytes);
        }
    }
}

fn cache_key(model: &str, messages: &[Value]) -> String {
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update(serde_json::to_vec(messages).unwrap_or_default());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Use the configured model, or a same-named variant (e.g. "gemma4:26b" -> "gemma4:26b-mlx").
fn resolve_model(sc: &SummarizeConfig) -> Result<String> {
    let tags: Value = ureq::get(&format!("{}/api/tags", sc.url))
        .timeout(StdDuration::from_secs(10))
        .call()
        .map_err(|e| anyhow!("cannot reach Ollama at {}: {e}", sc.url))?
        .into_json()?;
    let names: Vec<&str> = tags["models"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str()).collect();
    let want = sc.model.as_str();
    let found = names
        .iter()
        .find(|n| **n == want || n.strip_suffix(":latest") == Some(want))
        .or_else(|| names.iter().find(|n| n.starts_with(&format!("{want}-"))));
    found.map(|s| s.to_string()).ok_or_else(|| anyhow!("model {want:?} is not installed in Ollama (have: {})", names.join(", ")))
}

/// Summaries for the days in `days` that have events (or only those overflowing
/// `max_lines`, with `days = "overfull"`). Failures are logged and leave that
/// day's full text in place.
pub fn summarize_days(cfg: &Config, events: &[Event], days: impl Iterator<Item = NaiveDate>, max_lines: usize) -> Summaries {
    let mut out = Summaries::new();
    let wanted = |d: &NaiveDate| {
        let n = render::day_line_count(cfg, events, *d);
        n > 0 && (!cfg.summarize.only_overfull || n > max_lines)
    };
    let overfull: Vec<NaiveDate> = days.filter(wanted).collect();
    if overfull.is_empty() {
        return out;
    }
    let mut s = match Summarizer::new(cfg) {
        Ok(s) => s,
        Err(e) => {
            crate::log(&format!("summaries skipped: {e:#}"));
            return out;
        }
    };
    for day in overfull {
        match s.summarize(events, day, max_lines, |lines| render::summary_line_count(lines) <= max_lines) {
            Ok(lines) => {
                out.insert(day, lines);
            }
            Err(e) => crate::log(&format!("summary for {day} failed, printing in full: {e:#}")),
        }
    }
    s.save_cache();
    crate::log(&format!("summarized {} day(s) with {}", out.len(), s.model));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies() {
        assert_eq!(parse_reply(" (nothing)\n", 3), Some(vec![]));
        assert_eq!(parse_reply("\n```\n```", 3), None);
        // A line without an event number is unusable.
        assert_eq!(parse_reply("#1 C: Chess\nD: Swim", 3), None);
        let lines = parse_reply("```\n- **#3 D: PTC 10:05am**\n#2: C: Chess (3:30pm)\n#1, #4 A+C+D: L.A. trip\n#9 Bogus\n```", 4);
        assert_eq!(lines, None, "#9 is out of range, so that line has no valid reference");
        let lines = parse_reply("**#3 D: PTC 10:05am**\n#2: C: Chess (3:30pm)\n#1, #4 A+C+D: L.A. trip\n#2 D: Swim 7:15p", 4).unwrap();
        assert_eq!(
            lines,
            vec![
                ReplyLine { refs: vec![3], label: "D: PTC".into() },
                ReplyLine { refs: vec![2], label: "C: Chess".into() },
                ReplyLine { refs: vec![1, 4], label: "A+C+D: L.A. trip".into() },
                ReplyLine { refs: vec![2], label: "D: Swim".into() },
            ]
        );
    }

    #[test]
    fn times_come_from_events() {
        use chrono::TimeZone;
        let cfg = crate::config::parse(include_str!("../config.example.toml")).unwrap();
        let tz = cfg.timezone;
        let day = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let at = |h, m| tz.with_ymd_and_hms(2026, 11, 5, h, m, 0).unwrap();
        let mut chess = crate::model::test_event(at(15, 30), at(16, 30), false);
        chess.recurring = true;
        let mut camp = crate::model::test_event(at(9, 0), at(15, 0), false);
        camp.recurring = true;
        camp.daily = true;
        let ptc = crate::model::test_event(at(10, 5), at(10, 30), false);
        let holiday = crate::model::test_event(at(0, 0), at(0, 0) + chrono::Duration::days(1), true);
        let items = vec![&chess, &camp, &ptc, &holiday];
        let reply = parse_reply("#1 C: Chess\n#2 C: Camp\n#3 D: PTC\n#4 Holiday", 4).unwrap();
        let cells = assemble(&cfg, &items, day, &reply);
        let text: Vec<(&str, bool)> = cells.iter().map(|c| (c.text.as_str(), c.bold)).collect();
        assert_eq!(text, vec![("C: Chess 3:30p", false), ("C: Camp", false), ("D: PTC 10:05-10:30a", true), ("Holiday", true)]);
    }

    #[test]
    fn initials_go_before_the_colon() {
        let cfg = crate::config::parse(include_str!("../config.example.toml")).unwrap();
        assert_eq!(initials_first(&cfg, "Art class: C+D"), "C+D: Art class");
        assert_eq!(initials_first(&cfg, "Art class: Denise & Charlie"), "D+C: Art class");
        assert_eq!(initials_first(&cfg, "C+D: Art class"), "C+D: Art class");
        assert_eq!(initials_first(&cfg, "Book club: Moby Dick"), "Book club: Moby Dick");
        assert_eq!(initials_first(&cfg, "Halloween"), "Halloween");
    }

    #[test]
    fn builtin_examples_parse() {
        let ex = load_examples(None).unwrap();
        assert!(ex.len() >= 2);
        assert!(ex.iter().all(|e| !e.events.trim().is_empty() && !e.cell.trim().is_empty()));
    }
}
