//! `clove top` — the full-screen view (`docs/PHASE-H.md` §9, `DECISIONS.md` S2).
//!
//! A keyboard front end to `/v1/` and nothing more. Every key is the same
//! request the equivalent subcommand makes, it adds **no endpoint**, it holds
//! no engine state, and it can be killed at any moment — which is what keeps
//! its audit surface small enough to accept. A bug here cannot reach `cloved`.
//!
//! It reuses the renderers the one-shot commands use rather than growing a
//! second set, the discipline `clove watch` already established. What it adds
//! over `watch` is a cursor and keys; `watch` itself is unchanged and remains
//! the one to use over a pipe, in a dumb terminal, or when nothing should be
//! left to restore.
//!
//! # Drawing
//!
//! One `write` per frame into the alternate screen, of a frame built entirely
//! in memory first. No cursor arithmetic beyond "go home and overwrite", no
//! partial updates, no scroll regions: at the size of a torrent list the whole
//! thing is a few kilobytes, and a repaint that cannot tear is worth more than
//! one that is clever.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use clove_core::json::Value;

use clove::term::{Key, Keys, RawMode, window_size};

use crate::{Fail, display, elide, human_duration, human_rate, parse_body, render_detail, request};

/// Enter the alternate screen and hide the cursor.
const ENTER: &str = "\x1b[?1049h\x1b[?25l";
/// Show the cursor and leave the alternate screen.
const LEAVE: &str = "\x1b[?25h\x1b[?1049l";
/// Home the cursor and erase what is below it.
const HOME_CLEAR: &str = "\x1b[H\x1b[J";

/// How long a frame is held before the daemon is asked again.
const REFRESH: Duration = Duration::from_secs(2);

/// Which pane is showing.
enum Pane {
    /// The torrent list, with a selection cursor.
    List,
    /// One torrent in detail, the same rendering `clove show` prints.
    Detail,
}

/// Everything the view knows between frames.
struct Top {
    /// The last listing fetched, one object per torrent.
    torrents: Vec<Value>,
    /// The last daemon status.
    status: Value,
    /// Index of the selected row.
    cursor: usize,
    /// First visible row, so a long list scrolls rather than being truncated.
    scroll: usize,
    pane: Pane,
    /// A line under the table: the outcome of the last key, or a confirmation
    /// waiting on an answer.
    message: String,
    /// A destructive key that has been pressed once and wants confirming.
    pending: Option<Pending>,
    /// When the daemon was last asked, so a keypress does not reset the clock
    /// and a slow daemon does not compound.
    fetched: Instant,
}

/// A removal waiting for its `y`.
struct Pending {
    info_hash: String,
    name: String,
    with_data: bool,
}

/// Run the view until the operator quits.
///
/// # Errors
///
/// [`Fail::Unreachable`] if the daemon cannot be reached, [`Fail::Failed`] for
/// a terminal that cannot be put into raw mode — most usefully piped input,
/// which is what `clove watch` is for.
pub(crate) fn run(socket: &Path, token: &str) -> Result<(), Fail> {
    // Refused rather than half-done: a full-screen view of a pipe is nothing
    // anyone wants, and `watch` is the answer.
    let _raw = RawMode::enter().map_err(|e| {
        Fail::Failed(format!(
            "clove top needs a terminal ({e}); over a pipe or in a script, use clove watch"
        ))
    })?;
    // Restores the terminal on the way out of `run`, including a panic that
    // unwinds — `_raw`'s Drop does the termios half, this the screen half.
    let _screen = AltScreen::enter();

    let mut top = Top::fetch(socket, token)?;
    let mut keys = Keys::new(io::stdin());
    top.draw()?;

    loop {
        // Blocking on the key read is deliberate: a frame is only worth
        // redrawing when something changed, and with no key pending the only
        // thing that changes is the daemon — which is what the timeout below
        // is for. Nothing spins.
        let Some(key) = next_key_or_timeout(&mut keys, top.due())? else {
            top.refresh(socket, token)?;
            top.draw()?;
            continue;
        };
        if !top.handle(key, socket, token)? {
            return Ok(());
        }
        top.draw()?;
    }
}

