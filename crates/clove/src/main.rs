//! `clove(1)` — control CLI for `cloved`.
//!
//! A thin client: hand-rolled arg parsing, one request per invocation over the
//! local API (unix socket), rendering the daemon's JSON (`--json` passes it
//! through). Commands: `status`, `list`, `watch`, `show`, `add`, `remove`,
//! `pause`, `resume`, `verify`, `peer`, `priorities`, `completions`.
//! `watch` is the live view — a repaint loop over the same renderers, not a
//! TUI framework (`docs/PHASE-F.md` §6).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clove_core::config::{Config, Defaults};
use clove_core::http::{self, Request};
use clove_core::json::{self, Value};
use i2pnet::api;

/// Cap on a response body read from the (trusted, local) daemon.
const MAX_RESPONSE_BODY: usize = 8 * 1024 * 1024;

/// How a command failed, mapped to an exit code.
enum Fail {
    /// Bad invocation (exit 2).
    Usage(String),
    /// The daemon could not be reached (exit 3).
    Unreachable(String),
    /// The daemon was reached but the operation failed (exit 1).
    Failed(String),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Failed(m)) => {
            eprintln!("clove: {m}");
            ExitCode::from(1)
        }
        Err(Fail::Usage(m)) => {
            eprintln!("clove: {m}");
            ExitCode::from(2)
        }
        Err(Fail::Unreachable(m)) => {
            eprintln!("clove: {m}");
            ExitCode::from(3)
        }
    }
}

fn run() -> Result<(), Fail> {
    let mut socket: Option<PathBuf> = None;
    let mut json = false;
    let mut command: Option<String> = None;
    let mut operands: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                let path = args
                    .next()
                    .ok_or_else(|| Fail::Usage("--socket needs a path".to_owned()))?;
                socket = Some(PathBuf::from(path));
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other if command.is_none() => command = Some(other.to_owned()),
            // After the command, collect positional operands and flags like
            // `--data` for the subcommand to interpret.
            other => operands.push(other.to_owned()),
        }
    }

    match command.as_deref() {
        Some("status") => cmd_status(socket, json),
        Some("list") => cmd_list(socket, json),
        Some("watch") => cmd_watch(socket, &operands),
        Some("add") => cmd_add(socket, &operands),
        Some("remove") => cmd_remove(socket, &operands),
        Some("show") => cmd_show(socket, json, &operands),
        Some("pause") => cmd_action(socket, &operands, "pause", "paused"),
        Some("resume") => cmd_action(socket, &operands, "resume", "resumed"),
        Some("verify") => cmd_verify(socket, &operands),
        Some("peer") => cmd_peer(socket, &operands),
        Some("priorities") => cmd_priorities(socket, &operands),
        Some("announce") => cmd_action(socket, &operands, "announce", "announcing"),
        Some("sequential") => cmd_sequential(socket, &operands),
        Some("completions") => cmd_completions(&operands),
        Some(other) => Err(Fail::Usage(format!(
            "unknown command {other:?} (try --help)"
        ))),
        None => Err(Fail::Usage("no command given (try --help)".to_owned())),
    }
}

fn print_help() {
    println!("usage: clove [--socket <path>] <command>");
    println!("commands:");
    println!("  status [--json]                daemon and router status");
    println!("  list [--json]                  hosted torrents");
    println!("  watch [--interval <secs>]      live view, repainted (Ctrl-C to quit)");
    println!("  show <info-hash> [--json]      one torrent in detail");
    println!("  add <file.torrent|magnet:…>    add a torrent");
    println!("  remove <info-hash> [--data]    remove a torrent (--data also deletes files)");
    println!("  pause <info-hash>              pause a torrent");
    println!("  resume <info-hash>             resume a torrent");
    println!("  verify <info-hash>             re-check data on disk");
    println!("  peer <info-hash> <b32-addr>    hand a running torrent a peer to dial");
    println!("  priorities <info-hash> <spec>  set per-file priorities (e.g. 1,0,2)");
    println!("  announce <info-hash>           re-announce to every tracker now");
    println!("  sequential <info-hash> on|off  pick pieces in order instead of rarest-first");
    println!("  completions <bash|zsh|fish>    print a shell completion script");
}

/// Extract the single required info-hash operand.
fn one_info_hash(operands: &[String]) -> Result<&str, Fail> {
    match operands {
        [ih] => Ok(ih),
        [] => Err(Fail::Usage("this command needs an info-hash".to_owned())),
        _ => Err(Fail::Usage("too many arguments".to_owned())),
    }
}

fn cmd_status(socket: Option<PathBuf>, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(socket)?;
    let body = request(&socket, &token, "GET", "/v1/status", &[])?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    print!("{}", render_object(&parse_body(&body)?));
    Ok(())
}

