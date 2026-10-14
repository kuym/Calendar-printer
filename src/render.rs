//! Page layouts matching the family's reference Google Doc: a month grid and
//! a one-week grid with a day-by-day detail list, both portrait US Letter.
//!
//! Measurements (points) were taken from the reference PDF export.

use chrono::{Datelike, Duration, NaiveDate, Timelike, Weekday};

use crate::config::Config;
use crate::model::{Event, events_on};
use crate::pdf::{self, BLACK, Font, Gray, Page, WHITE};
use crate::summarize::{CellLine as SummaryLine, Summaries};

const MARGIN: f32 = 36.0;
const GRID_LEFT: f32 = 36.0;
const GRID_RIGHT: f32 = 576.0;
const PAGE_BOTTOM: f32 = 792.0 - 36.0;

const TITLE_SIZE: f32 = 30.0;
const TITLE_BASELINE: f32 = 64.5;
const QR_SIZE: f32 = 41.0;
const WEEK_LABEL_X: f32 = 108.0;

const TABLE_TOP: f32 = 89.0;
const HEADER_H: f32 = 26.0;
const HEADER_FILL: Gray = Gray(0.6); // Google Docs "dark gray 2" #999999
const RULE_W: f32 = 1.0;

const DAY_NUM_SIZE: f32 = 18.0;
const DAY_NUM_BASELINE: f32 = 23.2; // below row top
const EVENT_TOP: f32 = 36.0; // row top to the first event's line box
const EVENT_SIZE: f32 = 10.0;
const EVENT_LEADING: f32 = 1.2; // line height / font size
const EVENT_GAP: f32 = 4.0;
const CELL_PAD_X: f32 = 4.5;
const CELL_PAD_BOTTOM: f32 = 9.0;
const MONTH_MIN_ROW: f32 = 76.0;
const WEEK_MIN_ROW: f32 = 146.0;
/// Lines a weekly cell holds before its day is summarized (the detail list keeps everything).
const WEEK_CELL_LINES: usize = 9;

const WEEKEND_FILL: Gray = Gray(0.953); // #f3f3f3
const SHADED_FILL: Gray = Gray(0.851); // #d9d9d9
const OUTSIDE_FILL: Gray = Gray(0.8); // #cccccc

const LIST_HEADING_SIZE: f32 = 16.0;
const LIST_SIZE: f32 = 11.0;
const LIST_BULLET_X: f32 = 54.0;
const LIST_TEXT_X: f32 = 72.0;
const LIST_LINE: f32 = 14.5;

/// First day of the week containing `day`.
pub fn week_start_of(day: NaiveDate, week_start: Weekday) -> NaiveDate {
    let back = (day.weekday().num_days_from_monday() + 7 - week_start.num_days_from_monday()) % 7;
    day - Duration::days(back as i64)
}

pub fn month_first(year: i32, month: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, 1).expect("valid month")
}

pub fn month_last(year: i32, month: u32) -> NaiveDate {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    month_first(y, m) - Duration::days(1)
}

/// The dates shown in a month grid: whole weeks covering the month.
pub fn month_grid(year: i32, month: u32, week_start: Weekday) -> (NaiveDate, NaiveDate) {
    let first = week_start_of(month_first(year, month), week_start);
    let last = week_start_of(month_last(year, month), week_start) + Duration::days(6);
    (first, last)
}

// --- Text for events -----------------------------------------------------------

/// How times are written.
#[derive(Clone, Copy, PartialEq)]
pub enum TimeStyle {
    /// "7pm", "7:15pm".
    Full,
    /// For narrow grid cells: "7pm", but "7:15p" when there are minutes.
    Compact,
}

/// "7pm", "7:15pm" / "7:15p" (or "19:00" with a 24-hour clock).
fn clock(t: impl Timelike, h24: bool, suffix: bool, style: TimeStyle) -> String {
    let (h, m) = (t.hour(), t.minute());
    if h24 {
        return format!("{h}:{m:02}");
    }
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let mer = match (suffix, h < 12, style == TimeStyle::Compact && m != 0) {
        (false, _, _) => "",
        (true, true, false) => "am",
        (true, false, false) => "pm",
        (true, true, true) => "a",
        (true, false, true) => "p",
    };
    if m == 0 { format!("{h12}{mer}") } else { format!("{h12}:{m:02}{mer}") }
}