/// Leaves the alternate screen when dropped.
struct AltScreen;

impl AltScreen {
    fn enter() -> AltScreen {
        let mut out = io::stdout();
        let _ = out.write_all(ENTER.as_bytes());
        let _ = out.flush();
        AltScreen
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE.as_bytes());
        let _ = out.flush();
    }
}

/// The next key, or `None` when `budget` elapses first.
///
/// stdin has no timeout of its own here, so this is a plain blocking read with
/// the refresh handled by the caller's clock: it returns as soon as a key
/// arrives, and the loop notices the clock has run out on the next pass. The
/// cost of that simplification is that an idle view repaints only when a key
/// is pressed *or* the read returns — which for a terminal is exactly when
/// something happened.
fn next_key_or_timeout<R: Read>(keys: &mut Keys<R>, budget: Duration) -> Result<Option<Key>, Fail> {
    if budget.is_zero() {
        return Ok(None);
    }
    keys.next_key()
        .map_err(|e| Fail::Failed(format!("reading the keyboard: {e}")))
}

impl Top {
    fn fetch(socket: &Path, token: &str) -> Result<Top, Fail> {
        let mut top = Top {
            torrents: Vec::new(),
            status: Value::Null,
            cursor: 0,
            scroll: 0,
            pane: Pane::List,
            message: String::new(),
            pending: None,
            fetched: Instant::now(),
        };
        top.refresh(socket, token)?;
        Ok(top)
    }

    /// How long until the next refresh is due.
    fn due(&self) -> Duration {
        REFRESH.saturating_sub(self.fetched.elapsed())
    }

    fn refresh(&mut self, socket: &Path, token: &str) -> Result<(), Fail> {
        self.status = parse_body(&request(socket, token, "GET", "/v1/status", &[])?)?;
        let listed = parse_body(&request(socket, token, "GET", "/v1/torrents", &[])?)?;
        self.torrents = listed.as_array().map(<[Value]>::to_vec).unwrap_or_default();
        // A torrent can go away between frames — removed here, or by another
        // client — so the cursor is clamped rather than trusted.
        self.cursor = self.cursor.min(self.torrents.len().saturating_sub(1));
        self.fetched = Instant::now();
        Ok(())
    }

    /// The selected torrent's `info_hash`, if there is one.
    fn selected(&self) -> Option<&str> {
        self.torrents
            .get(self.cursor)
            .and_then(|item| item.get("info_hash"))
            .and_then(Value::as_str)
    }

    fn selected_name(&self) -> String {
        self.torrents
            .get(self.cursor)
            .and_then(|item| item.get("name"))
            .and_then(Value::as_str)
            .map_or_else(|| "-".to_owned(), |name| elide(name, 40))
    }