fn cmd_list(socket: Option<PathBuf>, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(socket)?;
    let body = request(&socket, &token, "GET", "/v1/torrents", &[])?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    print!("{}", render_torrents(&parse_body(&body)?));
    Ok(())
}

/// Default repaint interval for `clove watch`.
const WATCH_DEFAULT_SECS: u64 = 2;

/// Slowest repaint we accept, so a typo cannot wedge the view for an hour.
const WATCH_MAX_SECS: u64 = 3600;

/// The live view (`docs/PHASE-F.md` §6): re-fetch status + torrents on an
/// interval and repaint the same tables the one-shot commands print.
///
/// Deliberately *not* a TUI: no raw mode, no alternate screen, no framework —
/// two ANSI escapes (erase display, cursor home) and the existing renderers.
/// The terminal stays in its normal mode throughout, so Ctrl-C at any moment
/// leaves a sane terminal with nothing to restore.
fn cmd_watch(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let mut interval = WATCH_DEFAULT_SECS;
    let mut args = operands.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interval" => {
                let value = args.next().ok_or_else(|| {
                    Fail::Usage("--interval needs a number of seconds".to_owned())
                })?;
                interval = value
                    .parse::<u64>()
                    .ok()
                    .filter(|secs| (1..=WATCH_MAX_SECS).contains(secs))
                    .ok_or_else(|| {
                        Fail::Usage(format!("--interval must be 1..={WATCH_MAX_SECS} seconds"))
                    })?;
            }
            other => return Err(Fail::Usage(format!("unexpected argument {other:?}"))),
        }
    }

    let (socket, token) = resolve(socket)?;
    loop {
        let frame = watch_frame(&socket, &token, interval)?;
        // Erase the display and park the cursor at the top-left, then draw.
        // One write keeps the repaint from tearing on a slow terminal.
        print!("\x1b[2J\x1b[H{frame}");
        std::io::stdout()
            .flush()
            .map_err(|e| Fail::Failed(format!("writing to the terminal: {e}")))?;
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

/// One repaint's worth of text: the daemon summary line, then the torrents.
fn watch_frame(socket: &Path, token: &str, interval: u64) -> Result<String, Fail> {
    let status = parse_body(&request(socket, token, "GET", "/v1/status", &[])?)?;
    let torrents = parse_body(&request(socket, token, "GET", "/v1/torrents", &[])?)?;

    let router = status.get("router").and_then(Value::as_str).unwrap_or("-");
    let version = status.get("version").and_then(Value::as_str).unwrap_or("-");
    let count = status.get("torrents").and_then(Value::as_u64).unwrap_or(0);
    let uptime = status
        .get("uptime_secs")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "clove {version}  router: {router}  torrents: {count}  up: {}  (every {interval}s, Ctrl-C to quit)",
        human_duration(uptime)
    );
    out.push('\n');
    out.push_str(&render_torrents(&torrents));
    Ok(out)
}

/// Compact uptime: `9s`, `5m`, `3h12m`, `2d4h`.
fn human_duration(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, seconds) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn cmd_add(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let target = operands
        .first()
        .ok_or_else(|| Fail::Usage("add needs a .torrent file or magnet link".to_owned()))?;
    let (socket, token) = resolve(socket)?;
    let body = if target.starts_with("magnet:") {
        target.clone().into_bytes()
    } else {
        std::fs::read(target).map_err(|e| Fail::Failed(format!("reading {target}: {e}")))?
    };
    let reply = request(&socket, &token, "POST", "/v1/torrents", &body)?;
    let value = parse_body(&reply)?;
    match value.get("info_hash").and_then(Value::as_str) {
        Some(info_hash) => println!("added {info_hash}"),
        None => println!("{}", String::from_utf8_lossy(&reply).trim()),
    }
    Ok(())
}

fn cmd_remove(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let mut info_hash: Option<&str> = None;
    let mut delete_data = false;
    for op in operands {
        match op.as_str() {
            "--data" => delete_data = true,
            other if info_hash.is_none() => info_hash = Some(other),
            other => return Err(Fail::Usage(format!("unexpected argument {other:?}"))),
        }
    }
    let info_hash = info_hash.ok_or_else(|| Fail::Usage("remove needs an info-hash".to_owned()))?;
    let (socket, token) = resolve(socket)?;
    let target = if delete_data {
        format!("/v1/torrents/{info_hash}?data=1")
    } else {
        format!("/v1/torrents/{info_hash}")
    };
    request(&socket, &token, "DELETE", &target, &[])?;
    println!("removed {info_hash}");
    Ok(())
}

fn cmd_show(socket: Option<PathBuf>, json: bool, operands: &[String]) -> Result<(), Fail> {
    let info_hash = one_info_hash(operands)?;
    let (socket, token) = resolve(socket)?;
    let body = request(
        &socket,
        &token,
        "GET",
        &format!("/v1/torrents/{info_hash}"),
        &[],
    )?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    print!("{}", render_detail(&parse_body(&body)?));
    Ok(())
}

