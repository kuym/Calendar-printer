//! Install the hourly launchd agent that runs `calprint run`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{Config, home};

pub const LABEL: &str = "com.calprint.agent";

pub fn plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

pub fn log_path() -> PathBuf {
    home().join("Library/Logs/calprint.log")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Fire every hour at the configured print minute(s); `calprint run` decides what's due.
/// launchd also runs a missed interval once when the Mac wakes from sleep.
pub fn plist(cfg: &Config, exe: &Path, config_path: &Path) -> String {
    use chrono::Timelike;
    let mut minutes: Vec<u32> = vec![cfg.schedule.weekly_time.minute(), cfg.schedule.monthly_time.minute()];
    minutes.sort();
    minutes.dedup();
    let intervals: String = minutes
        .iter()
        .map(|m| format!("        <dict><key>Minute</key><integer>{m}</integer></dict>\n"))
        .collect();
    let log = xml_escape(&log_path().display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--config</string>
        <string>{config}</string>
        <string>run</string>
    </array>
    <key>StartCalendarInterval</key>
    <array>
{intervals}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.display().to_string()),
        config = xml_escape(&config_path.display().to_string()),
    )
}

fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output().context("running id -u")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn install(cfg: &Config, config_path: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let config_path = config_path.canonicalize().with_context(|| format!("config {} not found", config_path.display()))?;
    let path = plist_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    if let Some(dir) = log_path().parent() {
        std::fs::create_dir_all(dir)?;
    }
    let domain = format!("gui/{}", uid()?);
    // Replace any previous version; ignore "not loaded" errors.
    let _ = Command::new("launchctl").args(["bootout", &domain, &path.display().to_string()]).output();
    std::fs::write(&path, plist(cfg, &exe, &config_path))?;
    let out = Command::new("launchctl").args(["bootstrap", &domain, &path.display().to_string()]).output()?;
    if !out.status.success() {
        bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(path)
}

pub fn uninstall() -> Result<()> {
    let path = plist_path();
    let domain = format!("gui/{}", uid()?);
    let _ = Command::new("launchctl").args(["bootout", &domain, &path.display().to_string()]).output();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_valid() {
        let cfg = crate::config::parse(include_str!("../config.example.toml")).unwrap();
        let text = plist(&cfg, Path::new("/Users/me/.cargo/bin/calprint"), Path::new("/Users/me/.config/calprint/config.toml"));
        assert!(text.contains("<integer>0</integer>"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        std::fs::write(&path, text).unwrap();
        if let Ok(out) = Command::new("plutil").arg("-lint").arg(&path).output() {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        }
    }
}