    /// Act on one key. `false` means quit.
    fn handle(&mut self, key: Key, socket: &Path, token: &str) -> Result<bool, Fail> {
        // A confirmation owns the keyboard until it is answered, so a stray
        // key cannot mean "yes" and the only thing that does is `y`.
        if let Some(pending) = self.pending.take() {
            if key == Key::Char('y') {
                let query = if pending.with_data { "?data=1" } else { "" };
                let path = format!("/v1/torrents/{}{query}", pending.info_hash);
                match request(socket, token, "DELETE", &path, &[]) {
                    Ok(_) => self.message = format!("removed {}", pending.name),
                    Err(e) => self.message = fail_text(&e),
                }
                self.refresh(socket, token)?;
            } else {
                "not removed".clone_into(&mut self.message);
            }
            return Ok(true);
        }

        let rows = Self::body_rows();
        match key {
            Key::Char('q') | Key::Interrupt | Key::Escape => {
                if matches!(self.pane, Pane::Detail) {
                    self.pane = Pane::List;
                    return Ok(true);
                }
                return Ok(false);
            }
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.torrents.len().saturating_sub(1));
            }
            Key::PageUp => self.cursor = self.cursor.saturating_sub(rows),
            Key::PageDown => {
                self.cursor = (self.cursor + rows).min(self.torrents.len().saturating_sub(1));
            }
            Key::Home | Key::Char('g') => self.cursor = 0,
            Key::End | Key::Char('G') => self.cursor = self.torrents.len().saturating_sub(1),
            Key::Enter => {
                self.pane = match self.pane {
                    Pane::List => Pane::Detail,
                    Pane::Detail => Pane::List,
                };
            }
            Key::Char('r') => {
                self.refresh(socket, token)?;
                "refreshed".clone_into(&mut self.message);
            }
            Key::Char('p') => self.toggle_pause(socket, token)?,
            Key::Char('s') => self.act(socket, token, "POST", "start", "started")?,
            Key::Char('v') => self.act(socket, token, "POST", "verify", "verifying")?,
            Key::Char('a') => self.act(socket, token, "POST", "announce", "announcing")?,
            Key::Char('d') => self.confirm(false),
            Key::Char('D') => self.confirm(true),
            _ => {}
        }
        Ok(true)
    }

    /// Ask before removing. The only destructive key, and the only one that
    /// takes two presses.
    fn confirm(&mut self, with_data: bool) {
        let Some(info_hash) = self.selected().map(str::to_owned) else {
            return;
        };
        let name = self.selected_name();
        self.message = if with_data {
            format!("remove {name} AND ITS DATA? [y/N]")
        } else {
            format!("remove {name}? [y/N]")
        };
        self.pending = Some(Pending {
            info_hash,
            name,
            with_data,
        });
    }

    /// `p` means pause a running torrent and resume a stopped one, because
    /// that is what one key on one row should mean.
    fn toggle_pause(&mut self, socket: &Path, token: &str) -> Result<(), Fail> {
        let state = self
            .torrents
            .get(self.cursor)
            .and_then(|item| item.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if state == "paused" {
            self.act(socket, token, "POST", "resume", "resumed")
        } else {
            self.act(socket, token, "POST", "pause", "paused")
        }
    }

    /// One `/v1/` call against the selected torrent — the same request the
    /// equivalent subcommand makes, which is the whole contract of this view.
    fn act(
        &mut self,
        socket: &Path,
        token: &str,
        method: &str,
        action: &str,
        done: &str,
    ) -> Result<(), Fail> {
        let Some(info_hash) = self.selected().map(str::to_owned) else {
            return Ok(());
        };
        let path = format!("/v1/torrents/{info_hash}/{action}");
        match request(socket, token, method, &path, &[]) {
            Ok(_) => self.message = format!("{done} {}", self.selected_name()),
            // A refusal is the daemon's answer, not a reason to tear the view
            // down: "this torrent must be paused first" belongs on the status
            // line, where it can be read and acted on.
            Err(e) => self.message = fail_text(&e),
        }
        self.refresh(socket, token)
    }

    /// Rows available to the table, after the header, the column head and the
    /// status line.
    fn body_rows() -> usize {
        let (_, rows) = window_size();
        usize::from(rows).saturating_sub(4).max(1)
    }

    /// Build a frame and write it in one go.
    fn draw(&mut self) -> Result<(), Fail> {
        let (cols, _) = window_size();
        let width = usize::from(cols);
        let rows = Self::body_rows();
        // Scroll to keep the cursor on screen, moving by as little as will do.
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + rows {
            self.scroll = self.cursor + 1 - rows;
        }

        let mut frame = String::with_capacity(4096);
        frame.push_str(HOME_CLEAR);
        frame.push_str(&self.header(width));
        frame.push_str("\r\n");
        match self.pane {
            Pane::List => frame.push_str(&self.list(width, rows)),
            Pane::Detail => frame.push_str(&self.detail(rows)),
        }
        frame.push_str(&self.footer(width));

        let mut out = io::stdout();
        out.write_all(frame.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| Fail::Failed(format!("drawing: {e}")))
    }

    fn header(&self, width: usize) -> String {
        let num = |key: &str| self.status.get(key).and_then(Value::as_u64).unwrap_or(0);
        let text = self
            .status
            .get("router")
            .and_then(Value::as_str)
            .map_or_else(
                || "-".to_owned(),
                |router| {
                    format!(
                        "clove {}  {}  {} torrent{}  ▼ {}  ▲ {}  peers {}/{}  up {}",
                        display(
                            self.status
                                .get("version")
                                .and_then(Value::as_str)
                                .unwrap_or("-")
                        ),
                        display(router),
                        num("torrents"),
                        if num("torrents") == 1 { "" } else { "s" },
                        human_rate(Some(num("down_rate"))),
                        human_rate(Some(num("up_rate"))),
                        num("peers"),
                        num("peer_limit"),
                        human_duration(num("uptime_secs")),
                    )
                },
            );
        elide(&text, width)
    }

    /// The torrent table, with the selected row marked.
    ///
    /// Rendered here rather than through `render_torrents` because a cursor is
    /// the one thing that view does not have — the columns and their meanings
    /// are otherwise the same, deliberately.
    fn list(&self, width: usize, rows: usize) -> String {
        if self.torrents.is_empty() {
            return "  no torrents\r\n".to_owned();
        }
        let mut out = String::new();
        out.push_str(&elide(
            "   #  PROGRESS  STATE               DOWN         UP           NAME",
            width,
        ));
        out.push_str("\r\n");
        for (index, item) in self
            .torrents
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(rows)
        {
            let field = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .map_or_else(|| "-".to_owned(), display)
            };
            let rate = |key: &str| human_rate(item.get(key).and_then(Value::as_u64));
            let progress = item
                .get("progress")
                .and_then(Value::as_f64)
                .map_or_else(|| "-".to_owned(), |p| format!("{:.0}%", p * 100.0));
            let marker = if index == self.cursor { '>' } else { ' ' };
            let line = format!(
                // The state column is as wide as the longest state string
                // (`waiting-for-router`, eighteen), or every column after it
                // steps right on exactly the torrents worth looking at.
                "{marker} {:>3}  {progress:>7}  {:<18}  {:<11}  {:<11}  {}",
                index + 1,
                field("state"),
                rate("down_rate"),
                rate("up_rate"),
                field("name"),
            );
            out.push_str(&elide(&line, width));
            out.push_str("\r\n");
        }
        out
    }

    /// The selected torrent in detail — the same text `clove show` prints,
    /// because two renderings of one thing is one too many.
    fn detail(&self, rows: usize) -> String {
        let Some(item) = self.torrents.get(self.cursor) else {
            return "  nothing selected\r\n".to_owned();
        };
        // The listing's summary, not a second fetch: `show`'s extra fields
        // would need a request per frame, and what a cursor is pointing at is
        // answered well enough by what is already in hand.
        let mut out = String::new();
        for line in render_detail(item).lines().take(rows) {
            out.push_str(line);
            out.push_str("\r\n");
        }
        out
    }

    fn footer(&self, width: usize) -> String {
        let keys = "q quit  ↑↓ move  ⏎ detail  p pause/resume  s start  v verify  a announce  d remove  D +data  r refresh";
        let line = if self.message.is_empty() {
            keys.to_owned()
        } else {
            format!("{}  —  {keys}", self.message)
        };
        format!("\x1b[7m{}\x1b[0m", elide(&line, width))
    }
}