/// A `POST /v1/torrents/{ih}/{action}` with no body; prints `<done> <ih>`.
fn cmd_action(
    socket: Option<PathBuf>,
    operands: &[String],
    action: &str,
    done: &str,
) -> Result<(), Fail> {
    let info_hash = one_info_hash(operands)?;
    let (socket, token) = resolve(socket)?;
    request(
        &socket,
        &token,
        "POST",
        &format!("/v1/torrents/{info_hash}/{action}"),
        &[],
    )?;
    println!("{done} {info_hash}");
    Ok(())
}

fn cmd_verify(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let info_hash = one_info_hash(operands)?;
    let (socket, token) = resolve(socket)?;
    let reply = request(
        &socket,
        &token,
        "POST",
        &format!("/v1/torrents/{info_hash}/verify"),
        &[],
    )?;
    let verified = parse_body(&reply)?
        .get("verified")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("verified {verified} piece(s) for {info_hash}");
    Ok(())
}

fn cmd_peer(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let [info_hash, addr] = operands else {
        return Err(Fail::Usage(
            "peer needs <info-hash> and a b32 address".to_owned(),
        ));
    };
    let (socket, token) = resolve(socket)?;
    request(
        &socket,
        &token,
        "POST",
        &format!("/v1/torrents/{info_hash}/peers"),
        addr.as_bytes(),
    )?;
    println!("peer added to {info_hash}");
    Ok(())
}

fn cmd_priorities(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let [info_hash, spec] = operands else {
        return Err(Fail::Usage(
            "priorities needs <info-hash> and a spec like 1,0,2".to_owned(),
        ));
    };
    let (socket, token) = resolve(socket)?;
    request(
        &socket,
        &token,
        "PUT",
        &format!("/v1/torrents/{info_hash}/priorities"),
        spec.as_bytes(),
    )?;
    println!("set priorities for {info_hash}");
    Ok(())
}

fn cmd_sequential(socket: Option<PathBuf>, operands: &[String]) -> Result<(), Fail> {
    let [info_hash, setting] = operands else {
        return Err(Fail::Usage(
            "sequential needs <info-hash> and on or off".to_owned(),
        ));
    };
    let on = match setting.as_str() {
        "on" | "yes" | "true" => true,
        "off" | "no" | "false" => false,
        other => {
            return Err(Fail::Usage(format!("expected on or off, got {other:?}")));
        }
    };
    let (socket, token) = resolve(socket)?;
    request(
        &socket,
        &token,
        "PUT",
        &format!("/v1/torrents/{info_hash}/sequential"),
        if on {
            b"true".as_slice()
        } else {
            b"false".as_slice()
        },
    )?;
    println!(
        "{info_hash} now picks pieces {}",
        if on { "in order" } else { "rarest-first" }
    );
    Ok(())
}

/// Print a shell completion script (no daemon needed).
fn cmd_completions(operands: &[String]) -> Result<(), Fail> {
    let shell = operands
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("completions needs a shell: bash, zsh, or fish".to_owned()))?;
    let script = match shell {
        "bash" => include_str!("completions/clove.bash"),
        "zsh" => include_str!("completions/_clove.zsh"),
        "fish" => include_str!("completions/clove.fish"),
        other => {
            return Err(Fail::Usage(format!(
                "unsupported shell {other:?} (bash, zsh, or fish)"
            )));
        }
    };
    print!("{script}");
    Ok(())
}

/// Render a torrent's detail: scalar fields, then a files table and trackers.
fn render_detail(value: &Value) -> String {
    let mut out = String::new();
    for key in [
        "name",
        "info_hash",
        "size",
        "pieces",
        "have",
        "progress",
        "state",
        "private",
    ] {
        let Some(field) = value.get(key) else {
            continue;
        };
        let rendered = match key {
            "size" => field.as_u64().map_or_else(|| field.to_line(), human_size),
            "progress" => field
                .as_f64()
                .map_or_else(|| field.to_line(), |p| format!("{:.0}%", p * 100.0)),
            _ => field.to_line(),
        };
        let _ = writeln!(out, "{key:<9}  {rendered}");
    }
    if let Some(files) = value.get("files").and_then(Value::as_array) {
        out.push_str("\nfiles:\n");
        let mut rows = Vec::with_capacity(files.len());
        for file in files {
            let size = file
                .get("length")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_owned(), human_size);
            let priority = file
                .get("priority")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_owned(), priority_name);
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned();
            rows.push(vec![size, priority, path]);
        }
        out.push_str(&align(&["SIZE", "PRIORITY", "PATH"], &rows));
    }
    if let Some(trackers) = value.get("trackers").and_then(Value::as_array)
        && !trackers.is_empty()
    {
        out.push_str("\ntrackers:\n");
        for tracker in trackers {
            if let Some(url) = tracker.as_str() {
                let _ = writeln!(out, "  {url}");
            }
        }
    }
    out
}

