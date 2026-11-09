//! Control annotations: hashtags in an event's title or notes that tell calprint
//! how to treat the event. The "#" is required; words without it are never
//! annotations. Supported so far:
//!
//! - **don't print** (`#dontprint`, `#donotprint`, `#do_not_print`, `#DontPrint`,
//!   `#don't print`, typos like `#dontprnit`, ...): the event is left off every page.
//!
//! Exact spellings (ignoring case, spaces and punctuation) are recognized
//! directly. Any other hashtag is classified by the local Ollama model set up in
//! `[summarize]`, so variations and typos count while ordinary hashtags
//! (`#soccer`, `#reprint`) don't; its answers are cached. Without the model
//! (summaries disabled, or Ollama unreachable) a one-typo spelling check stands in.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;
use crate::model::Event;
use crate::summarize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Annotation {
    /// Leave the event off the printed pages.
    DontPrint,
}

/// Exact "don't print" spellings, lowercase with only letters and digits kept.
const DONT_PRINT: &[&str] = &["dontprint", "donotprint", "noprint"];

/// Words after the "#" considered part of a hashtag ("#do not print").
const MAX_WORDS: usize = 3;

/// A hashtag and the words right after it, e.g. ["do", "not", "print"] for "#do not print".
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    words: Vec<String>,
}

impl Candidate {
    /// What the model sees, e.g. "#do not print".
    pub fn phrase(&self) -> String {
        format!("#{}", self.words.join(" "))
    }

    /// The tag alone and with each following word appended, normalized:
    /// "#do not print" -> ["do", "donot", "donotprint"].
    fn spellings(&self) -> Vec<String> {
        (1..=self.words.len()).map(|k| normalize(&self.words[..k].concat())).collect()
    }

    fn exact(&self) -> Option<Annotation> {
        self.spellings().iter().any(|s| DONT_PRINT.contains(&s.as_str())).then_some(Annotation::DontPrint)
    }

    /// Offline stand-in for the model: within one typo of a known spelling.
    fn close(&self) -> Option<Annotation> {
        self.spellings().iter().any(|s| DONT_PRINT.iter().any(|f| osa_distance(s, f) <= 1)).then_some(Annotation::DontPrint)
    }
}

fn normalize(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Leading characters of `s` that can belong to a hashtag word (stops at
/// markup such as "</b>" or "&nbsp;" in Google Calendar notes).
fn word_part(s: &str) -> &str {
    let end = s.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '\'' | '\u{2019}' | '.'))).unwrap_or(s.len());
    s[..end].trim_end_matches(['.', '-', '\''])
}

/// Every hashtag in `text`: a "#" directly followed by a letter.
pub fn candidates(text: &str) -> Vec<Candidate> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let mut rest = *w;
        while let Some(pos) = rest.find('#') {
            rest = &rest[pos + 1..];
            if !rest.starts_with(char::is_alphabetic) {
                continue;
            }
            let first = word_part(rest);
            let mut tag = vec![first.to_string()];
            // Following words join the phrase only if the tag word ended cleanly.
            if first.len() == rest.len() {
                for next in words.iter().skip(i + 1).take(MAX_WORDS - 1) {
                    let part = word_part(next);
                    if part.is_empty() || next.contains('#') {
                        break;
                    }
                    tag.push(part.to_string());
                    if part.len() != next.len() {
                        break;
                    }
                }
            }
            out.push(Candidate { words: tag });
        }
    }
    out
}

/// Optimal-string-alignment distance (edits plus adjacent transpositions).
fn osa_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

const SYSTEM_PROMPT: &str = "People write hashtags in a family calendar's event titles and notes. Some hashtags are control annotations telling the program that prints the calendar what to do with the event. Decide which annotation, if any, the writer of each hashtag meant.
Annotations:
- dont_print: the writer wants this event left off the printed calendar. Count any spelling, spacing, punctuation, capitalization, abbreviation or typo of \"don't print\" or \"do not print\", e.g. #dontprint, #DoNotPrint, #do_not_print, #dont-print, #don't print, #noprint, #dontprnit, #donotprnt, #dnotprint.
- none: anything else, including ordinary hashtags about the event itself (#soccer, #birthday, #reprint, #printshop, #blueprint) and words that merely follow the hashtag.
Each hashtag is shown with up to two words that follow it, which may or may not be part of it.
Reply with only a JSON object mapping each hashtag's number to \"dont_print\" or \"none\".";

fn user_prompt(phrases: &[String]) -> String {
    phrases.iter().enumerate().map(|(i, p)| format!("{}. {p}", i + 1)).collect::<Vec<_>>().join("\n")
}

/// Parse the model's JSON reply: the annotation for each phrase it answered, by index.
pub fn parse_reply(reply: &str, n: usize) -> Result<HashMap<usize, Option<Annotation>>> {
    let v: Value = serde_json::from_str(reply.trim().trim_matches('`').trim_start_matches("json")).map_err(|e| anyhow!("annotation reply is not JSON: {e}"))?;
    let obj = v.as_object().ok_or_else(|| anyhow!("annotation reply is not a JSON object"))?;
    let mut out = HashMap::new();
    for (k, v) in obj {
        let Ok(i) = k.trim().trim_start_matches('#').trim_end_matches('.').parse::<usize>() else { continue };
        if !(1..=n).contains(&i) {
            continue;
        }
        let answer = v.as_str().map(normalize).unwrap_or_default();
        let ann = if answer == "dontprint" || answer == "donotprint" { Some(Annotation::DontPrint) } else { None };
        out.insert(i - 1, ann);
    }
    Ok(out)
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// "model\nphrase" -> the model's answer.
    entries: BTreeMap<String, Option<Annotation>>,
}

struct Classifier<'a> {
    cfg: &'a Config,
    model: String,
    cache_path: PathBuf,
    cache: Cache,
}

