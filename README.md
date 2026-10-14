# calprint

Printed wall calendars from Google Calendar. `calprint` renders US Letter
(8.5" × 11") **weekly** and **monthly** pages as PDFs and sends them to a
printer on a predictable schedule from a Mac (e.g. a Mac mini at home):

| Page    | Prints                                          | Covers                         |
|---------|-------------------------------------------------|--------------------------------|
| Weekly  | Sunday 6:00 PM                                  | The week starting next Monday  |
| Monthly | 6:00 PM on the last day of each month           | Next month                     |

Days, times, week start, orientation and catch-up are all configurable.

Everything is a single Rust binary with no runtime dependencies. Printing goes
through the Mac's own CUPS (`lp`) and scheduling through `launchd`, so no Swift
helper is needed.

## Install

```sh
# Rust toolchain (once): https://rustup.rs
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

git clone <this repo> calendar-generator && cd calendar-generator
cargo install --path .          # installs ~/.cargo/bin/calprint
```

The binary is self-contained (fonts included). To deploy to another Apple
Silicon Mac without installing Rust there, copy it over:

```sh
cargo build --release
scp target/release/calprint user@remote.local:.local/bin/calprint
```

## Configure

```sh
mkdir -p ~/.config/calprint
cp config.example.toml ~/.config/calprint/config.toml
```

Edit `~/.config/calprint/config.toml`. The file is commented; the essentials:

### 0. People

Give each person's calendar an `initial` (and optionally `person`, their name).
The grid shows `C: Soccer 3pm`; the weekly list shows `Charlie: Soccer 3pm`. An event
that appears on several people's calendars is printed once as `A+C+D: LA trip`.
Events whose titles already start with initials (`B+C: Pick up @ School`) print
as typed; add a `[people]` entry (e.g. `E = "Erica"`) to spell those out in the
weekly list. Mark a holidays calendar `shade_days = true` to gray those days.

### 1. Calendars

Each `[[calendars]]` entry uses one of two ways to read Google Calendar:

**Secret iCal address (recommended — no sign-in, never expires).**
In Google Calendar on the web: *Settings → (your calendar) → Integrate calendar
→ Secret address in iCal format*. Copy it into `ics_url`. Repeat for each
calendar you want on the wall (family, each person, school, holidays…). Treat
these URLs like passwords.