/// "4-6pm", "8am-12pm", "2:30-4:30pm" ("2:30-4:30p" compact).
fn time_range(e: &Event, h24: bool, style: TimeStyle) -> String {
    if e.end <= e.start || e.is_multi_day() {
        return clock(e.start, h24, true, style);
    }
    let same_half = (e.start.hour() < 12) == (e.end.hour() < 12);
    format!("{}-{}", clock(e.start, h24, !same_half, style), clock(e.end, h24, true, style))
}

/// Recurring events show their start time; one-off events show the full range.
pub fn time_note(e: &Event, day: NaiveDate, h24: bool, style: TimeStyle) -> Option<String> {
    if e.all_day || e.first_day() != day {
        None
    } else if e.recurring || e.is_multi_day() {
        Some(clock(e.start, h24, true, style))
    } else {
        Some(time_range(e, h24, style))
    }
}

/// A prefix typed into the title itself, e.g. "A+B+C: Fly to LAX" -> Some(["A", "B", "C"]).
fn title_prefix(title: &str) -> Option<Vec<&str>> {
    let (head, rest) = title.split_once(':')?;
    if !rest.starts_with(' ') {
        return None;
    }
    let parts: Vec<&str> = head.split('+').map(str::trim).collect();
    let ok = parts.iter().all(|p| {
        (1..=3).contains(&p.len()) && p.starts_with(|c: char| c.is_ascii_uppercase()) && p.chars().all(|c| c.is_ascii_alphabetic())
    });
    ok.then_some(parts)
}

/// The time shown in a grid cell: none for all-day events, daily routines, or
/// days after the first of a multi-day event.
pub fn grid_time(cfg: &Config, e: &Event, day: NaiveDate) -> Option<String> {
    if e.daily { None } else { time_note(e, day, cfg.layout.clock_24h, TimeStyle::Compact) }
}

/// An event's grid label without its time: "C+D: Soccer game".
pub fn cell_label(cfg: &Config, e: &Event) -> String {
    if title_prefix(&e.title).is_some() {
        return e.title.clone();
    }
    let initials: Vec<&str> = e.calendars.iter().map(|&i| cfg.calendars[i].initial.as_str()).filter(|s| !s.is_empty()).collect();
    if initials.is_empty() { e.title.clone() } else { format!("{}: {}", initials.join("+"), e.title) }
}

/// Text in a grid cell: "C+D: Soccer Game 10:15am".
pub fn cell_text(cfg: &Config, e: &Event, day: NaiveDate) -> String {
    match grid_time(cfg, e, day) {
        Some(t) => format!("{} {t}", cell_label(cfg, e)),
        None => cell_label(cfg, e),
    }
}

/// Text in the weekly detail list: "Charlie+Denise: Soccer Game 10:15am @ Masjid".
pub fn list_text(cfg: &Config, e: &Event, day: NaiveDate) -> String {
    let mut s = match title_prefix(&e.title) {
        Some(initials) => {
            let names: Vec<&str> = initials.iter().map(|i| cfg.people.get(*i).map(String::as_str).unwrap_or(i)).collect();
            let rest = e.title.split_once(':').map(|(_, r)| r.trim()).unwrap_or_default();
            format!("{}: {rest}", names.join("+"))
        }
        None => {
            let names: Vec<&str> = e.calendars.iter().map(|&i| cfg.calendars[i].person.as_str()).filter(|s| !s.is_empty()).collect();
            if names.is_empty() { e.title.clone() } else { format!("{}: {}", names.join("+"), e.title) }
        }
    };
    if let Some(t) = time_note(e, day, cfg.layout.clock_24h, TimeStyle::Full) {
        s = format!("{s} {t}");
    }
    if e.is_multi_day() && e.first_day() == day {
        s = format!("{s} (through {})", e.last_day().format("%a %b %-d"));
    }
    if cfg.layout.show_location
        && let Some(loc) = e.location.as_deref().and_then(|l| l.split(',').next()).map(str::trim).filter(|l| !l.is_empty()) {
            s = format!("{s} @ {loc}");
        }
    s
}