impl<'a> Classifier<'a> {
    fn new(cfg: &'a Config) -> Result<Classifier<'a>> {
        let model = summarize::resolve_model(&cfg.summarize)?;
        let cache_path = cfg.state_dir.join("annotations-v1.json");
        let cache = std::fs::read_to_string(&cache_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Ok(Classifier { cfg, model, cache_path, cache })
    }

    fn key(&self, phrase: &str) -> String {
        format!("{}\n{}", self.model, phrase.to_lowercase())
    }

    /// The model's answers for `phrases`; phrases it couldn't answer are missing.
    fn classify(&mut self, phrases: &[String]) -> HashMap<String, Option<Annotation>> {
        let mut out = HashMap::new();
        let mut todo = Vec::new();
        for p in phrases {
            match self.cache.entries.get(&self.key(p)) {
                Some(a) => {
                    out.insert(p.clone(), *a);
                }
                None => todo.push(p.clone()),
            }
        }
        for chunk in todo.chunks(25) {
            let messages = [
                json!({"role": "system", "content": SYSTEM_PROMPT}),
                json!({"role": "user", "content": user_prompt(&["#DontPrnt".into(), "#soccer practice at".into(), "#do not print".into()])}),
                json!({"role": "assistant", "content": r#"{"1": "dont_print", "2": "none", "3": "dont_print"}"#}),
                json!({"role": "user", "content": user_prompt(chunk)}),
            ];
            let answers = summarize::chat(&self.cfg.summarize, &self.model, &messages, 0.0, true).and_then(|r| parse_reply(&r, chunk.len()));
            match answers {
                Ok(answers) => {
                    for (i, a) in answers {
                        self.cache.entries.insert(self.key(&chunk[i]), a);
                        out.insert(chunk[i].clone(), a);
                    }
                }
                Err(e) => crate::log(&format!("annotation check with {} failed, using spelling check: {e:#}", self.model)),
            }
        }
        out
    }

    fn save_cache(&mut self) {
        while self.cache.entries.len() > 5000 {
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

fn event_candidates(e: &Event) -> Vec<Candidate> {
    let mut c = candidates(&e.title);
    if let Some(d) = &e.description {
        c.extend(candidates(d));
    }
    c
}

/// Decide each candidate: exact spellings directly, then the model, then the
/// spelling check for anything the model didn't answer.
fn resolve(cfg: &Config, all: &[Candidate]) -> HashMap<String, Option<Annotation>> {
    let mut decided: HashMap<String, Option<Annotation>> = HashMap::new();
    let mut unknown: Vec<String> = Vec::new();
    for c in all {
        let phrase = c.phrase();
        if decided.contains_key(&phrase) || unknown.contains(&phrase) {
            continue;
        }
        match c.exact() {
            Some(a) => {
                decided.insert(phrase, Some(a));
            }
            None => unknown.push(phrase),
        }
    }
    if !unknown.is_empty() && cfg.summarize.enabled {
        match Classifier::new(cfg) {
            Ok(mut cl) => {
                decided.extend(cl.classify(&unknown));
                cl.save_cache();
            }
            Err(e) => crate::log(&format!("annotation check using spelling only: {e:#}")),
        }
    }
    for c in all {
        decided.entry(c.phrase()).or_insert_with(|| c.close());
    }
    decided
}

/// Remove events carrying a "don't print" annotation.
pub fn drop_unprintable(cfg: &Config, events: Vec<Event>) -> Vec<Event> {
    let per_event: Vec<Vec<Candidate>> = events.iter().map(event_candidates).collect();
    let all: Vec<Candidate> = per_event.iter().flatten().cloned().collect();
    if all.is_empty() {
        return events;
    }
    let decided = resolve(cfg, &all);
    let before = events.len();
    let kept: Vec<Event> = events
        .into_iter()
        .zip(per_event)
        .filter(|(_, cands)| !cands.iter().any(|c| decided.get(&c.phrase()).copied().flatten() == Some(Annotation::DontPrint)))
        .map(|(e, _)| e)
        .collect();
    if kept.len() < before {
        crate::log(&format!("left off {} event(s) marked don't print", before - kept.len()));
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(text: &str) -> bool {
        candidates(text).iter().any(|c| c.exact().is_some())
    }

    fn close(text: &str) -> bool {
        candidates(text).iter().any(|c| c.close().is_some())
    }

    #[test]
    fn finds_hashtags() {
        let phrases = |t: &str| candidates(t).iter().map(Candidate::phrase).collect::<Vec<_>>();
        assert_eq!(phrases("Dentist #dontprint"), vec!["#dontprint"]);
        assert_eq!(phrases("Dentist #do not print, ok"), vec!["#do not print"]);
        assert_eq!(phrases("<p><b>#DoNotPrint</b>&nbsp;thanks</p>"), vec!["#DoNotPrint"]);
        assert_eq!(phrases("Room #3 then #soccer #fun"), vec!["#soccer", "#fun"]);
        assert!(phrases("dontprint, no hash").is_empty());
        assert!(phrases("# dontprint").is_empty());
    }

    #[test]
    fn exact_spellings() {
        for t in ["x #dontprint", "#donotprint", "#do_not_print", "#DONT-PRINT", "#don't print", "#Don\u{2019}t Print", "#do not print", "#noprint", "notes: (#dontprint)"] {
            assert!(exact(t), "{t}");
        }
        for t in ["dontprint", "#print", "#dontprnit", "#soccer", "#reprint"] {
            assert!(!exact(t), "{t}");
        }
    }

    #[test]
    fn spelling_fallback() {
        for t in ["#dontprnit", "#donotprnt", "#dnotprint", "#dontprint", "#dont prnt"] {
            assert!(close(t), "{t}");
        }
        for t in ["#reprint", "#imprint", "#blueprint", "#print", "#dont", "dontprnit"] {
            assert!(!close(t), "{t}");
        }
    }

    #[test]
    fn parses_replies() {
        let r = parse_reply("```json\n{\"1\": \"dont_print\", \"2\": \"none\", \"#3\": \"DONTPRINT\", \"9\": \"dont_print\"}\n```", 3).unwrap();
        assert_eq!(r.get(&0), Some(&Some(Annotation::DontPrint)));
        assert_eq!(r.get(&1), Some(&None));
        assert_eq!(r.get(&2), Some(&Some(Annotation::DontPrint)));
        assert_eq!(r.len(), 3);
        assert!(parse_reply("nope", 1).is_err());
    }

    #[test]
    fn drops_marked_events_offline() {
        let mut cfg = crate::config::parse(include_str!("../config.example.toml")).unwrap();
        cfg.summarize.enabled = false;
        let tz = cfg.timezone;
        use chrono::TimeZone;
        let at = tz.with_ymd_and_hms(2026, 11, 5, 9, 0, 0).unwrap();
        let ev = |title: &str, notes: Option<&str>| {
            let mut e = crate::model::test_event(at, at + chrono::Duration::hours(1), false);
            e.title = title.into();
            e.description = notes.map(String::from);
            e
        };
        let events = vec![ev("Soccer #fun", None), ev("Surprise party #dontprint", None), ev("Gift shopping", Some("#do_not_print")), ev("Doctor #dontprnit", None)];
        let kept: Vec<String> = drop_unprintable(&cfg, events).into_iter().map(|e| e.title).collect();
        assert_eq!(kept, vec!["Soccer #fun"]);
    }

    /// Needs Ollama with the example config's model: `cargo test -- --ignored live_model`.
    #[test]
    #[ignore]
    fn live_model() {
        let mut cfg = crate::config::parse(include_str!("../config.example.toml")).unwrap();
        cfg.summarize.enabled = true;
        cfg.state_dir = std::env::temp_dir().join(format!("calprint-annot-{}", std::process::id()));
        let yes = ["#dontprnit", "#DoNotPirnt", "#do-nt-print", "#dont_prt", "#pls dont print", "#hide-dont print"];
        let no = ["#soccer practice", "#reprint flyers", "#printshop run", "#blueprint review", "#birthday party", "#print tickets"];
        let all: Vec<Candidate> = yes.iter().chain(&no).flat_map(|t| candidates(t)).collect();
        let decided = resolve(&cfg, &all);
        let _ = std::fs::remove_dir_all(&cfg.state_dir);
        for t in yes {
            assert_eq!(decided[&candidates(t)[0].phrase()], Some(Annotation::DontPrint), "{t}");
        }
        for t in no {
            assert_eq!(decided[&candidates(t)[0].phrase()], None, "{t}");
        }
    }
}