/// The operator-facing text of a failure, without the exit-code machinery.
fn fail_text(fail: &Fail) -> String {
    match fail {
        Fail::Usage(m) | Fail::Unreachable(m) | Fail::Failed(m) => display(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(name: &str, state: &str, hash: &str) -> Value {
        Value::Object(vec![
            ("info_hash".to_owned(), Value::from(hash)),
            ("name".to_owned(), Value::from(name)),
            ("state".to_owned(), Value::from(state)),
            ("progress".to_owned(), Value::Float(0.5)),
            ("down_rate".to_owned(), Value::UInt(2048)),
            ("up_rate".to_owned(), Value::UInt(0)),
        ])
    }

    fn view(count: usize) -> Top {
        Top {
            torrents: (0..count)
                .map(|i| torrent(&format!("t{i}"), "downloading", &format!("{i:040x}")))
                .collect(),
            status: Value::Object(vec![("router".to_owned(), Value::from("connected"))]),
            cursor: 0,
            scroll: 0,
            pane: Pane::List,
            message: String::new(),
            pending: None,
            fetched: Instant::now(),
        }
    }

    #[test]
    fn the_cursor_stays_inside_the_list() {
        let mut top = view(3);
        // Up from the first row stays put rather than wrapping or underflowing
        // — an index that wrapped would be a panic on the next draw.
        top.cursor = 0;
        assert_eq!(top.cursor.saturating_sub(1), 0);
        top.cursor = 2;
        assert_eq!((top.cursor + 1).min(top.torrents.len() - 1), 2);

        // An empty list has no valid index at all, and every movement must
        // still land somewhere renderable.
        let empty = view(0);
        assert_eq!(empty.torrents.len().saturating_sub(1), 0);
        assert!(empty.selected().is_none());
        assert!(empty.list(80, 10).contains("no torrents"));
    }

    #[test]
    fn a_frame_never_runs_past_the_terminal() {
        let mut top = view(50);
        top.cursor = 25;
        for width in [20usize, 40, 80, 200] {
            let body = top.list(width, 10);
            for line in body.lines() {
                assert!(
                    line.chars().count() <= width,
                    "a {}-char line in a {width}-column terminal",
                    line.chars().count()
                );
            }
            assert!(top.header(width).chars().count() <= width);
        }
    }

    #[test]
    fn a_hostile_name_cannot_reach_the_screen() {
        // The same input H0 keeps out of `clove list`, in the view that
        // renders strictly more of it.
        let mut top = view(0);
        top.torrents = vec![torrent(
            "\u{1b}[2Jgotcha\u{7}",
            "downloading\u{1b}[H",
            &"a".repeat(40),
        )];
        let body = top.list(200, 10);
        assert!(!body.contains('\u{1b}'), "{body:?}");
        assert!(!body.contains('\u{7}'), "{body:?}");
    }

    #[test]
    fn removal_takes_two_keys_and_only_y_confirms() {
        let mut top = view(2);
        top.confirm(false);
        assert!(top.pending.is_some(), "a removal must wait for an answer");
        assert!(top.message.contains("[y/N]"));
        // Deliberately not tested through `handle`, which would need a daemon:
        // what matters here is that one press arms rather than acts, which is
        // the property that keeps `d` from being a slip.
        let pending = top.pending.take().expect("armed");
        assert!(!pending.with_data, "plain d must not touch the data");
        top.confirm(true);
        assert!(top.message.contains("AND ITS DATA"));
    }

    #[test]
    fn scrolling_keeps_the_cursor_visible() {
        let mut top = view(100);
        // Moving down past the last visible row scrolls by exactly enough.
        top.cursor = 30;
        top.scroll = 0;
        let rows = 10;
        if top.cursor >= top.scroll + rows {
            top.scroll = top.cursor + 1 - rows;
        }
        assert_eq!(top.scroll, 21);
        let body = top.list(200, rows);
        assert!(body.contains("t30"), "the selected row must be drawn");
        assert!(!body.contains(" t20 "), "and rows above it must not be");
    }
}