/// One-off events are bold; routine repeating ones are light.
fn cell_font(e: &Event) -> Font {
    if e.recurring { Font::Light } else { Font::Bold }
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn month_abbrev(d: NaiveDate) -> String {
    let m = d.format("%b").to_string();
    if m == "May" { m } else { format!("{m}.") }
}

// --- Grid ----------------------------------------------------------------------

struct CellLine {
    bytes: Vec<u8>,
    font: Font,
    /// Extra space before this line (between events).
    gap: f32,
}

const CELL_TEXT_W: f32 = (GRID_RIGHT - GRID_LEFT) / 7.0 - 2.0 * CELL_PAD_X;

/// Wrap (text, font) items into cell lines with a small gap between items.
fn wrap_items(items: impl IntoIterator<Item = (String, Font)>, size: f32) -> Vec<CellLine> {
    let mut out = Vec::new();
    for (n, (text, font)) in items.into_iter().enumerate() {
        for (i, bytes) in pdf::wrap(&text, font, size, CELL_TEXT_W).into_iter().enumerate() {
            let gap = if i == 0 && n > 0 { EVENT_GAP * size / EVENT_SIZE } else { 0.0 };
            out.push(CellLine { bytes, font, gap });
        }
    }
    out
}

fn summary_items(lines: &[SummaryLine]) -> impl Iterator<Item = (String, Font)> + '_ {
    lines.iter().map(|l| (l.text.clone(), if l.bold { Font::Bold } else { Font::Light }))
}

/// Wrapped lines for one day cell: its summary if it has one, else every event.
fn cell_lines(cfg: &Config, events: &[Event], summaries: &Summaries, day: NaiveDate, size: f32) -> Vec<CellLine> {
    match summaries.get(&day) {
        Some(lines) => wrap_items(summary_items(lines), size),
        None => wrap_items(events_on(events, day).into_iter().map(|e| (cell_text(cfg, e, day), cell_font(e))), size),
    }
}

/// Printed lines a day's events take at full size.
pub fn day_line_count(cfg: &Config, events: &[Event], day: NaiveDate) -> usize {
    cell_lines(cfg, events, &Summaries::new(), day, EVENT_SIZE).len()
}

/// Printed lines a summary takes at full size.
pub fn summary_line_count(lines: &[SummaryLine]) -> usize {
    wrap_items(summary_items(lines), EVENT_SIZE).len()
}

/// Lines that fit in a cell at full size when the grid has `rows` week rows.
pub fn cell_line_budget(rows: usize) -> usize {
    if rows <= 1 {
        return WEEK_CELL_LINES;
    }
    let row_h = (PAGE_BOTTOM - TABLE_TOP - HEADER_H) / rows as f32;
    (((row_h - EVENT_TOP - CELL_PAD_BOTTOM) / (EVENT_SIZE * EVENT_LEADING)).floor() as usize).max(2)
}

fn lines_height(lines: &[CellLine], size: f32) -> f32 {
    lines.iter().map(|l| l.gap + size * EVENT_LEADING).sum()
}

fn day_fill(cfg: &Config, events: &[Event], day: NaiveDate, in_range: bool) -> Option<Gray> {
    let shaded = events.iter().any(|e| e.touches(day) && e.calendars.iter().any(|&i| cfg.calendars[i].shade_days));
    if !in_range {
        Some(OUTSIDE_FILL)
    } else if shaded {
        Some(SHADED_FILL)
    } else if matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
        Some(WEEKEND_FILL)
    } else {
        None
    }
}

