//! Google Calendar API (read-only) with an installed-app OAuth loopback flow.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::GoogleConfig;
use crate::model::{Event, is_daily_rule};

const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
const AUTH_URI: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/calendar/v3";

#[derive(Deserialize)]
struct ClientFile {
    installed: Option<ClientSecret>,
    web: Option<ClientSecret>,
}

#[derive(Deserialize, Clone)]
struct ClientSecret {
    client_id: String,
    client_secret: String,
}

#[derive(Serialize, Deserialize)]
struct Token {
    refresh_token: String,
    access_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

fn read_client(path: &Path) -> Result<ClientSecret> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!("reading OAuth client file {} (download a \"Desktop app\" client from Google Cloud Console)", path.display())
    })?;
    let f: ClientFile = serde_json::from_str(&text).context("parsing OAuth client file")?;
    f.installed.or(f.web).ok_or_else(|| anyhow!("OAuth client file has no \"installed\" section"))
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("system RNG");
    URL_SAFE_NO_PAD.encode(buf)
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn api_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow!("HTTP {code}: {}", body.chars().take(500).collect::<String>())
        }
        other => anyhow!(other),
    }
}

fn save_token(path: &Path, token: &Token) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(token)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Interactive one-time sign-in: opens a browser, waits for the redirect, stores a refresh token.
pub fn authorize(cfg: &GoogleConfig) -> Result<()> {
    let client = read_client(&cfg.credentials_file)?;
    let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback port")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token();
    let url = format!(
        "{AUTH_URI}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}&access_type=offline&prompt=consent",
        pct(&client.client_id),
        pct(&redirect),
        pct(SCOPE),
    );
    println!("Opening your browser to sign in to Google. If it does not open, visit:\n\n{url}\n");
    let _ = std::process::Command::new("open").arg(&url).status();

    let code = loop {
        let (mut stream, _) = listener.accept()?;
        let mut request_line = String::new();
        BufReader::new(&stream).read_line(&mut request_line)?;
        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params: Vec<(String, String)> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), pct_decode(v)))
            .collect();
        let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        if get("state").is_none() && get("code").is_none() && get("error").is_none() {
            // Favicon or similar; ignore.
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            continue;
        }
        let body = if get("code").is_some() && get("state").as_deref() == Some(state.as_str()) {
            "calprint is signed in. You can close this tab."
        } else {
            "calprint sign-in failed. Return to the terminal for details."
        };
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        if let Some(err) = get("error") {
            bail!("Google sign-in was not completed: {err}");
        }
        if get("state").as_deref() != Some(state.as_str()) {
            bail!("OAuth state mismatch; try again");
        }
        break get("code").context("no authorization code in redirect")?;
    };

    let resp: Value = ureq::post(TOKEN_URI)
        .send_form(&[
            ("code", code.as_str()),
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ])
        .map_err(api_error)?
        .into_json()?;
    let token = token_from(&resp, None)?;
    save_token(&cfg.token_file, &token)?;
    println!("Saved Google token to {}", cfg.token_file.display());
    Ok(())
}

fn token_from(resp: &Value, previous_refresh: Option<&str>) -> Result<Token> {
    let refresh = resp["refresh_token"]
        .as_str()
        .or(previous_refresh)
        .ok_or_else(|| anyhow!("Google did not return a refresh token: {resp}"))?;
    Ok(Token {
        refresh_token: refresh.to_string(),
        access_token: resp["access_token"].as_str().map(String::from),
        expires_at: resp["expires_in"].as_i64().map(|s| Utc::now() + chrono::Duration::seconds(s)),
    })
}

/// A client holding a fresh access token.
pub struct Client {
    access_token: String,
}