**Google Calendar API (optional).** Use `google_id` instead of `ics_url` if
you'd rather not use secret addresses, or the calendar's owner has disabled
them.
1. In [Google Cloud Console](https://console.cloud.google.com/), create a
   project, enable the **Google Calendar API**, configure the OAuth consent
   screen (add yourself as a test user), and create an **OAuth client ID** of
   type **Desktop app**. Download the JSON to `~/.config/calprint/credentials.json`.
2. Run `calprint auth` once on the Mac mini; a browser opens to sign in.
   A read-only refresh token is saved to `~/.config/calprint/token.json`.
3. `calprint calendars` lists calendar IDs to paste into the config.

> If the consent screen is left in "Testing" mode, Google expires refresh tokens
> after 7 days. Publish the app (it can stay unverified for personal use) to
> keep the token working, or use secret iCal addresses.

### 2. Printer

```sh
calprint printers
```

Put the queue name in `[printer] name`. The default options print single-sided
on Letter paper (`media=Letter`, `sides=one-sided`), which matters for printers
that default to duplex. Any `lp -o` option can be added, e.g.
`"print-color-mode=monochrome"`.

## Try it

```sh
calprint sample --out-dir /tmp          # demo pages with made-up events, no network
calprint month                          # next month -> PDF (archive_dir)
calprint month 2026-12 -o ~/Desktop/dec.pdf
calprint week                           # next week
calprint week 2026-11-23 --print        # render and print now
calprint print month                    # print this month now (on demand)
calprint print month 2026-10            # ...or a specific month; also `next`
calprint print week next                # print next week now; also `this` or a date
calprint run --dry-run                  # what the scheduler would print right now
calprint run --dry-run --now 2026-10-31T18:05:00-07:00
```

Set `[printer] enabled = false` to exercise the schedule without using paper;
PDFs are still written to `archive_dir`.

## Schedule it

```sh
calprint install
```

This writes `~/Library/LaunchAgents/com.calprint.agent.plist` and loads it.
launchd runs `calprint run` at the top of every hour (and at login); `run`
prints only what is due and remembers what it has printed in
`~/Library/Application Support/calprint/state.json`, so:

- each page prints **once**, even if `run` is invoked repeatedly;
- if the Mac was asleep, offline, or the printer errored at 6 PM, it retries
  hourly and catches up within `catch_up_hours` (default 12). Later than that it
  logs the page as missed rather than printing a stale page;
- when the last day of a month is a Sunday, both pages print.

Logs go to `~/Library/Logs/calprint.log`. Re-run `calprint install` after
moving the binary or config; `calprint uninstall` removes the agent.

### Mac mini notes

- A LaunchAgent runs while your user is logged in. Enable automatic login
  (*System Settings → Users & Groups*) so it survives power outages and reboots.
- launchd runs a missed job when the Mac wakes, but a sleeping Mac won't wake
  on its own. Either disable sleep (*System Settings → Energy → Prevent automatic
  sleeping*) or schedule a wake shortly before print time:
  `sudo pmset repeat wakeorpoweron MTWRFSU 17:55:00`.

## Layout

Both pages are portrait US Letter and match the family's original Google Doc
(measured from its PDF export): Roboto type, a QR code in the top-left corner
(`qr_url`), the title top-right, and a grid with a gray header row of weekday
names and black rules.

- **Monthly** (`June 2026`): whole weeks Monday-Sunday. Day numbers sit at the
  top right of each cell; the first of a month reads `Jun. 1`. Weekends are light
  gray, holiday (`shade_days`) days a little darker, and days from the adjacent
  months darker still (their events are shown too). Rows grow to fit their
  events; on a very busy month the text shrinks (down to 7 pt) so the page still
  fits, and any cell that still overflows ends with "…".
- **Weekly** (`W43` + `October 19-25, 2026`, range in bold): the same grid for
  one week, then a day-by-day list (`Monday, Oct 19th`) with names spelled out,
  time ranges and locations (`@ Place`). Days with nothing get an empty bullet to
  write on. Turn the list off with `layout.weekly_details = false`.
- **Event text**: one-off events are **bold** and show their time range
  (`4-6pm`); repeating events (lessons, practice) are light and show their start
  time (`7:15pm`). All-day and multi-day events repeat on each day they cover.

Everything is grayscale, which suits a monochrome laser printer. Emoji in event
titles are dropped; characters outside Western European scripts print as `?`.

## Summaries with a local LLM (optional)

With `[summarize] enabled = true`, each day's events are condensed by a local
[Ollama](https://ollama.com) model (default `gemma4:26b` at `localhost:11434`;
an installed variant such as `gemma4:26b-mlx` is picked up automatically) into
a few short lines in the style of the hand-made calendar: routine drop-offs and
pick-ups disappear, titles shrink (`Charlie: After-school class:  Chess` becomes
`C: Chess 3:30p`), one-off events stay bold. The weekly detail list always keeps
the full event text.

The model only picks and shortens items; it refers to events by number and
calprint adds each item's time from the calendar itself, so every timed item on
the grid shows one (`3pm`, `7:15p`, `4-6pm`). All-day events and daily routines
(repeating every day, or five or more days a week) are shown without a time.

- **Few-shot examples** teach the style. Each pairs a past day's events with the
  cell text written for that day; the best source is your own old printouts.
  `calprint summarize 2026-06-01 2026-06-30 --all` prints past days' events in
  exactly the form the model sees, ready to paste into an examples file (see
  `examples.default.toml` for the format, including `(nothing)` for days that
  should stay blank). Point `summarize.examples_file` at it.
- `calprint summarize 2026-11-01 2026-11-30` previews summaries without
  rendering.
- `days = "overfull"` limits summaries to days that don't fit their cell.
- Results are cached in the state directory, so reprints are identical and fast.
  If Ollama is down or a reply is unusable, that day prints its full event text.

## Development

```sh
cargo test
cargo run -- sample --out-dir /tmp
```

| File               | Purpose                                                 |
|--------------------|---------------------------------------------------------|
| `src/config.rs`    | TOML config                                             |
| `src/ics.rs`       | iCal parsing; RRULE/EXDATE/RDATE/RECURRENCE-ID expansion |
| `src/google.rs`    | Google Calendar API + OAuth (loopback, PKCE)            |
| `src/render.rs`    | Month and week page layouts                             |
| `src/pdf.rs`       | Minimal PDF canvas (embedded Roboto, Helvetica, QR)     |
| `src/schedule.rs`  | What's due, catch-up window, printed-state              |
| `src/printer.rs`   | `lp` / `lpstat`                                         |
| `src/launchd.rs`   | LaunchAgent plist install/uninstall                     |
| `src/summarize.rs` | Ollama day summaries with few-shot examples and cache   |
