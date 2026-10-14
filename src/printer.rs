//! Send PDFs to a CUPS printer with `lp`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::PrinterConfig;

pub fn lp_args(cfg: &PrinterConfig, file: &Path, title: &str) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(name) = &cfg.name {
        args.extend(["-d".to_string(), name.clone()]);
    }
    args.extend(["-n".to_string(), cfg.copies.to_string(), "-t".to_string(), title.to_string()]);
    for opt in &cfg.options {
        args.extend(["-o".to_string(), opt.clone()]);
    }
    args.push(file.display().to_string());
    args
}

/// Submit the job; returns lp's confirmation line (e.g. "request id is Printer-42 (1 file(s))").
pub fn print(cfg: &PrinterConfig, file: &Path, title: &str) -> Result<String> {
    if let Some(name) = &cfg.name {
        let ok = Command::new("lpstat").args(["-p", name]).output().map(|o| o.status.success()).unwrap_or(false);
        if !ok {
            bail!("printer {name:?} is not installed on this Mac (see `calprint printers`)");
        }
    }
    let out = Command::new("lp").args(lp_args(cfg, file, title)).output().context("running lp")?;
    if !out.status.success() {
        bail!("lp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn list() -> Result<String> {
    let out = Command::new("lpstat").args(["-p", "-d"]).output().context("running lpstat")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_lp_command() {
        let cfg = PrinterConfig {
            name: Some("Office".into()),
            copies: 2,
            options: vec!["media=Letter".into(), "sides=one-sided".into()],
            enabled: true,
        };
        assert_eq!(
            lp_args(&cfg, Path::new("/tmp/a b.pdf"), "November 2026").join(" "),
            "-d Office -n 2 -t November 2026 -o media=Letter -o sides=one-sided /tmp/a b.pdf"
        );
    }
}