/// Draw the header row and `rows` weeks of day cells starting at `first`.
/// Returns the y coordinate of the table's bottom edge.
#[allow(clippy::too_many_arguments)]
fn draw_grid(p: &mut Page, cfg: &Config, events: &[Event], summaries: &Summaries, first: NaiveDate, rows: usize, in_range: impl Fn(NaiveDate) -> bool, min_row: f32) -> f32 {
    let col_w = (GRID_RIGHT - GRID_LEFT) / 7.0;
    let body_top = TABLE_TOP + HEADER_H;
    let available = PAGE_BOTTOM - body_top;
    let days = |r: usize| (0..7).map(move |c| first + Duration::days((r * 7 + c) as i64));

    // Shrink the event text until every row fits on the page.
    let mut size = EVENT_SIZE;
    let (heights, lines) = loop {
        let lines: Vec<Vec<Vec<CellLine>>> = (0..rows).map(|r| days(r).map(|d| cell_lines(cfg, events, summaries, d, size)).collect()).collect();
        let heights: Vec<f32> =
            lines.iter().map(|row| row.iter().map(|l| EVENT_TOP + lines_height(l, size) + CELL_PAD_BOTTOM).fold(min_row, f32::max)).collect();
        if heights.iter().sum::<f32>() <= available || size <= 7.0 {
            break (heights, lines);
        }
        size -= 0.5;
    };
    // Still too tall at the smallest size: give every row an equal share and clip cells.
    let heights = if heights.iter().sum::<f32>() > available { vec![available / rows as f32; rows] } else { heights };

    // Fills.
    p.fill_rect(GRID_LEFT, TABLE_TOP, GRID_RIGHT - GRID_LEFT, HEADER_H, HEADER_FILL);
    let mut y = body_top;
    for (r, h) in heights.iter().enumerate() {
        for (c, day) in days(r).enumerate() {
            if let Some(fill) = day_fill(cfg, events, day, in_range(day)) {
                p.fill_rect(GRID_LEFT + c as f32 * col_w, y, col_w, *h, fill);
            }
        }
        y += h;
    }
    let bottom = y;

    // Header labels.
    for (c, wd) in days(0).enumerate() {
        p.text_center(GRID_LEFT + (c as f32 + 0.5) * col_w, TABLE_TOP + 17.8, Font::Bold, 12.0, WHITE, &wd.format("%A").to_string());
    }

    // Cell contents.
    let leading = size * EVENT_LEADING;
    let mut y = body_top;
    for (r, h) in heights.iter().enumerate() {
        let limit = y + h - CELL_PAD_BOTTOM + 2.0;
        for (c, day) in days(r).enumerate() {
            let x = GRID_LEFT + c as f32 * col_w;
            let label = if day.day() == 1 { format!("{} 1", month_abbrev(day)) } else { day.day().to_string() };
            p.text_right(x + col_w - 5.0, y + DAY_NUM_BASELINE, Font::Light, DAY_NUM_SIZE, BLACK, &label);

            let cell = &lines[r][c];
            let mut ly = y + EVENT_TOP;
            for (i, line) in cell.iter().enumerate() {
                let is_last = i + 1 == cell.len();
                // Out of room: if more lines follow this one, end with an ellipsis instead.
                if ly + line.gap + leading > limit || (!is_last && ly + line.gap + 2.0 * leading > limit && lines_height(&cell[i..], size) > limit - ly) {
                    p.text(x + CELL_PAD_X, ly + line.gap + size * Font::Light.ascent(), Font::Light, size, BLACK, "\u{2026}");
                    break;
                }
                ly += line.gap;
                p.bytes(x + CELL_PAD_X, ly + size * line.font.ascent(), line.font, size, BLACK, &line.bytes);
                ly += leading;
            }
        }
        y += h;
    }

    // Rules: outer frame, header, row and column lines.
    let mut y = TABLE_TOP;
    p.line(GRID_LEFT, y, GRID_RIGHT, y, RULE_W, BLACK);
    y += HEADER_H;
    p.line(GRID_LEFT, y, GRID_RIGHT, y, RULE_W, BLACK);
    for h in &heights {
        y += h;
        p.line(GRID_LEFT, y, GRID_RIGHT, y, RULE_W, BLACK);
    }
    for c in 0..=7 {
        let x = GRID_LEFT + c as f32 * col_w;
        p.line(x, TABLE_TOP, x, bottom, RULE_W, BLACK);
    }
    bottom
}

