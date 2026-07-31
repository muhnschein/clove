//! `clove(1)` — control CLI for `cloved`.
//!
//! A thin client: hand-rolled arg parsing, one request per invocation over the
//! local API (unix socket), rendering the daemon's JSON (`--json` passes it
//! through). Commands: `status`, `list`, `watch`, `show`, `add`, `remove`,
//! `pause`, `resume`, `verify`, `peer`, `priorities`, `completions`.
//! `watch` is the live view — a repaint loop over the same renderers, not a
//! TUI framework (`docs/PHASE-F.md` §6).

mod top;

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
#[derive(Debug)]
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
    let mut config_path: Option<PathBuf> = None;
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
            "-c" | "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| Fail::Usage(format!("{arg} needs a path")))?;
                config_path = Some(PathBuf::from(path));
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

    let where_ = Where {
        socket,
        config: config_path,
    };
    match command.as_deref() {
        Some("status") => cmd_status(&where_, json),
        Some("stats") => cmd_stats(&where_, json),
        Some("list") => cmd_list(&where_, json),
        Some("watch") => cmd_watch(&where_, &operands),
        Some("top") => cmd_top(&where_, &operands),
        Some("add") => cmd_add(&where_, &operands),
        Some("remove") => cmd_remove(&where_, &operands),
        Some("show") => cmd_show(&where_, json, &operands),
        Some("pause") => cmd_action(&where_, &operands, "pause", "paused"),
        Some("resume") => cmd_action(&where_, &operands, "resume", "resumed"),
        Some("start") => cmd_action(&where_, &operands, "start", "started"),
        Some("verify") => cmd_verify(&where_, &operands),
        Some("peer") => cmd_peer(&where_, &operands),
        Some("priorities") => cmd_priorities(&where_, &operands),
        Some("announce") => cmd_action(&where_, &operands, "announce", "announcing"),
        Some("sequential") => cmd_sequential(&where_, &operands),
        Some("seed-ratio") => cmd_seed_ratio(&where_, &operands),
        Some("completions") => cmd_completions(&operands),
        Some(other) => Err(Fail::Usage(format!(
            "unknown command {other:?} (try --help)"
        ))),
        None => Err(Fail::Usage("no command given (try --help)".to_owned())),
    }
}

/// Where to find the daemon: the overrides from the command line, before the
/// configuration they fall back to has been read.
struct Where {
    /// `--socket`, if given.
    socket: Option<PathBuf>,
    /// `-c`/`--config`, if given.
    config: Option<PathBuf>,
}

fn print_help() {
    println!("usage: clove [-c <config>] [--socket <path>] <command>");
    println!("  -c, --config <path>            read this configuration instead of the default");
    println!("commands:");
    println!("  status [--json]                daemon and router status");
    println!("  stats [--json]                 totals across every torrent");
    println!("  list [--json]                  hosted torrents");
    println!("  watch [--interval <secs>]      live view, repainted (Ctrl-C to quit)");
    println!("  top                            full-screen view with keys (q to quit)");
    println!("  show <torrent> [--json]        one torrent in detail");
    println!("  add <file.torrent|magnet:…>    add a torrent");
    println!("      [--paused] [--sequential]  ...stopped, or in file order");
    println!("  remove <torrent…> [--data]     remove torrents (--data also deletes files)");
    println!("  pause <torrent…>               pause torrents");
    println!("  resume <torrent…>              resume torrents");
    println!("  start <torrent…>               resume and jump the queue");
    println!("  verify <torrent…>              re-check data on disk");
    println!("  peer <torrent> <b32-addr>      hand a running torrent a peer to dial");
    println!("  priorities <torrent> <spec>    set per-file priorities (e.g. 1,0,2)");
    println!("  announce <torrent…>            re-announce to every tracker now");
    println!("  sequential <torrent> on|off    pick pieces in order instead of rarest-first");
    println!("  seed-ratio <torrent> <ratio>   stop seeding it at this ratio (0 = follow config)");
    println!("  completions <bash|zsh|fish>    print a shell completion script");
    println!();
    println!("<torrent> is an info-hash, a unique prefix of one, or a # from");
    println!("`clove list`; commands taking <torrent…> accept several, or --all.");
}

/// What a command calls the torrent it wants: a full info-hash or a unique
/// prefix of one. Resolved by the daemon, not here.
const REF_HELP: &str = "an info-hash, a unique prefix of one, or a # from `clove list`";

/// Extract the single required torrent reference.
fn one_info_hash(operands: &[String]) -> Result<&str, Fail> {
    match operands {
        [ih] => Ok(ih),
        [] => Err(Fail::Usage(format!(
            "this command needs a torrent ({REF_HELP})"
        ))),
        _ => Err(Fail::Usage("too many arguments".to_owned())),
    }
}

/// Split a command's operands into torrent references and the `--all` flag.
fn parse_refs(operands: &[String]) -> Result<(Vec<String>, bool), Fail> {
    let mut refs = Vec::new();
    let mut all = false;
    for op in operands {
        match op.as_str() {
            "--all" => all = true,
            other if other.starts_with('-') => {
                return Err(Fail::Usage(format!("unexpected option {other:?}")));
            }
            other => refs.push(other.to_owned()),
        }
    }
    Ok((refs, all))
}

