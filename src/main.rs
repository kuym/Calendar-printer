//! calprint: render Google Calendar into printable US Letter week and month
//! pages, and print them on a schedule from a Mac.

mod config;
mod google;
mod ics;
mod launchd;
mod metrics;
mod model;
mod pdf;
mod printer;
mod render;
mod sample;
mod schedule;
mod sources;
mod summarize;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};

use config::Config;
use schedule::{Job, State};

#[derive(Parser)]
#[command(version, about = "Printed wall calendars from Google Calendar")]
struct Cli {
    /// Config file [default: ~/.config/calprint/config.toml, or $CALPRINT_CONFIG]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a month or week page right now (on demand), even if scheduled printing is off.
    Print {
        #[command(subcommand)]
        page: PrintPage,
    },
    /// Render a monthly page (default: next month).
    Month {
        /// this, next, or YYYY-MM.
        month: Option<String>,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Render a weekly page (default: next week).
    Week {
        /// this, next, or any date in the week as YYYY-MM-DD.
        date: Option<String>,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Scheduled entry point: print whatever is due now. Safe to run any time.
    Run {
        /// Don't print or record anything; just say what would happen.
        #[arg(long)]
        dry_run: bool,
        /// Pretend the current time is this (RFC 3339), for testing.
        #[arg(long)]
        now: Option<String>,
    },
    /// Sign in to Google once (only needed for google_id calendars).
    Auth,
    /// List calendars visible through the Google API, with their IDs.
    Calendars,
    /// List the printers installed on this Mac.
    Printers,
    /// Install (or update) the launchd agent that runs `calprint run` hourly.
    Install,
    /// Remove the launchd agent.
    Uninstall,
    /// Preview LLM summaries for a range of days (or print each day's model input with --all).
    Summarize {
        /// First day, YYYY-MM-DD.
        from: String,
        /// Last day, YYYY-MM-DD [default: same as FROM].
        to: Option<String>,
        /// Lines a cell holds [default: a 5-week month grid's cell].
        #[arg(long)]
        max_lines: Option<usize>,
        /// Print every day's model input without calling the model (for writing examples).
        #[arg(long)]
        all: bool,
    },
    /// Render demo month and week pages with made-up events (no network).
    Sample {
        /// Directory for the PDFs.
        #[arg(long, default_value = ".")]
        out_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum PrintPage {
    /// A month: this (default), next, or YYYY-MM.
    Month { month: Option<String> },
    /// A week: this (default), next, or any date in the week as YYYY-MM-DD.
    Week { date: Option<String> },
}

#[derive(clap::Args)]
struct OutputArgs {
    /// Where to write the PDF [default: archive_dir, or the state dir].
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// Send it to the configured printer too.
    #[arg(long)]
    print: bool,
}

pub fn log(msg: &str) {
    eprintln!("{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
}

fn now_in(cfg: &Config) -> DateTime<Tz> {
    Utc::now().with_timezone(&cfg.timezone)
}

fn output_dir(cfg: &Config) -> PathBuf {
    cfg.archive_dir.clone().unwrap_or_else(|| cfg.state_dir.join("pdf"))
}

fn job_title(job: Job) -> String {
    match job {
        Job::Week(d) => format!("Week of {}", d.format("%b %-d, %Y")),
        Job::Month(y, m) => render::month_first(y, m).format("%B %Y").to_string(),
    }
}

/// Fetch events and render the page for `job`.
fn build(cfg: &Config, job: Job, demo: bool) -> Result<Vec<u8>> {
    let (first, last) = match job {
        Job::Week(start) => (start, start + Duration::days(6)),
        Job::Month(y, m) => render::month_grid(y, m, cfg.week_start),
    };
    let events = if demo { sample::events(cfg, first, last) } else { sources::fetch_events(cfg, first, last)? };
    let summaries = if cfg.summarize.enabled {
        let rows = ((last - first).num_days() as usize + 1) / 7;
        let days = (0..=(last - first).num_days()).map(|i| first + Duration::days(i));
        summarize::summarize_days(cfg, &events, days, render::cell_line_budget(rows))
    } else {
        summarize::Summaries::new()
    };
    let page = match job {
        Job::Week(start) => render::week_page(cfg, start, &events, &summaries),
        Job::Month(y, m) => render::month_page(cfg, y, m, &events, &summaries),
    };
    Ok(pdf::write_pdf(&job_title(job), vec![page]))
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn render_one(cfg: &Config, job: Job, out: OutputArgs) -> Result<()> {
    let bytes = build(cfg, job, false)?;
    let path = out.out.unwrap_or_else(|| output_dir(cfg).join(job.file_name()));
    write(&path, &bytes)?;
    println!("Wrote {}", path.display());
    if out.print {
        println!("{}", printer::print(&cfg.printer, &path, &job_title(job))?);
    }
    Ok(())
}

fn run(cfg: &Config, dry_run: bool, now: Option<&str>) -> Result<()> {
    let now = match now {
        Some(s) => DateTime::parse_from_rfc3339(s).context("--now must be RFC 3339")?.with_timezone(&cfg.timezone),
        None => now_in(cfg),
    };
    let state_path = schedule::state_path(cfg);
    let mut state = State::load(&state_path)?;
    let (due, missed) = schedule::due_jobs(cfg, now, &state);
    for m in &missed {
        // Recorded so the warning appears once, not every hour.
        log(&format!("missed {}: print time {} was over {}h ago; skipping", m.job.key(), m.moment, cfg.schedule.catch_up_hours));
        if !dry_run {
            state.printed.insert(m.job.key());
        }
    }
    if due.is_empty() {
        if dry_run {
            println!("Nothing due at {now}.");
        }
        if !dry_run && !missed.is_empty() {
            state.save(&state_path)?;
        }
        return Ok(());
    }
    let mut failures = 0;
    for d in due {
        let key = d.job.key();
        if dry_run {
            println!("Would print {key} (scheduled {})", d.moment);
            continue;
        }
        let result = (|| -> Result<()> {
            let bytes = build(cfg, d.job, false)?;
            let path = output_dir(cfg).join(d.job.file_name());
            write(&path, &bytes)?;
            log(&format!("{key}: wrote {}", path.display()));
            if cfg.printer.enabled {
                let msg = printer::print(&cfg.printer, &path, &job_title(d.job))?;
                log(&format!("{key}: {msg}"));
            } else {
                log(&format!("{key}: printing disabled in config; PDF only"));
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.printed.insert(key);
                state.save(&state_path)?;
            }
            Err(e) => {
                // Not recorded, so the next hourly run retries within the catch-up window.
                failures += 1;
                log(&format!("{key}: FAILED: {e:#}"));
            }
        }
    }
    if failures > 0 {
        bail!("{failures} job(s) failed; will retry next hour");
    }
    Ok(())
}

/// "this", "next" (the default), or YYYY-MM.
fn parse_month(s: Option<&str>, cfg: &Config) -> Result<Job> {
    let today = now_in(cfg).date_naive();
    let d = match s.unwrap_or("next") {
        "this" => today,
        "next" => render::month_last(today.year(), today.month()) + Duration::days(1),
        s => NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d").with_context(|| format!("month must be this, next or YYYY-MM, got {s:?}"))?,
    };
    Ok(Job::Month(d.year(), d.month()))
}

/// "this", "next" (the default), or any date in the week as YYYY-MM-DD.
fn parse_week(s: Option<&str>, cfg: &Config) -> Result<Job> {
    let today = now_in(cfg).date_naive();
    let day = match s.unwrap_or("next") {
        "this" => today,
        "next" => today + Duration::days(7),
        s => NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("week must be this, next or YYYY-MM-DD, got {s:?}"))?,
    };
    Ok(Job::Week(render::week_start_of(day, cfg.week_start)))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match real_main(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(&format!("error: {e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let config_path = cli.config.clone().unwrap_or_else(config::default_config_path);
    let load = || config::load(&config_path);
    match cli.command {
        Cmd::Print { page } => {
            let cfg = load()?;
            let job = match page {
                PrintPage::Month { month } => parse_month(Some(month.as_deref().unwrap_or("this")), &cfg)?,
                PrintPage::Week { date } => parse_week(Some(date.as_deref().unwrap_or("this")), &cfg)?,
            };
            render_one(&cfg, job, OutputArgs { out: None, print: true })
        }
        Cmd::Month { month, out } => {
            let cfg = load()?;
            render_one(&cfg, parse_month(month.as_deref(), &cfg)?, out)
        }
        Cmd::Week { date, out } => {
            let cfg = load()?;
            render_one(&cfg, parse_week(date.as_deref(), &cfg)?, out)
        }
        Cmd::Run { dry_run, now } => run(&load()?, dry_run, now.as_deref()),
        Cmd::Auth => google::authorize(&load()?.google),
        Cmd::Calendars => {
            let cfg = load()?;
            for (id, name) in google::Client::connect(&cfg.google)?.calendars()? {
                println!("{name}\n    google_id = \"{id}\"");
            }
            Ok(())
        }
        Cmd::Printers => {
            print!("{}", printer::list()?);
            Ok(())
        }
        Cmd::Install => {
            let cfg = load()?;
            let path = launchd::install(&cfg, &config_path)?;
            println!("Installed {}\nLogs: {}", path.display(), launchd::log_path().display());
            println!("Check what is due with: calprint run --dry-run");
            Ok(())
        }
        Cmd::Uninstall => {
            launchd::uninstall()?;
            println!("Removed {}", launchd::plist_path().display());
            Ok(())
        }
        Cmd::Summarize { from, to, max_lines, all } => {
            let cfg = load()?;
            let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("dates must be YYYY-MM-DD, got {s:?}"));
            let first = date(&from)?;
            let last = to.as_deref().map(date).transpose()?.unwrap_or(first);
            let max_lines = max_lines.unwrap_or_else(|| render::cell_line_budget(5));
            let events = sources::fetch_events(&cfg, first, last)?;
            let days: Vec<NaiveDate> = (0..=(last - first).num_days()).map(|i| first + Duration::days(i)).collect();
            if all {
                for day in days {
                    let input = summarize::day_input(&cfg, &events, day);
                    if !input.is_empty() {
                        println!("# {day} ({} lines)\n{input}\n", render::day_line_count(&cfg, &events, day));
                    }
                }
                return Ok(());
            }
            let summaries = summarize::summarize_days(&cfg, &events, days.iter().copied(), max_lines);
            for day in days {
                if let Some(lines) = summaries.get(&day) {
                    println!("# {day}: {} lines -> {}", render::day_line_count(&cfg, &events, day), render::summary_line_count(lines));
                    println!("{}\n  =>", summarize::day_input(&cfg, &events, day));
                    for l in lines {
                        println!("  {}{}", if l.bold { "**" } else { "" }, l.text);
                    }
                    println!();
                }
            }
            Ok(())
        }
        Cmd::Sample { out_dir } => {
            let cfg = match config::load(&config_path) {
                Ok(c) => c,
                Err(_) => config::parse(include_str!("../config.example.toml"))?,
            };
            let today = now_in(&cfg).date_naive();
            let month = parse_month(None, &cfg)?;
            let week = Job::Week(render::week_start_of(today + Duration::days(7), cfg.week_start));
            for job in [month, week] {
                let path = out_dir.join(format!("sample-{}", job.file_name()));
                write(&path, &build(&cfg, job, true)?)?;
                println!("Wrote {}", path.display());
            }
            Ok(())
        }
    }
}