fn draw_qr(p: &mut Page, cfg: &Config) {
    if let Some(url) = &cfg.qr_url {
        p.qr(MARGIN + 1.0, MARGIN + 1.0, QR_SIZE, url);
    }
}

/// Draw title segments right-aligned at the right margin, shrinking to stay right of `min_left`.
fn draw_title(p: &mut Page, parts: &[(String, Font)], min_left: f32) {
    let width = |size: f32| parts.iter().map(|(t, f)| pdf::text_width(t, *f, size)).sum::<f32>();
    let mut size = TITLE_SIZE;
    while GRID_RIGHT - width(size) < min_left && size > 14.0 {
        size -= 1.0;
    }
    let mut x = GRID_RIGHT - width(size);
    for (t, f) in parts {
        p.text(x, TITLE_BASELINE, *f, size, BLACK, t);
        x += pdf::text_width(t, *f, size);
    }
}

// --- Month -------------------------------------------------------------------

pub fn month_page(cfg: &Config, year: i32, month: u32, events: &[Event], summaries: &Summaries) -> Page {
    let mut p = Page::letter();
    draw_qr(&mut p, cfg);
    let title = month_first(year, month).format("%B %Y").to_string();
    draw_title(&mut p, &[(title, Font::Light)], MARGIN + QR_SIZE + 12.0);
    let (grid_first, grid_last) = month_grid(year, month, cfg.week_start);
    let rows = ((grid_last - grid_first).num_days() + 1) as usize / 7;
    draw_grid(&mut p, cfg, events, summaries, grid_first, rows, |d| d.month() == month, MONTH_MIN_ROW);
    p
}

// --- Week --------------------------------------------------------------------

/// Title parts: "January " + bold "12-18" + ", 2026".
pub fn week_title(start: NaiveDate) -> Vec<(String, Font)> {
    let end = start + Duration::days(6);
    let range = if start.month() == end.month() {
        format!("{}-{}", start.day(), end.day())
    } else {
        format!("{} - {} {}", start.day(), end.format("%B"), end.day())
    };
    vec![(format!("{} ", start.format("%B")), Font::Light), (range, Font::Bold), (format!(", {}", end.year()), Font::Light)]
}

pub fn week_label(start: NaiveDate) -> String {
    format!("W{}", (start + Duration::days(3)).iso_week().week())
}

pub fn week_page(cfg: &Config, start: NaiveDate, events: &[Event], summaries: &Summaries) -> Page {
    let mut p = Page::letter();
    draw_qr(&mut p, cfg);
    let label_x = if cfg.qr_url.is_some() { WEEK_LABEL_X } else { MARGIN };
    let label = week_label(start);
    p.text(label_x, TITLE_BASELINE, Font::Light, 16.0, BLACK, &label);
    draw_title(&mut p, &week_title(start), label_x + pdf::text_width(&label, Font::Light, 16.0) + 16.0);

    let bottom = draw_grid(&mut p, cfg, events, summaries, start, 1, |_| true, WEEK_MIN_ROW);
    if cfg.layout.weekly_details {
        detail_list(&mut p, cfg, events, start, bottom);
    }
    p
}

/// Vertical offsets (at scale 1) in the detail list, measured from the reference.
const FIRST_HEADING: f32 = 35.8; // table bottom -> first heading baseline
const NEXT_HEADING: f32 = 25.4; // last bullet baseline -> next heading baseline
const FIRST_BULLET: f32 = 19.3; // heading baseline -> first bullet baseline