impl Client {
    pub fn connect(cfg: &GoogleConfig) -> Result<Client> {
        let text = std::fs::read_to_string(&cfg.token_file)
            .with_context(|| format!("no Google token at {}; run `calprint auth` once on this Mac", cfg.token_file.display()))?;
        let token: Token = serde_json::from_str(&text).context("parsing Google token file")?;
        if let (Some(at), Some(exp)) = (&token.access_token, token.expires_at)
            && exp > Utc::now() + chrono::Duration::seconds(120) {
                return Ok(Client { access_token: at.clone() });
            }
        let client = read_client(&cfg.credentials_file)?;
        let resp: Value = ureq::post(TOKEN_URI)
            .send_form(&[
                ("client_id", client.client_id.as_str()),
                ("client_secret", client.client_secret.as_str()),
                ("refresh_token", token.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .map_err(|e| api_error(e).context("refreshing Google token (if revoked, run `calprint auth` again)"))?
            .into_json()?;
        let fresh = token_from(&resp, Some(&token.refresh_token))?;
        save_token(&cfg.token_file, &fresh)?;
        Ok(Client { access_token: fresh.access_token.context("token response had no access_token")? })
    }

    fn get_all(&self, url: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut req = ureq::get(url).set("Authorization", &format!("Bearer {}", self.access_token));
            for (k, v) in query {
                req = req.query(k, v);
            }
            if let Some(p) = &page {
                req = req.query("pageToken", p);
            }
            let resp: Value = req.call().map_err(api_error)?.into_json()?;
            if let Some(arr) = resp["items"].as_array() {
                items.extend(arr.iter().cloned());
            }
            match resp["nextPageToken"].as_str() {
                Some(t) => page = Some(t.to_string()),
                None => return Ok(items),
            }
        }
    }

    pub fn calendars(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .get_all(&format!("{API}/users/me/calendarList"), &[])?
            .into_iter()
            .map(|c| {
                let name = c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or_default().to_string();
                (c["id"].as_str().unwrap_or_default().to_string(), name)
            })
            .collect())
    }

    pub fn events(&self, cal_index: usize, id: &str, from: DateTime<Tz>, to: DateTime<Tz>, display: Tz) -> Result<Vec<Event>> {
        let items = self.get_all(
            &format!("{API}/calendars/{}/events", pct(id)),
            &[
                ("timeMin", from.to_rfc3339()),
                ("timeMax", to.to_rfc3339()),
                ("singleEvents", "true".into()),
                ("orderBy", "startTime".into()),
                ("timeZone", display.name().into()),
                ("maxResults", "2500".into()),
            ],
        )?;
        // Whether each recurring series repeats daily, looked up once per series.
        let mut daily: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        for rid in items.iter().filter_map(|i| i["recurringEventId"].as_str()) {
            if !daily.contains_key(rid) {
                let master: Value = ureq::get(&format!("{API}/calendars/{}/events/{}", pct(id), pct(rid)))
                    .set("Authorization", &format!("Bearer {}", self.access_token))
                    .call()
                    .map_err(api_error)?
                    .into_json()?;
                let is_daily = master["recurrence"].as_array().into_iter().flatten().filter_map(Value::as_str).any(is_daily_rule);
                daily.insert(rid.to_string(), is_daily);
            }
        }
        let mut out = Vec::new();
        for item in items {
            if item["status"] == "cancelled" {
                continue;
            }
            // Skip invitations this account declined.
            let declined = item["attendees"]
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p["self"] == true && p["responseStatus"] == "declined"));
            if declined {
                continue;
            }
            let (Some((start, all_day)), Some((end, _))) = (when(&item["start"], display), when(&item["end"], display)) else {
                continue;
            };
            out.push(Event {
                title: item["summary"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(No title)").to_string(),
                start,
                end,
                all_day,
                recurring: item["recurringEventId"].is_string(),
                daily: item["recurringEventId"].as_str().and_then(|r| daily.get(r)).copied().unwrap_or(false),
                calendars: vec![cal_index],
                location: item["location"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
            });
        }
        Ok(out)
    }
}

fn when(v: &Value, display: Tz) -> Option<(DateTime<Tz>, bool)> {
    if let Some(s) = v["dateTime"].as_str() {
        return DateTime::parse_from_rfc3339(s).ok().map(|d| (d.with_timezone(&display), false));
    }
    let d = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    use chrono::TimeZone;
    display.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|d| (d, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_coding() {
        assert_eq!(pct("a b@group.calendar.google.com"), "a%20b%40group.calendar.google.com");
        assert_eq!(pct_decode("4%2F0Ab_x%20y+z"), "4/0Ab_x y z");
        assert_eq!(pct_decode("bad%2"), "bad%2");
    }

    #[test]
    fn parses_event_times() {
        let la = chrono_tz::America::Los_Angeles;
        let (d, all_day) = when(&serde_json::json!({"date": "2026-11-26"}), la).unwrap();
        assert!(all_day);
        assert_eq!(d.format("%Y-%m-%d %H:%M").to_string(), "2026-11-26 00:00");
        let (d, all_day) = when(&serde_json::json!({"dateTime": "2026-11-26T18:00:00Z"}), la).unwrap();
        assert!(!all_day);
        assert_eq!(d.format("%H:%M").to_string(), "10:00");
    }
}