fn priority_name(priority: u64) -> String {
    match priority {
        0 => "skip".to_owned(),
        1 => "normal".to_owned(),
        2 => "high".to_owned(),
        other => other.to_string(),
    }
}

/// Parse a daemon response body as JSON.
fn parse_body(body: &[u8]) -> Result<Value, Fail> {
    let text = std::str::from_utf8(body)
        .map_err(|_| Fail::Failed("daemon response was not UTF-8".to_owned()))?;
    json::parse(text).map_err(|e| Fail::Failed(format!("parsing response: {e}")))
}

/// Render a JSON object as an aligned `key   value` table; a non-object value
/// is printed on one line.
fn render_object(value: &Value) -> String {
    let Some(fields) = value.as_object() else {
        return format!("{}\n", value.to_line());
    };
    let width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (key, val) in fields {
        let _ = writeln!(out, "{key:<width$}  {}", val.to_line());
    }
    out
}

/// Render the torrents array as an aligned table.
fn render_torrents(value: &Value) -> String {
    let Some(items) = value.as_array() else {
        return format!("{}\n", value.to_line());
    };
    if items.is_empty() {
        return "no torrents\n".to_owned();
    }
    let headers = ["PROGRESS", "STATE", "SIZE", "NAME", "INFO-HASH"];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(items.len());
    for item in items {
        let progress = item
            .get("progress")
            .and_then(Value::as_f64)
            .map_or_else(|| "-".to_owned(), |p| format!("{:.0}%", p * 100.0));
        let state = field_str(item, "state");
        let size = item
            .get("size")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".to_owned(), human_size);
        let name = field_str(item, "name");
        let hash = item.get("info_hash").and_then(Value::as_str).unwrap_or("-");
        let hash_short = hash.get(..12).unwrap_or(hash).to_owned();
        rows.push(vec![progress, state, size, name, hash_short]);
    }
    align(&headers, &rows)
}

fn field_str(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_owned()
}

/// Left-align columns to the widest cell (header included); the last column is
/// not padded.
fn align(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let mut out = String::new();
    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    write_row(&mut out, &header_cells, &widths);
    for row in rows {
        write_row(&mut out, row, &widths);
    }
    out
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let last = cells.len().saturating_sub(1);
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        if i == last {
            out.push_str(cell);
        } else {
            let _ = write!(out, "{cell:<width$}", width = widths[i]);
        }
    }
    out.push('\n');
}

/// Human-readable byte size (powers of 1024).
#[allow(
    clippy::cast_precision_loss,
    reason = "display only; exact precision is not required"
)]
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// Resolve the API socket path and token: an explicit `--socket` wins,
/// otherwise the config default. The token always comes from the data dir.
fn resolve(socket: Option<PathBuf>) -> Result<(PathBuf, String), Fail> {
    let defaults = Defaults::from_env().map_err(|e| Fail::Failed(e.to_string()))?;
    let config = Config::parse("", &defaults).map_err(|e| Fail::Failed(e.to_string()))?;
    let socket = socket.unwrap_or(config.api_socket);
    let token_path = config.data_dir.join("token");
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| {
            Fail::Failed(format!(
                "reading API token {}: {e} (has cloved run?)",
                token_path.display()
            ))
        })?
        .trim()
        .to_owned();
    Ok((socket, token))
}

/// Send one request and return the response body, mapping transport and HTTP
/// errors to the right failure kind.
fn request(
    socket: &Path,
    token: &str,
    method: &str,
    target: &str,
    body: &[u8],
) -> Result<Vec<u8>, Fail> {
    let mut stream = api::connect_unix(socket).map_err(|e| {
        Fail::Unreachable(format!(
            "cannot reach cloved at {} ({e}); is it running?",
            socket.display()
        ))
    })?;

    let req = Request {
        method,
        target,
        host: "clove",
        headers: &[("X-Clove-Token", token)],
        body,
    };
    stream
        .write_all(&req.encode())
        .map_err(|e| Fail::Unreachable(format!("sending request: {e}")))?;

    let response = http::read_response(&mut stream, MAX_RESPONSE_BODY)
        .map_err(|e| Fail::Failed(format!("reading response: {e}")))?;

    if response.status == 401 {
        return Err(Fail::Failed(
            "unauthorized: bad or missing API token".to_owned(),
        ));
    }
    if !(200..300).contains(&response.status) {
        let detail = String::from_utf8_lossy(&response.body);
        return Err(Fail::Failed(format!(
            "daemon returned {} — {}",
            response.status,
            detail.trim()
        )));
    }
    Ok(response.body)
}