/// "Monday, Apr 13th" headings with a bullet per event, below the week grid.
fn detail_list(p: &mut Page, cfg: &Config, events: &[Event], start: NaiveDate, table_bottom: f32) {
    let text_w = GRID_RIGHT - LIST_TEXT_X;
    let days: Vec<NaiveDate> = (0..7).map(|i| start + Duration::days(i)).collect();
    // Wrapped bullets per day at a given scale, plus the total height they need.
    let layout = |scale: f32| {
        let size = LIST_SIZE * scale;
        let mut total = 0.0;
        let mut out = Vec::new();
        for (i, day) in days.iter().enumerate() {
            let items: Vec<(Vec<Vec<u8>>, Font)> = events_on(events, *day)
                .into_iter()
                .map(|e| {
                    let font = if e.recurring { Font::Sans } else { Font::SansBold };
                    (pdf::wrap(&list_text(cfg, e, *day), font, size, text_w), font)
                })
                .collect();
            let lines = items.iter().map(|(l, _)| l.len()).sum::<usize>().max(1);
            total += (if i == 0 { FIRST_HEADING } else { NEXT_HEADING } + FIRST_BULLET + (lines - 1) as f32 * LIST_LINE) * scale;
            out.push(items);
        }
        (out, total)
    };
    let room = PAGE_BOTTOM - table_bottom;
    let mut scale = 1.0;
    let (mut items, mut total) = layout(scale);
    while total > room && scale > 0.7 {
        scale -= 0.05;
        (items, total) = layout(scale);
    }

    let (heading, size, line) = (LIST_HEADING_SIZE * scale, LIST_SIZE * scale, LIST_LINE * scale);
    let bullet = |p: &mut Page, y: f32| p.circle(LIST_BULLET_X + 2.5 * scale, y - size * 0.33, 1.9 * scale, BLACK);
    let mut y = table_bottom;
    for (i, (day, day_items)) in days.iter().zip(items).enumerate() {
        y += if i == 0 { FIRST_HEADING } else { NEXT_HEADING } * scale;
        if y > PAGE_BOTTOM {
            return;
        }
        let text = format!("{}, {} {}", day.format("%A"), day.format("%b"), ordinal(day.day()));
        p.text(MARGIN, y, Font::Light, heading, BLACK, &text);
        y += FIRST_BULLET * scale;
        if day_items.is_empty() {
            // An empty bullet, as in the reference, leaves room to write.
            bullet(p, y);
            continue;
        }
        let mut first_line = true;
        for (lines, font) in day_items {
            for (j, l) in lines.iter().enumerate() {
                if !first_line {
                    y += line;
                }
                first_line = false;
                if y > PAGE_BOTTOM {
                    return;
                }
                if j == 0 {
                    bullet(p, y);
                }
                p.bytes(LIST_TEXT_X, y, font, size, BLACK, l);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn cfg() -> Config {
        crate::config::parse(include_str!("../config.example.toml")).unwrap()
    }

    #[test]
    fn grids() {
        // November 2026 starts on a Sunday.
        assert_eq!(month_grid(2026, 11, Weekday::Mon), (d(2026, 10, 26), d(2026, 12, 6)));
        assert_eq!(month_grid(2026, 11, Weekday::Sun), (d(2026, 11, 1), d(2026, 12, 5)));
        assert_eq!(month_last(2026, 12), d(2026, 12, 31));
        assert_eq!(month_last(2028, 2), d(2028, 2, 29));
    }

    #[test]
    fn titles_and_labels() {
        let join = |v: Vec<(String, Font)>| v.into_iter().map(|(s, _)| s).collect::<String>();
        assert_eq!(join(week_title(d(2026, 1, 12))), "January 12-18, 2026");
        assert_eq!(join(week_title(d(2026, 8, 31))), "August 31 - September 6, 2026");
        assert_eq!(week_label(d(2026, 1, 12)), "W3");
        assert_eq!(week_label(d(2026, 4, 13)), "W16");
        assert_eq!(ordinal(13), "13th");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(month_abbrev(d(2026, 6, 1)), "Jun.");
        assert_eq!(month_abbrev(d(2026, 5, 1)), "May");
    }

    #[test]
    fn time_styles() {
        let t = |h, m| chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!(clock(t(15, 0), false, true, TimeStyle::Compact), "3pm");
        assert_eq!(clock(t(19, 15), false, true, TimeStyle::Compact), "7:15p");
        assert_eq!(clock(t(10, 5), false, true, TimeStyle::Compact), "10:05a");
        assert_eq!(clock(t(19, 15), false, true, TimeStyle::Full), "7:15pm");
        assert_eq!(clock(t(19, 15), true, true, TimeStyle::Compact), "19:15");
    }

    #[test]
    fn event_text_matches_reference_style() {
        let cfg = cfg();
        let tz = cfg.timezone;
        let at = |h, m| tz.with_ymd_and_hms(2026, 4, 18, h, m, 0).unwrap();
        let idx = |name: &str| cfg.calendars.iter().position(|c| c.person == name).unwrap();
        let day = d(2026, 4, 18);

        let mut e = crate::model::test_event(at(14, 30), at(15, 30), false);
        e.title = "Alex's Birthday".into();
        e.calendars = vec![idx("Charlie")];
        e.location = Some("San Francisco, CA".into());
        assert_eq!(cell_text(&cfg, &e, day), "C: Alex's Birthday 2:30-3:30p");
        assert_eq!(list_text(&cfg, &e, day), "Charlie: Alex's Birthday 2:30-3:30pm @ San Francisco");

        let mut e = crate::model::test_event(at(10, 15), at(11, 30), false);
        e.title = "Soccer Game".into();
        e.recurring = true;
        e.calendars = vec![idx("Charlie"), idx("Denise")];
        assert_eq!(cell_text(&cfg, &e, day), "C+D: Soccer Game 10:15a");
        assert_eq!(list_text(&cfg, &e, day), "Charlie+Denise: Soccer Game 10:15am");

        // A prefix typed in the title is kept on the grid and expanded in the list.
        let mut e = crate::model::test_event(at(0, 0), at(0, 0) + Duration::days(1), true);
        e.title = "A+C+D: L.A. resort trip".into();
        assert_eq!(cell_text(&cfg, &e, day), "A+C+D: L.A. resort trip");
        assert_eq!(list_text(&cfg, &e, day), "Adam+Charlie+Denise: L.A. resort trip");

        let mut e = crate::model::test_event(at(8, 0), at(12, 0), false);
        e.title = "Babysitter".into();
        e.calendars = vec![];
        assert_eq!(cell_text(&cfg, &e, day), "Babysitter 8am-12pm");
        assert_eq!(title_prefix("Note: something"), None);
    }

    #[test]
    fn busy_days_still_fit_on_the_page() {
        let cfg = cfg();
        let tz = cfg.timezone;
        let day = d(2026, 11, 18);
        let events: Vec<Event> = (0..20)
            .map(|i| {
                let mut e = crate::model::test_event(
                    tz.from_local_datetime(&day.and_hms_opt(6 + i / 2, (i % 2) * 30, 0).unwrap()).unwrap(),
                    tz.from_local_datetime(&day.and_hms_opt(7 + i / 2, 0, 0).unwrap()).unwrap(),
                    false,
                );
                e.title = format!("Event number {i} with a rather long descriptive title");
                e
            })
            .collect();
        assert!(!pdf::write_pdf("m", vec![month_page(&cfg, 2026, 11, &events, &Summaries::new())]).is_empty());
        assert!(!pdf::write_pdf("w", vec![week_page(&cfg, week_start_of(day, cfg.week_start), &events, &Summaries::new())]).is_empty());
    }

    #[test]
    fn summaries_replace_cell_text() {
        let cfg = cfg();
        let tz = cfg.timezone;
        let day = d(2026, 11, 18);
        let events: Vec<Event> = (0..12)
            .map(|i| {
                let mut e = crate::model::test_event(
                    tz.from_local_datetime(&day.and_hms_opt(7 + i, 0, 0).unwrap()).unwrap(),
                    tz.from_local_datetime(&day.and_hms_opt(8 + i, 0, 0).unwrap()).unwrap(),
                    false,
                );
                e.title = format!("Activity number {i} somewhere far away");
                e
            })
            .collect();
        assert!(day_line_count(&cfg, &events, day) > cell_line_budget(5));
        let mut summaries = Summaries::new();
        summaries.insert(day, vec![SummaryLine { text: "Busy day!".into(), bold: true }]);
        assert_eq!(summary_line_count(&summaries[&day]), 1);
        let bytes = pdf::write_pdf("m", vec![month_page(&cfg, 2026, 11, &events, &summaries)]);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("(Busy day!)"));
        assert!(!text.contains("Activity number 3"));
        assert_eq!(cell_line_budget(5), 6);
        assert_eq!(cell_line_budget(6), 5);
    }
}
