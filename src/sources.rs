//! Collect events from every configured calendar.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone};
use chrono_tz::Tz;

use crate::config::{Config, Source};
use crate::google;
use crate::ics;
use crate::model::{Event, merge_duplicates};

fn fetch_ics(url: &str) -> Result<String> {
    if let Some(path) = url.strip_prefix("file://").or_else(|| url.starts_with('/').then_some(url)) {
        return std::fs::read_to_string(path).with_context(|| format!("reading {path}"));
    }
    let url = match url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };
    let resp = ureq::get(&url)
        .set("User-Agent", concat!("calprint/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .call()
        .map_err(|e| match e {
            // Never echo the secret URL into logs.
            ureq::Error::Status(code, _) => anyhow::anyhow!("iCal feed returned HTTP {code}"),
            other => anyhow::anyhow!("iCal feed request failed: {}", other.kind()),
        })?;
    Ok(resp.into_string()?)
}

pub fn midnight(tz: Tz, day: NaiveDate) -> DateTime<Tz> {
    tz.from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap_or_else(|| tz.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap()))
}

/// All events touching the inclusive date range [first, last].
pub fn fetch_events(cfg: &Config, first: NaiveDate, last: NaiveDate) -> Result<Vec<Event>> {
    let tz = cfg.timezone;
    let (from, to) = (midnight(tz, first), midnight(tz, last + Duration::days(1)));
    let mut google_client = None;
    let mut events = Vec::new();
    for (i, cal) in cfg.calendars.iter().enumerate() {
        let found = match &cal.source {
            Source::Ics(url) => {
                let text = fetch_ics(url).with_context(|| format!("calendar {:?}", cal.name))?;
                ics::parse_feed(&text, i, &cal.name, from, to, tz)
            }
            Source::Google(id) => {
                if google_client.is_none() {
                    google_client = Some(google::Client::connect(&cfg.google)?);
                }
                google_client.as_ref().unwrap().events(i, id, from, to, tz).with_context(|| format!("calendar {:?}", cal.name))?
            }
        };
        crate::log(&format!("calendar {:?}: {} events", cal.name, found.len()));
        events.extend(found);
    }
    Ok(merge_duplicates(events))
}