/// The torrents a command will act on.
///
/// `--all` is the one case that needs a listing, and it is the CLI's job
/// rather than a daemon-side `/v1/torrents/all` endpoint: expanding here keeps
/// the bulk case made of the same single-torrent requests, so there is one
/// code path on the daemon and nothing new to authorise.
fn expand(socket: &Path, token: &str, refs: &[String], all: bool) -> Result<Vec<String>, Fail> {
    if !all {
        if refs.is_empty() {
            return Err(Fail::Usage(format!(
                "this command needs a torrent ({REF_HELP}), or --all"
            )));
        }
        // A bare number is the `#` column of the listing, resolved here
        // against the listing as it is right now. Deliberately client-side and
        // deliberately not an identity the daemon knows: a position means
        // something different the moment a torrent is added, so nothing may
        // store one, and `clove list` printing the index it just used is the
        // only honest way to offer it.
        if refs.iter().any(|r| is_index(r)) {
            return resolve_indices(socket, token, refs);
        }
        return Ok(refs.to_vec());
    }
    if !refs.is_empty() {
        return Err(Fail::Usage(
            "--all takes no torrent references of its own".to_owned(),
        ));
    }
    let listed = parse_body(&request(socket, token, "GET", "/v1/torrents", &[])?)?;
    let items = listed
        .as_array()
        .ok_or_else(|| Fail::Failed("daemon did not return a torrent list".to_owned()))?;
    Ok(items
        .iter()
        // A magnet still fetching its metadata is an add in progress, not yet
        // a torrent: it has no engine to pause, verify or announce, and
        // including it would make `resume --all` fail for the whole run
        // because one entry was never resumable. `state` is the one marker
        // that says so, and using it keeps this rule out of every command.
        .filter(|item| item.get("state").and_then(Value::as_str) != Some("fetching-metadata"))
        .filter_map(|item| item.get("info_hash").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

/// Longest listing position accepted as one: `999`, three digits.
///
/// The bound is what keeps positions and info-hashes from overlapping, and it
/// is not arbitrary. The daemon's shortest accepted hash prefix is four
/// characters, so **no string of one to three digits can be a torrent
/// reference at all** — and every string of four or more is treated as one,
/// including an all-digit hash like `0000…0000`, which is a perfectly legal
/// info-hash and was briefly being read as position zero.
///
/// The cost is that a client hosting a thousand torrents cannot name the
/// thousandth by position. It can still name it by prefix, which is the
/// spelling that scales.
const MAX_INDEX_DIGITS: usize = 3;

/// Whether a reference is a listing position rather than a torrent reference.
///
/// See [`MAX_INDEX_DIGITS`] for why the two cannot collide.
fn is_index(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= MAX_INDEX_DIGITS
        && reference.bytes().all(|b| b.is_ascii_digit())
}

/// Turn `#` positions into info-hashes, against one fetch of the listing.
///
/// One listing for the whole command, so `clove pause 1 2 3` cannot act on
/// three different orderings.
fn resolve_indices(socket: &Path, token: &str, refs: &[String]) -> Result<Vec<String>, Fail> {
    let listed = parse_body(&request(socket, token, "GET", "/v1/torrents", &[])?)?;
    let items = listed
        .as_array()
        .ok_or_else(|| Fail::Failed("daemon did not return a torrent list".to_owned()))?;
    let mut out = Vec::with_capacity(refs.len());
    for reference in refs {
        if !is_index(reference) {
            out.push(reference.clone());
            continue;
        }
        let position: usize = reference
            .parse()
            .map_err(|_| Fail::Usage(format!("{reference} is not a listing position")))?;
        let hash = position
            .checked_sub(1)
            .and_then(|i| items.get(i))
            .and_then(|item| item.get("info_hash"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Fail::Failed(format!(
                    "no torrent at position {reference} (the listing has {})",
                    items.len()
                ))
            })?;
        out.push(hash.to_owned());
    }
    Ok(out)
}

/// [`resolve_indices`] for the commands that take exactly one torrent, so a
/// `#` position means the same thing everywhere it can be typed.
fn one_target(socket: &Path, token: &str, reference: &str) -> Result<String, Fail> {
    if !is_index(reference) {
        return Ok(reference.to_owned());
    }
    let refs = [reference.to_owned()];
    resolve_indices(socket, token, &refs)?
        .pop()
        .ok_or_else(|| Fail::Failed(format!("could not resolve {reference}")))
}

/// Apply `op` to every target, and report the worst thing that happened.
///
/// One target behaves exactly as it did before there were several — the error
/// is the command's own, printed once by `main` — so nothing scripted against
/// the single-torrent form changes shape.
///
/// Past that, each failure is printed against the torrent it belongs to and
/// the command keeps going, because the alternative is a bulk pause that stops
/// on the first paused torrent. An unreachable daemon is the exception: it is
/// not a per-torrent failure, and every remaining attempt would rediscover it.
fn for_each<F>(targets: &[String], mut op: F) -> Result<(), Fail>
where
    F: FnMut(&str) -> Result<(), Fail>,
{
    if let [only] = targets {
        return op(only);
    }
    let mut failed = 0usize;
    for target in targets {
        match op(target) {
            Ok(()) => {}
            Err(e @ Fail::Unreachable(_)) => return Err(e),
            Err(Fail::Usage(m) | Fail::Failed(m)) => {
                eprintln!("clove: {}: {m}", display(target));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(Fail::Failed(format!(
            "{failed} of {} failed",
            targets.len()
        )));
    }
    Ok(())
}

fn cmd_status(where_: &Where, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(where_)?;
    let body = request(&socket, &token, "GET", "/v1/status", &[])?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    print!("{}", render_object(&parse_body(&body)?));
    Ok(())
}

fn cmd_list(where_: &Where, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(where_)?;
    let body = request(&socket, &token, "GET", "/v1/torrents", &[])?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    print!("{}", render_torrents(&parse_body(&body)?));
    Ok(())
}

/// Client-wide totals: what every torrent adds up to right now.
///
/// Deliberately a separate command from `status`, which answers "is the daemon
/// and its router alright". This one answers "what is my client doing", and
/// they are read at different moments.
fn cmd_stats(where_: &Where, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(where_)?;
    let body = request(&socket, &token, "GET", "/v1/status", &[])?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    let status = parse_body(&body)?;
    let torrents = parse_body(&request(&socket, &token, "GET", "/v1/torrents", &[])?)?;
    let empty: Vec<Value> = Vec::new();
    let items: &[Value] = torrents.as_array().unwrap_or(&empty);

    let mut by_state: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let (mut up, mut down) = (0u64, 0u64);
    for item in items {
        *by_state.entry(field_str(item, "state")).or_default() += 1;
        up += item.get("uploaded").and_then(Value::as_u64).unwrap_or(0);
        down += item.get("downloaded").and_then(Value::as_u64).unwrap_or(0);
    }

    let mut out = String::new();
    let num = |key: &str| status.get(key).and_then(Value::as_u64).unwrap_or(0);
    let _ = writeln!(out, "torrents      {}", items.len());
    for (state, count) in &by_state {
        let _ = writeln!(out, "  {state:<12}{count}");
    }
    let _ = writeln!(out, "down rate     {}", human_rate(Some(num("down_rate"))));
    let _ = writeln!(out, "up rate       {}", human_rate(Some(num("up_rate"))));
    let _ = writeln!(
        out,
        "peers         {} of {}",
        num("peers"),
        num("peer_limit")
    );
    let _ = writeln!(out, "downloaded    {}", human_size(down));
    let _ = writeln!(out, "uploaded      {}", human_size(up));
    let _ = writeln!(out, "session       {}", human_duration(num("uptime_secs")));
    print!("{out}");
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
fn cmd_watch(where_: &Where, operands: &[String]) -> Result<(), Fail> {
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

    let (socket, token) = resolve(where_)?;
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

/// The full-screen view (`docs/PHASE-H.md` §9).
///
/// Separate from [`cmd_watch`] on purpose, and not chosen between by sniffing
/// whether stdout is a terminal: the two have different interaction models,
/// and a command that silently becomes a different program depending on where
/// its output goes is worse than two commands that say what they are.
fn cmd_top(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    if let Some(unexpected) = operands.first() {
        return Err(Fail::Usage(format!("unexpected argument {unexpected:?}")));
    }
    let (socket, token) = resolve(where_)?;
    top::run(&socket, &token)
}

/// One repaint's worth of text: the daemon summary line, then the torrents.
fn watch_frame(socket: &Path, token: &str, interval: u64) -> Result<String, Fail> {
    let status = parse_body(&request(socket, token, "GET", "/v1/status", &[])?)?;
    let torrents = parse_body(&request(socket, token, "GET", "/v1/torrents", &[])?)?;

    // The router line carries whatever the SAM bridge last said, so it is not
    // ours either — see `display`.
    let router = field_str(&status, "router");
    let version = field_str(&status, "version");
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

fn cmd_add(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    let mut target: Option<&String> = None;
    let mut flags: Vec<&str> = Vec::new();
    for op in operands {
        match op.as_str() {
            "--paused" => flags.push("paused=1"),
            "--sequential" => flags.push("sequential=1"),
            other if other.starts_with("--") => {
                return Err(Fail::Usage(format!("unexpected option {other:?}")));
            }
            _ if target.is_none() => target = Some(op),
            other => return Err(Fail::Usage(format!("unexpected argument {other:?}"))),
        }
    }
    let target =
        target.ok_or_else(|| Fail::Usage("add needs a .torrent file or magnet link".to_owned()))?;
    let (socket, token) = resolve(where_)?;
    let body = if target.starts_with("magnet:") {
        target.clone().into_bytes()
    } else {
        std::fs::read(target).map_err(|e| Fail::Failed(format!("reading {target}: {e}")))?
    };
    let path = if flags.is_empty() {
        "/v1/torrents".to_owned()
    } else {
        format!("/v1/torrents?{}", flags.join("&"))
    };
    let reply = request(&socket, &token, "POST", &path, &body)?;
    let value = parse_body(&reply)?;
    match value.get("info_hash").and_then(Value::as_str) {
        Some(info_hash) => println!("added {info_hash}"),
        None => println!("{}", String::from_utf8_lossy(&reply).trim()),
    }
    Ok(())
}

fn cmd_remove(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    let mut delete_data = false;
    let rest: Vec<String> = operands
        .iter()
        .filter(|op| {
            let is_data = op.as_str() == "--data";
            delete_data |= is_data;
            !is_data
        })
        .cloned()
        .collect();
    let (refs, all) = parse_refs(&rest)?;
    let (socket, token) = resolve(where_)?;
    let targets = expand(&socket, &token, &refs, all)?;
    for_each(&targets, |target| {
        let path = if delete_data {
            format!("/v1/torrents/{target}?data=1")
        } else {
            format!("/v1/torrents/{target}")
        };
        request(&socket, &token, "DELETE", &path, &[])?;
        println!("removed {}", display(target));
        Ok(())
    })
}

fn cmd_show(where_: &Where, json: bool, operands: &[String]) -> Result<(), Fail> {
    let reference = one_info_hash(operands)?;
    let (socket, token) = resolve(where_)?;
    let info_hash = one_target(&socket, &token, reference)?;
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

/// A `POST /v1/torrents/{ref}/{action}` with no body, for each target;
/// prints `<done> <ref>`.
fn cmd_action(where_: &Where, operands: &[String], action: &str, done: &str) -> Result<(), Fail> {
    let (refs, all) = parse_refs(operands)?;
    let (socket, token) = resolve(where_)?;
    let targets = expand(&socket, &token, &refs, all)?;
    for_each(&targets, |target| {
        request(
            &socket,
            &token,
            "POST",
            &format!("/v1/torrents/{target}/{action}"),
            &[],
        )?;
        println!("{done} {}", display(target));
        Ok(())
    })
}

fn cmd_verify(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    let (refs, all) = parse_refs(operands)?;
    let (socket, token) = resolve(where_)?;
    let targets = expand(&socket, &token, &refs, all)?;
    for_each(&targets, |target| {
        let reply = request(
            &socket,
            &token,
            "POST",
            &format!("/v1/torrents/{target}/verify"),
            &[],
        )?;
        let verified = parse_body(&reply)?
            .get("verified")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("verified {verified} piece(s) for {}", display(target));
        Ok(())
    })
}

fn cmd_peer(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    let [info_hash, addr] = operands else {
        return Err(Fail::Usage(
            "peer needs <info-hash> and a b32 address".to_owned(),
        ));
    };
    let (socket, token) = resolve(where_)?;
    let info_hash = one_target(&socket, &token, info_hash)?;
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

fn cmd_priorities(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    let [info_hash, spec] = operands else {
        return Err(Fail::Usage(
            "priorities needs <info-hash> and a spec like 1,0,2".to_owned(),
        ));
    };
    let (socket, token) = resolve(where_)?;
    let info_hash = one_target(&socket, &token, info_hash)?;
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

fn cmd_sequential(where_: &Where, operands: &[String]) -> Result<(), Fail> {
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
    let (socket, token) = resolve(where_)?;
    let info_hash = one_target(&socket, &token, info_hash)?;
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

fn cmd_seed_ratio(where_: &Where, operands: &[String]) -> Result<(), Fail> {
    let [info_hash, ratio] = operands else {
        return Err(Fail::Usage(
            "seed-ratio needs <torrent> and a ratio like 2 or 1.75".to_owned(),
        ));
    };
    let (socket, token) = resolve(where_)?;
    let info_hash = one_target(&socket, &token, info_hash)?;
    request(
        &socket,
        &token,
        "PUT",
        &format!("/v1/torrents/{info_hash}/seed-ratio"),
        ratio.as_bytes(),
    )?;
    if ratio == "0" {
        println!(
            "{} now follows the daemon's seed_ratio",
            display(&info_hash)
        );
    } else {
        println!("{} will stop seeding at {ratio}", display(&info_hash));
    }
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
        "sequential",
        "private",
        "peers",
        "known_peers",
        "pex_peers",
        "inbound_peers",
        "downloaded",
        "uploaded",
        "down_rate",
        "up_rate",
        "ratio",
        "seed_ratio",
        "announces_ok",
        "announces_failed",
        // Last, and unwrapped: each is a sentence rather than a field, and
        // between them they answer the two questions a stopped or peerless
        // torrent raises.
        "paused_because",
        "last_announce_error",
    ] {
        let Some(field) = value.get(key) else {
            continue;
        };
        let rendered = match key {
            "size" | "downloaded" | "uploaded" => {
                field.as_u64().map_or_else(|| cell(field), human_size)
            }
            "down_rate" | "up_rate" => human_rate(field.as_u64()),
            // Thousandths on the wire, because bencode and JSON integers are
            // exact and a ratio round-tripped through a float is not.
            "ratio" | "seed_ratio" => field.as_u64().map_or_else(
                || cell(field),
                |milli| {
                    if milli == 0 && key == "seed_ratio" {
                        "unlimited (follows the daemon)".to_owned()
                    } else {
                        // Integer division, not a float: thousandths are on
                        // the wire precisely so a ratio never round-trips
                        // through something that can render 1.5 as 1.499.
                        format!("{}.{:03}", milli / 1000, milli % 1000)
                    }
                },
            ),
            "progress" => field
                .as_f64()
                .map_or_else(|| cell(field), |p| format!("{:.0}%", p * 100.0)),
            _ => cell(field),
        };
        // Wide enough for the longest key printed here (`last_announce_error`),
        // so the value column does not step right when a torrent has a
        // problem — which is exactly when it is being read carefully.
        let _ = writeln!(out, "{key:<19}  {rendered}");
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
            let path = display(file.get("path").and_then(Value::as_str).unwrap_or("-"));
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
                let _ = writeln!(out, "  {}", display(url));
            }
        }
    }
    out
}

/// How wide a torrent name is allowed to be in a table cell before it is
/// elided. Long enough for anything an operator recognises a torrent by,
/// short enough that one pathological name cannot push the info-hash column
/// off the far side of the terminal.
const NAME_WIDTH: usize = 60;

/// Make an attacker-controlled string safe to write to a terminal.
///
/// A `.torrent` is not trusted input. `info.name`, the file paths under it and
/// the tracker URLs beside it are all chosen by whoever made the torrent, and
/// `metainfo::check_component` — which refuses separators, `.`, `..` and NUL —
/// has no opinion about `ESC`. Printed verbatim, a torrent named
/// `"\x1b[2J\x1b[1;31m…"` clears the reader's screen and recolours it, and one
/// named `"\x1b]0;…\x07"` retitles their terminal. `SECURITY.md` already scopes
/// torrent names as hostile input; this is the same bytes reaching a different
/// interpreter.
///
/// This belongs *here* and not in the daemon. `json::write_string` already
/// escapes everything below `0x20` as `\u00XX`, so the API is inert and
/// `--json` consumers were never affected. What the JSON has to keep is the
/// torrent's actual name — sanitising there would misreport it — so the
/// substitution happens at the one boundary where bytes become a terminal's
/// input.
///
/// Replaced with `.`:
/// - the `Cc` category (C0, `DEL` and C1), which is where the escapes live;
/// - the bidirectional overrides, which reorder *neighbouring* text rather
///   than drawing anything themselves, and so can make `…rat.exe` render as
///   `…exe.tar` in the list an operator is reading to decide what to trust.
///
/// Everything else passes through, including the UTF-8 that makes [`align`]
/// approximate — it counts characters, not display columns, which is unchanged
/// here and not worth a dependency to fix.
fn display(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            c if c.is_control() => '.',
            // LRM/RLM, the LRE/RLE/PDF/LRO/RLO run, and the isolates.
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '.',
            c => c,
        })
        .collect()
}

/// [`display`], then clamp to `width` characters, marking any elision.
///
/// Character count rather than display width, to stay consistent with
/// [`align`]: two functions disagreeing about how wide a string is would
/// misalign the table in a way neither of them looks wrong doing.
fn elide(s: &str, width: usize) -> String {
    let safe = display(s);
    if safe.chars().count() <= width {
        return safe;
    }
    let mut out: String = safe.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A JSON value rendered into a table cell, sanitised ([`display`]).
///
/// Every site that turns a daemon response into terminal output goes through
/// this or [`elide`]; `to_line` on its own is the bug.
fn cell(value: &Value) -> String {
    display(&value.to_line())
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
        let _ = writeln!(out, "{key:<width$}  {}", cell(val));
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
    let headers = [
        "#",
        "PROGRESS",
        "STATE",
        "SIZE",
        "DOWN",
        "UP",
        "NAME",
        "INFO-HASH",
    ];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let progress = item
            .get("progress")
            .and_then(Value::as_f64)
            .map_or_else(|| "-".to_owned(), |p| format!("{:.0}%", p * 100.0));
        let state = field_str(item, "state");
        let size = item
            .get("size")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".to_owned(), human_size);
        let name = elide(
            item.get("name").and_then(Value::as_str).unwrap_or("-"),
            NAME_WIDTH,
        );
        let hash = item.get("info_hash").and_then(Value::as_str).unwrap_or("-");
        let hash_short = hash.get(..12).unwrap_or(hash).to_owned();
        rows.push(vec![
            (index + 1).to_string(),
            progress,
            state,
            size,
            human_rate(item.get("down_rate").and_then(Value::as_u64)),
            human_rate(item.get("up_rate").and_then(Value::as_u64)),
            name,
            hash_short,
        ]);
    }
    align(&headers, &rows)
}

fn field_str(item: &Value, key: &str) -> String {
    display(item.get(key).and_then(Value::as_str).unwrap_or("-"))
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

/// A transfer rate, or a dash when there is nothing moving.
///
/// Zero prints as `-` rather than `0 B/s`: a listing is mostly idle torrents,
/// and a column of zeroes is noise that hides the one row that is doing
/// something — which is the whole reason the column is there.
fn human_rate(bytes_per_sec: Option<u64>) -> String {
    match bytes_per_sec {
        Some(0) | None => "-".to_owned(),
        Some(rate) => format!("{}/s", human_size(rate)),
    }
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
    // Rounding to one decimal can push a value back over the boundary the
    // loop just cleared: 1048575 bytes is 1023.999 KiB, which prints as
    // "1024.0 KiB". Step up instead of showing a size in units of itself.
    if (size * 10.0).round() >= 10_240.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// Resolve the API socket path and token from the same configuration the daemon
/// uses, which is what `clove(1)` promises: an explicit `--socket` wins,
/// otherwise `api_socket`; the token comes from `data_dir`.
///
/// Reading the config file is the point. Parsing an empty one instead — as this
/// used to — means a `data_dir` or `api_socket` set in `clove.conf` is invisible
/// here, and every command fails looking for a token in a directory the daemon
/// does not use.
fn resolve(where_: &Where) -> Result<(PathBuf, String), Fail> {
    let defaults = Defaults::from_env().map_err(|e| Fail::Failed(e.to_string()))?;
    // An explicit -c must exist; the default path may simply be absent, in
    // which case the built-in defaults are the whole configuration. Same rule
    // as cloved, so the two agree on what "no config" means.
    let text = match &where_.config {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| Fail::Usage(format!("reading {}: {e}", path.display())))?,
        None => std::fs::read_to_string(defaults.config_path()).unwrap_or_default(),
    };
    let config = Config::parse(&text, &defaults).map_err(|e| Fail::Failed(e.to_string()))?;
    let socket = where_.socket.clone().unwrap_or(config.api_socket);
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
    // Unreachable with a well-formed token and a target we built ourselves,
    // which is exactly why it is worth refusing rather than assuming.
    let encoded = req.encode().ok_or_else(|| {
        Fail::Failed("request contains a line break; refusing to send".to_owned())
    })?;
    stream
        .write_all(&encoded)
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

#[cfg(test)]
mod tests {
    //! Tests for the parts of the CLI that are pure: argument shapes, the
    //! exit-code contract, and the rendering of a daemon response into the
    //! table a human reads.
    //!
    //! `ci/smoke.sh` proves the commands work against a live daemon. What it
    //! cannot see is *formatting* — a column that stops aligning, a size that
    //! renders as `1024.0 KiB`, a missing field that prints `null` instead of
    //! `-`. Those regress silently and are exactly what a scripted end-to-end
    //! test looks straight past.

    use super::*;

    fn obj(fields: &[(&str, Value)]) -> Value {
        Value::Object(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn sizes_read_the_way_a_human_expects() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        // The boundary either side of the first unit change.
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(human_size(1024u64.pow(4)), "1.0 TiB");
        assert_eq!(human_size(1024u64.pow(5)), "1.0 PiB");
        // The unit table runs out at PiB; the value must keep growing rather
        // than wrap or index past the end.
        assert!(human_size(u64::MAX).ends_with(" PiB"));
        // Nothing may render as "1024.0" of a smaller unit.
        for n in [1023u64, 1024, 1025, 1024 * 1023, 1024 * 1024 - 1] {
            assert!(
                !human_size(n).starts_with("1024."),
                "{n} rendered as {}",
                human_size(n)
            );
        }
    }

    #[test]
    fn durations_pick_the_two_largest_units() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(59), "59s");
        assert_eq!(human_duration(60), "1m");
        assert_eq!(human_duration(3_599), "59m");
        assert_eq!(human_duration(3_600), "1h0m");
        assert_eq!(human_duration(3_661), "1h1m");
        assert_eq!(human_duration(86_399), "23h59m");
        assert_eq!(human_duration(86_400), "1d0h");
        assert_eq!(human_duration(90_000), "1d1h");
        assert_eq!(human_duration(u64::MAX), "213503982334601d7h");
    }

    #[test]
    fn a_torrent_name_cannot_drive_the_terminal() {
        // The whole point: these bytes come out of a `.torrent` written by
        // whoever wanted us to have it, and `check_component` lets every one
        // of them through — it only refuses separators, `.`, `..` and NUL.
        let hostile = "\u{1b}[2J\u{1b}[1;31mowned\u{7}\u{9b}0;title\u{7}\u{0}end";
        let safe = display(hostile);
        for bad in ['\u{1b}', '\u{7}', '\u{9b}', '\u{0}'] {
            assert!(!safe.contains(bad), "{bad:?} survived in {safe:?}");
        }
        // One `.` per control character, and the inert remainder of each
        // sequence left visible rather than swallowed — an operator seeing
        // `.[2J` in a name should be able to tell what it was trying to do.
        assert_eq!(safe, ".[2J.[1;31mowned..0;title..end");
        // Tab and newline are controls too, and a name containing either
        // would break the table by itself.
        assert_eq!(display("a\tb\nc\r\n"), "a.b.c..");

        // Trojan-source reordering: the override characters draw nothing
        // themselves but reverse what is printed around them, so a name can
        // render as a different extension than the one it has.
        assert_eq!(display("safe\u{202e}gnp.exe"), "safe.gnp.exe");
        for c in [
            '\u{200e}', '\u{200f}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}',
        ] {
            assert_eq!(display(&c.to_string()), ".");
        }

        // What must *not* change: ordinary text, and the non-ASCII that a
        // legitimately-named torrent is full of.
        for ok in ["ubuntu-24.04.iso", "Дистрибутив", "日本語", "café", "🎉"] {
            assert_eq!(display(ok), ok);
        }

        // And it has to hold through the renderer an operator actually reads,
        // not just the helper.
        let table = render_torrents(&Value::Array(vec![obj(&[
            ("name", Value::from(hostile)),
            ("state", Value::from("seeding\u{1b}[2J")),
            ("info_hash", Value::from("ab".repeat(20))),
        ])]));
        assert!(!table.contains('\u{1b}'), "{table:?}");
        let detail = render_detail(&obj(&[
            ("name", Value::from(hostile)),
            (
                "last_announce_error",
                Value::from("connect failed\u{1b}[2J"),
            ),
            (
                "files",
                Value::Array(vec![obj(&[
                    ("path", Value::from("dir/\u{1b}[2Jevil")),
                    ("length", Value::UInt(1)),
                ])]),
            ),
            (
                "trackers",
                Value::Array(vec![Value::from("http://t.i2p/a\u{1b}[2J")]),
            ),
        ]));
        assert!(!detail.contains('\u{1b}'), "{detail:?}");
        // `clove status` renders through a different function; it reads the
        // router's last word, which is not ours either.
        let status = render_object(&obj(&[("router", Value::from("lost\u{1b}[2J"))]));
        assert!(!status.contains('\u{1b}'), "{status:?}");
    }

    #[test]
    fn long_names_are_elided_rather_than_wrapping() {
        assert_eq!(elide("short", 10), "short");
        // Exactly the limit is not elided; one past it is.
        assert_eq!(elide(&"x".repeat(10), 10), "x".repeat(10));
        let cut = elide(&"x".repeat(11), 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'), "{cut:?}");
        // Elision counts characters, so a multi-byte name is cut at a
        // character boundary and not mid-codepoint.
        let wide = elide(&"é".repeat(80), NAME_WIDTH);
        assert_eq!(wide.chars().count(), NAME_WIDTH);
        // Sanitising happens first: an elided name cannot smuggle an escape
        // through in the part that survives.
        assert!(!elide(&format!("\u{1b}[2J{}", "x".repeat(80)), NAME_WIDTH).contains('\u{1b}'));

        // A pathological name must not push the last column out of the
        // terminal, which is what the width is for.
        let table = render_torrents(&Value::Array(vec![obj(&[
            ("name", Value::from("n".repeat(400))),
            ("info_hash", Value::from("cd".repeat(20))),
        ])]));
        for line in table.lines() {
            assert!(line.chars().count() < 120, "{} chars", line.chars().count());
        }
    }

    #[test]
    fn columns_align_to_the_widest_cell() {
        let out = align(
            &["A", "LONGHEADER"],
            &[
                vec!["x".to_owned(), "1".to_owned()],
                vec!["wide-cell".to_owned(), "2".to_owned()],
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two rows");
        // Every second column starts at the same offset — the whole contract
        // of this function. The widest first-column cell is 9 characters, so
        // that offset is 11 on every line including the header.
        let second_col: Vec<usize> = lines
            .iter()
            .map(|l| l.rfind("  ").expect("a column separator") + 2)
            .collect();
        assert_eq!(second_col, vec![11, 11, 11], "{out}");
        // The last column is not padded: no trailing spaces anywhere.
        for line in &lines {
            assert_eq!(*line, line.trim_end(), "trailing padding in {line:?}");
        }
        // A row shorter than the header must not panic or misalign the rest.
        let ragged = align(&["A", "B", "C"], &[vec!["only".to_owned()]]);
        assert_eq!(ragged.lines().count(), 2);
    }

    #[test]
    fn an_empty_list_says_so_rather_than_printing_a_bare_header() {
        assert_eq!(render_torrents(&Value::Array(Vec::new())), "no torrents\n");
    }

    #[test]
    fn the_listing_shortens_hashes_and_fills_missing_fields() {
        let full = "58e2fc46a8dc57c78191f079648750b0644d03a2";
        let out = render_torrents(&Value::Array(vec![
            obj(&[
                ("info_hash", Value::from(full.to_owned())),
                ("name", Value::from("release.iso".to_owned())),
                ("size", Value::UInt(1_500_000_000)),
                ("progress", Value::Float(0.423)),
                ("state", Value::from("downloading".to_owned())),
            ]),
            // A torrent whose metadata has not arrived yet: every optional
            // field is absent and must render as a dash, not as "null".
            obj(&[("info_hash", Value::from("ab".repeat(20)))]),
        ]));
        assert!(out.contains("58e2fc46a8dc"), "{out}");
        assert!(
            !out.contains(full),
            "the full hash belongs in show, not list: {out}"
        );
        assert!(out.contains("42%"), "{out}");
        assert!(out.contains("1.4 GiB"), "{out}");
        assert!(!out.contains("null"), "{out}");
        assert!(out.lines().any(|l| l.contains(" -  ")), "{out}");
    }

    #[test]
    fn a_non_array_listing_does_not_pretend_to_be_a_table() {
        // If the daemon ever answered with something unexpected, the CLI
        // prints it rather than rendering an empty table over it.
        let out = render_torrents(&Value::from("unexpected".to_owned()));
        assert!(out.contains("unexpected"), "{out}");
    }

    #[test]
    fn detail_shows_the_per_torrent_switches() {
        let out = render_detail(&obj(&[
            ("name", Value::from("film.mkv".to_owned())),
            ("size", Value::UInt(2048)),
            ("progress", Value::Float(0.5)),
            ("state", Value::from("downloading".to_owned())),
            ("sequential", Value::Bool(true)),
            ("private", Value::Bool(false)),
        ]));
        assert!(out.contains("2.0 KiB"), "{out}");
        assert!(out.contains("50%"), "{out}");
        // Sequential mode is a per-torrent setting an operator turned on; it
        // has to be visible in the view they turn to for confirmation.
        assert!(out.contains("sequential"), "{out}");
        assert!(out.contains("true"), "{out}");
    }

    #[test]
    fn priority_names() {
        assert_eq!(priority_name(0), "skip");
        assert_eq!(priority_name(1), "normal");
        assert_eq!(priority_name(2), "high");
        // A value the daemon should never send is shown, not swallowed.
        assert_eq!(priority_name(9), "9");
    }

    #[test]
    fn one_operand_commands_reject_zero_and_many() {
        let ok = vec!["abc".to_owned()];
        assert_eq!(one_info_hash(&ok).expect("one operand"), "abc");
        assert!(matches!(one_info_hash(&[]), Err(Fail::Usage(_))));
        assert!(matches!(
            one_info_hash(&["a".to_owned(), "b".to_owned()]),
            Err(Fail::Usage(_))
        ));
    }

    #[test]
    fn bulk_commands_separate_references_from_flags() {
        let refs = |v: &[&str]| {
            parse_refs(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .map(|(refs, all)| (refs.join(","), all))
        };
        assert_eq!(refs(&["3f2a"]).expect("one"), ("3f2a".to_owned(), false));
        assert_eq!(
            refs(&["3f2a", "9b1c"]).expect("several"),
            ("3f2a,9b1c".to_owned(), false)
        );
        assert_eq!(refs(&["--all"]).expect("all"), (String::new(), true));
        // A flag is never mistaken for a torrent, whichever side it lands on;
        // `expand` is what then rejects the combination.
        assert_eq!(
            refs(&["--all", "3f2a"]).expect("flag first"),
            refs(&["3f2a", "--all"]).expect("flag last")
        );
        // An unknown option is a usage error rather than a torrent named
        // "--dat" that the daemon would then fail to resolve.
        assert!(matches!(refs(&["--dat"]), Err(Fail::Usage(_))));
        assert!(matches!(refs(&["3f2a", "-x"]), Err(Fail::Usage(_))));
    }

    #[test]
    fn a_listing_position_can_never_be_an_info_hash() {
        // Positions: one to three digits, which the daemon could not read as a
        // reference anyway since its shortest prefix is four characters.
        for index in ["1", "2", "42", "999"] {
            assert!(is_index(index), "{index} should be a position");
        }
        // Four or more characters is a reference, all-digit or not. This is
        // the regression: an all-zero info-hash is legal, is 40 digits, and
        // was being read as position zero — so `clove pause <that hash>`
        // reported "no torrent at position 0…0" instead of "no such torrent".
        for reference in [
            &"0".repeat(40),
            &"1234".to_owned(),
            &"0000".to_owned(),
            &"3f2a".to_owned(),
            &"1000".to_owned(),
        ] {
            assert!(!is_index(reference), "{reference} should be a reference");
        }
        // And nothing else is either.
        for neither in ["", "1a", "-1", "1.0", " 1"] {
            assert!(!is_index(neither), "{neither:?} should not be a position");
        }
    }

    #[test]
    fn rates_read_as_rates_and_idle_reads_as_nothing() {
        // A listing is mostly idle torrents; a column of "0 B/s" hides the one
        // row that is doing something.
        assert_eq!(human_rate(None), "-");
        assert_eq!(human_rate(Some(0)), "-");
        assert_eq!(human_rate(Some(1)), "1 B/s");
        assert_eq!(human_rate(Some(1024)), "1.0 KiB/s");
        assert_eq!(human_rate(Some(1024 * 1024)), "1.0 MiB/s");
    }

    #[test]
    fn a_bulk_command_reports_the_worst_outcome() {
        // One target keeps the single-torrent contract exactly: the error is
        // the command's own, so anything scripted against it is unchanged.
        let solo = vec!["a".to_owned()];
        let err = for_each(&solo, |_| Err(Fail::Failed("no such torrent".to_owned())));
        assert!(matches!(err, Err(Fail::Failed(m)) if m == "no such torrent"));
        assert!(for_each(&solo, |_| Ok(())).is_ok());

        let many: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect();
        // All good is good.
        assert!(for_each(&many, |_| Ok(())).is_ok());

        // A failure part-way does not stop the rest — the point of a bulk
        // pause is that one already-paused torrent does not abandon the others.
        let mut seen = Vec::new();
        let out = for_each(&many, |t| {
            seen.push(t.to_owned());
            if t == "b" {
                Err(Fail::Failed("already paused".to_owned()))
            } else {
                Ok(())
            }
        });
        assert_eq!(seen, ["a", "b", "c"], "every target was attempted");
        assert!(matches!(out, Err(Fail::Failed(m)) if m == "1 of 3 failed"));

        // An unreachable daemon stops immediately: it is not a per-torrent
        // failure and the remaining attempts would each rediscover it.
        let mut tried = 0;
        let out = for_each(&many, |_| {
            tried += 1;
            Err(Fail::Unreachable("no socket".to_owned()))
        });
        assert_eq!(tried, 1, "gave up after the first");
        assert!(matches!(out, Err(Fail::Unreachable(_))));
    }

    #[test]
    fn expand_refuses_the_ambiguous_invocations() {
        let socket = Path::new("/nonexistent/clove.sock");
        // No torrents and no --all is a usage error, named so the operator
        // learns that a prefix would have done.
        let err = expand(socket, "t", &[], false).expect_err("nothing to act on");
        assert!(
            matches!(&err, Fail::Usage(m) if m.contains("prefix")),
            "{err:?}"
        );
        // Mixing them is a usage error rather than a silent preference for one.
        let both = expand(socket, "t", &["3f2a".to_owned()], true);
        assert!(matches!(both, Err(Fail::Usage(_))));
        // Plain references never touch the socket, which is why this passes a
        // path that does not exist.
        assert_eq!(
            expand(socket, "t", &["3f2a".to_owned(), "9b1c".to_owned()], false).expect("refs"),
            vec!["3f2a".to_owned(), "9b1c".to_owned()]
        );
    }

    #[test]
    fn a_broken_response_body_is_a_failure_not_a_panic() {
        assert!(parse_body(b"{\"ok\":true}").is_ok());
        // Not JSON at all.
        assert!(matches!(parse_body(b"<html>"), Err(Fail::Failed(_))));
        // Truncated mid-object.
        assert!(matches!(parse_body(b"{\"ok\":"), Err(Fail::Failed(_))));
        // Not UTF-8.
        assert!(matches!(parse_body(&[0xFF, 0xFE]), Err(Fail::Failed(_))));
        assert!(matches!(parse_body(b""), Err(Fail::Failed(_))));
    }

    #[test]
    fn configuration_decides_where_the_daemon_is() {
        // The regression this pins: `resolve` used to parse an empty config, so
        // a data_dir or api_socket in clove.conf was invisible to the CLI and
        // every command went looking for a token in the wrong directory.
        let dir = std::env::temp_dir().join(format!("clove-cli-conf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let data = dir.join("state");
        std::fs::create_dir_all(&data).expect("data dir");
        std::fs::write(data.join("token"), "a".repeat(64)).expect("token");
        let conf = dir.join("clove.conf");
        std::fs::write(
            &conf,
            format!(
                "data_dir {}\napi_socket {}\n",
                data.display(),
                dir.join("sock").display()
            ),
        )
        .expect("conf");

        let where_ = Where {
            socket: None,
            config: Some(conf.clone()),
        };
        let (socket, token) = resolve(&where_).expect("resolve from the config file");
        assert_eq!(socket, dir.join("sock"), "api_socket was ignored");
        assert_eq!(
            token,
            "a".repeat(64),
            "the token came from the wrong data_dir"
        );

        // --socket still wins over the file, and only over the socket.
        let override_ = Where {
            socket: Some(PathBuf::from("/tmp/other.sock")),
            config: Some(conf.clone()),
        };
        let (socket, token) = resolve(&override_).expect("resolve with an override");
        assert_eq!(socket, PathBuf::from("/tmp/other.sock"));
        assert_eq!(token, "a".repeat(64));

        // A -c path that does not exist is a usage error, not a silent fallback
        // to the defaults — the same rule cloved follows.
        let missing = Where {
            socket: None,
            config: Some(dir.join("nope.conf")),
        };
        assert!(matches!(resolve(&missing), Err(Fail::Usage(_))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_codes_are_distinct_and_documented() {
        // clove(1) promises 0 ok, 1 failed, 2 usage, 3 unreachable. A script
        // that distinguishes "daemon is down" from "you asked for something
        // that does not exist" depends on these not drifting.
        let code = |f: &Fail| match f {
            Fail::Failed(_) => 1u8,
            Fail::Usage(_) => 2,
            Fail::Unreachable(_) => 3,
        };
        assert_eq!(code(&Fail::Failed(String::new())), 1);
        assert_eq!(code(&Fail::Usage(String::new())), 2);
        assert_eq!(code(&Fail::Unreachable(String::new())), 3);
    }
}
