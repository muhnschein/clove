//! `clove(1)` — control CLI for `cloved`.
//!
//! A thin client: hand-rolled arg parsing, one request per invocation over the
//! local API (unix socket), rendering the daemon's JSON (`--json` passes it
//! through). Commands: `status`, `list`, `show`, `add`, `remove`, `pause`,
//! `resume`, `verify`, `priorities`, `sequential`, `seed-ratio`,
//! `completions`.
//!
//! One view concept, deliberately. `list` is a one-shot table under a summary
//! header; for a live view users will need to use `watch clove list`.
//!
//! A torrent is named by info-hash, by a unique prefix of one, or by its
//! position in `list` — resolved by the daemon except for the position, which
//! is this end's, since a position is not an identity anything may store.

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
        Some("list") => cmd_list(&where_, json),
        Some("add") => cmd_add(&where_, &operands),
        Some("remove") => cmd_remove(&where_, &operands),
        Some("show") => cmd_show(&where_, json, &operands),
        Some("pause") => cmd_action(&where_, &operands, "pause", "paused"),
        Some("resume") => cmd_action(&where_, &operands, "resume", "resumed"),
        Some("verify") => cmd_verify(&where_, &operands),
        Some("priorities") => cmd_priorities(&where_, &operands),
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
    println!("  status [--json]                daemon, router, and client-wide totals");
    println!("  list [--json]                  hosted torrents");
    println!("  show <torrent> [--json]        one torrent in detail");
    println!("  add <file.torrent|magnet:…>    add a torrent");
    println!("      [--paused] [--sequential]  ...stopped, or in file order");
    println!("  remove <torrent…> [--data]     remove torrents (--data also deletes files)");
    println!("  pause <torrent…>               pause torrents");
    println!("  resume <torrent…>              resume torrents");
    println!("  verify <torrent…>              re-check data on disk");
    println!("  priorities <torrent> <spec>    set per-file priorities (e.g. 1,0,2)");
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
///
/// `with_pending` says whether `--all` reaches magnets still fetching their
/// metadata: `remove` can act on one, nothing else can.
fn expand(
    socket: &Path,
    token: &str,
    refs: &[String],
    all: bool,
    with_pending: bool,
) -> Result<Vec<String>, Fail> {
    if !all {
        if refs.is_empty() {
            return Err(Fail::Usage(format!(
                "this command needs a torrent ({REF_HELP}), or --all"
            )));
        }
        for reference in refs.iter().filter(|r| !is_index(r)) {
            check_reference(reference)?;
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
    Ok(listed_targets(items, with_pending))
}

/// The info-hashes `--all` stands for, out of a listing.
///
/// A magnet still fetching its metadata is an add in progress, not yet a
/// torrent: it has no engine to pause, verify or resume, and including it
/// would make `resume --all` fail for the whole run because one entry was
/// never resumable. It *can* be removed, though, and `remove --all` that left
/// every magnet behind was not removing all. `state` is the one marker that
/// says which it is, and using it keeps this rule out of every command.
fn listed_targets(items: &[Value], with_pending: bool) -> Vec<String> {
    items
        .iter()
        .filter(|item| {
            with_pending || item.get("state").and_then(Value::as_str) != Some("fetching-metadata")
        })
        .filter_map(|item| item.get("info_hash").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Shortest hash prefix the daemon resolves; `registry::MIN_PREFIX` there.
const MIN_PREFIX: usize = 4;

/// Whether `reference` has the shape of something the daemon could resolve:
/// four to forty lowercase hex characters.
fn is_reference(reference: &str) -> bool {
    (MIN_PREFIX..=40).contains(&reference.len())
        && reference
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Refuse a torrent reference the daemon would refuse, before it is built
/// into a request target.
///
/// The daemon checks the same thing, but by then the text is part of the
/// path: `clove remove 'abcd?data=1'` reached the daemon as a removal *with*
/// `data=1`, and no `--data` had been typed. A reference that is not one is a
/// usage error here, where it is still just an argument.
fn check_reference(reference: &str) -> Result<(), Fail> {
    if is_reference(reference) {
        return Ok(());
    }
    Err(Fail::Usage(format!(
        "{:?} is not a torrent reference; a torrent is named by {REF_HELP}",
        display(reference)
    )))
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
        check_reference(reference)?;
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

/// The one client-wide report: is the daemon and its router alright, and what
/// is the client doing.
fn cmd_status(where_: &Where, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(where_)?;
    let body = request(&socket, &token, "GET", "/v1/status", &[])?;
    if json {
        // Deliberately the endpoint's own answer, unchanged. The state counts
        // and lifetime totals below are an aggregation this end performs for a
        // human; inventing a CLI-only JSON shape for them would be a second
        // schema nobody versions. If they are ever wanted by a script they
        // belong in `/v1/status` itself.
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    let status = parse_body(&body)?;
    let torrents = parse_body(&request(&socket, &token, "GET", "/v1/torrents", &[])?)?;
    print!("{}", render_status(&status, &torrents));
    Ok(())
}

fn cmd_list(where_: &Where, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(where_)?;
    let body = request(&socket, &token, "GET", "/v1/torrents", &[])?;
    if json {
        println!("{}", String::from_utf8_lossy(&body).trim_end());
        return Ok(());
    }
    // The header bar is the second request, and only on the human path: a
    // listing worth reading answers "and how is the client overall".
    let status = parse_body(&request(&socket, &token, "GET", "/v1/status", &[])?)?;
    print!("{}", render_list(&status, &parse_body(&body)?));
    Ok(())
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
        // Always hex from this daemon, and still the daemon's text: the one
        // place a string from the API reached the terminal unscrubbed.
        Some(info_hash) => println!("added {}", display(info_hash)),
        // A reply we did not recognise, shown as-is so it is not lost — but
        // scrubbed, because "as-is" here means straight from the daemon's JSON
        // to a terminal, and that is the one thing this must not be.
        None => println!("{}", display(String::from_utf8_lossy(&reply).trim())),
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
    let targets = expand(&socket, &token, &refs, all, true)?;
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
    let targets = expand(&socket, &token, &refs, all, false)?;
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
    let targets = expand(&socket, &token, &refs, all, false)?;
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
        "banned_peers",
        // Last, and unwrapped: each is a sentence rather than a field, and
        // between them they answer the questions a stopped, peerless or
        // stalled torrent raises.
        "paused_because",
        "last_announce_error",
        "storage_error",
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
/// elided.
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
/// What is replaced, and why, is [`clove_core::text::scrub`]'s to say — the
/// daemon scrubs its own stderr with the same function, and two copies of a
/// security control are how one of them comes to be missing a case.
///
/// Everything else passes through, including the UTF-8 that makes [`align`]
/// approximate — it counts characters, not display columns, which is unchanged
/// here and not worth a dependency to fix.
fn display(s: &str) -> String {
    clove_core::text::scrub(s)
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

/// Label column for [`render_status`]: two spaces of indent, the longest state
/// name (`waiting-for-router`, eighteen), and two more before the value — so
/// the state breakdown nests under the torrent count, and the longest label
/// there neither pushes the value column right nor runs into it.
const STATUS_LABEL: usize = 22;

/// The whole client on one screen: the daemon, its router, and what every
/// torrent adds up to right now.
///
/// `status` comes from `/v1/status`; the state breakdown and the lifetime
/// totals are summed here from `/v1/torrents`, because they are a question
/// about the collection rather than a field the daemon keeps.
fn render_status(status: &Value, torrents: &Value) -> String {
    let empty: Vec<Value> = Vec::new();
    let items: &[Value] = torrents.as_array().unwrap_or(&empty);

    let mut by_state: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let (mut up, mut down) = (0u64, 0u64);
    for item in items {
        *by_state.entry(field_str(item, "state")).or_default() += 1;
        up += item.get("uploaded").and_then(Value::as_u64).unwrap_or(0);
        down += item.get("downloaded").and_then(Value::as_u64).unwrap_or(0);
    }

    let num = |key: &str| status.get(key).and_then(Value::as_u64).unwrap_or(0);

    // Built as rows first so the blank separators are part of the report
    // rather than something written between two halves of it. An empty label
    // and an empty value is the blank line.
    let mut rows: Vec<(String, String)> = vec![
        // Who is answering, whether it can reach the network, and for how
        // long. The router line and the SAM address are the daemon's words
        // rather than ours, so they go through the sanitiser a name does.
        ("clove".to_owned(), field_str(status, "version")),
        ("router".to_owned(), field_str(status, "router")),
        ("sam".to_owned(), field_str(status, "sam_address")),
        ("uptime".to_owned(), human_duration(num("uptime_secs"))),
    ];
    // Omitted when the daemon does not report it, rather than rendered as the
    // "-" a missing field gives: a dash here reads as "nothing applied", and an
    // old daemon should not be able to say that.
    if status.get("sandbox").is_some() {
        rows.push(("sandbox".to_owned(), field_str(status, "sandbox")));
    }
    rows.extend([
        (String::new(), String::new()),
        ("torrents".to_owned(), items.len().to_string()),
    ]);
    rows.extend(
        by_state
            .iter()
            .map(|(state, count)| (format!("  {state}"), count.to_string())),
    );
    rows.extend([
        ("down rate".to_owned(), whole_rate(Some(num("down_rate")))),
        ("up rate".to_owned(), whole_rate(Some(num("up_rate")))),
        (
            "peers".to_owned(),
            format!("{} of {}", num("peers"), num("peer_limit")),
        ),
        (String::new(), String::new()),
        // Lifetime rather than this session: these are the resume files'
        // totals, and they keep their decimal — nothing repaints them, and
        // the difference between 1.2 and 1.9 TiB is why they are read.
        ("downloaded".to_owned(), human_size(down)),
        ("uploaded".to_owned(), human_size(up)),
    ]);

    let mut out = String::new();
    for (label, value) in rows {
        if label.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "{label:<STATUS_LABEL$}{value}");
        }
    }
    out
}

/// The summary line above the listing: the same glance `top` used to open
/// with, on a command that exits.
fn list_header(status: &Value) -> String {
    let num = |key: &str| status.get(key).and_then(Value::as_u64).unwrap_or(0);
    let count = num("torrents");
    format!(
        "clove {}  {}  {}  {count} torrent{}  ▼ {}  ▲ {}  {} ({} max.)\n",
        field_str(status, "version"),
        field_str(status, "router"),
        human_duration(num("uptime_secs")),
        if count == 1 { "" } else { "s" },
        whole_rate(Some(num("down_rate"))),
        whole_rate(Some(num("up_rate"))),
        num("peers"),
        num("peer_limit"),
    )
}

/// Column widths for the listing, in order: `#`, `PROGRESS`, `STATE`, `SIZE`,
/// `▼`, `▲`, `PEERS`. `NAME` is last and takes what it needs up to
/// [`NAME_WIDTH`].
///
/// Fixed rather than measured from the rows, which is the point. Widths taken
/// from the content mean every column after the one that changed steps
/// sideways the moment a rate crosses into four digits or a torrent finishes
/// verifying — so a listing being watched (`watch -n 2 clove list`) never
/// holds still, and the eye has to re-find each column on every repaint. The
/// numbers are the widest each field can render: `1023 GiB` is eight, a rate
/// with its unit is ten, and `waiting-for-router` is eighteen.
const LIST_WIDTHS: [usize; 7] = [3, PROGRESS_WIDTH, 18, 8, 10, 10, 7];

/// The `PROGRESS` cell: the bar, a space, and the percentage right-aligned in
/// four (`100%`).
const PROGRESS_WIDTH: usize = BAR_CELLS + 5;

/// Render the listing: a header bar, a blank line, then one row per torrent.
fn render_list(status: &Value, torrents: &Value) -> String {
    let Some(items) = torrents.as_array() else {
        return format!("{}\n", torrents.to_line());
    };
    let mut out = list_header(status);
    out.push('\n');
    if items.is_empty() {
        out.push_str("no torrents\n");
        return out;
    }
    // Headers are aligned the way their column is: the numeric ones to the
    // right, so a figure and its label share an edge.
    write_fixed(
        &mut out,
        &[
            ("#", Align::Right),
            ("PROGRESS", Align::Left),
            ("STATE", Align::Left),
            ("SIZE", Align::Right),
            ("▼", Align::Right),
            ("▲", Align::Right),
            ("PEERS", Align::Right),
            ("NAME", Align::Left),
        ],
    );
    for (index, item) in items.iter().enumerate() {
        let progress = progress_cell(item.get("progress").and_then(Value::as_f64));
        let size = item
            .get("size")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".to_owned(), whole_size);
        let name = elide(
            item.get("name").and_then(Value::as_str).unwrap_or("-"),
            NAME_WIDTH,
        );
        write_fixed(
            &mut out,
            &[
                ((index + 1).to_string().as_str(), Align::Right),
                (progress.as_str(), Align::Left),
                (field_str(item, "state").as_str(), Align::Left),
                (size.as_str(), Align::Right),
                (
                    whole_rate(item.get("down_rate").and_then(Value::as_u64)).as_str(),
                    Align::Right,
                ),
                (
                    whole_rate(item.get("up_rate").and_then(Value::as_u64)).as_str(),
                    Align::Right,
                ),
                (peers_cell(item).as_str(), Align::Right),
                (name.as_str(), Align::Left),
            ],
        );
    }
    out
}

/// Which edge a listing cell is padded against.
#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
}

/// Write one listing row at [`LIST_WIDTHS`], two spaces between columns.
///
/// A cell wider than its column is printed in full rather than truncated —
/// misaligning one row is better than silently reporting a smaller number
/// than the daemon gave — and the last column is never padded.
fn write_fixed(out: &mut String, cells: &[(&str, Align)]) {
    let last = cells.len().saturating_sub(1);
    for (i, (text, align)) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let width = LIST_WIDTHS.get(i).copied().unwrap_or(0);
        let pad = width.saturating_sub(text.chars().count());
        if i == last {
            out.push_str(text);
        } else if *align == Align::Right {
            for _ in 0..pad {
                out.push(' ');
            }
            out.push_str(text);
        } else {
            out.push_str(text);
            for _ in 0..pad {
                out.push(' ');
            }
        }
    }
    out.push('\n');
}

/// `connected/known`, the two numbers that separate "this swarm is small" from
/// "this torrent cannot reach anyone".
///
/// A magnet still fetching its metadata knows peers but has no engine holding
/// connections, so its left half is a dash rather than a zero it did not
/// measure.
fn peers_cell(item: &Value) -> String {
    let count = |key: &str| {
        item.get(key)
            .and_then(Value::as_u64)
            .map_or_else(|| "-".to_owned(), |n| n.to_string())
    };
    format!("{}/{}", count("peers"), count("known_peers"))
}

/// Cells in the progress bar.
const BAR_CELLS: usize = 10;

/// Partial cells, in eighths of a cell — so the bar moves eight times per cell
/// rather than once, and a torrent that gains a percent visibly gains it.
const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// The bar and the figure, in one column: `████▌░░░░░  45%`.
///
/// The two never disagree. Both round *down*, so a torrent at 99.6% shows
/// neither a full bar nor `100%` — `100%` is reserved for a torrent that
/// actually has every piece it asked for, which is the one thing an operator
/// reads this column to find out.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display only: the value is a fraction clamped to 0..=1 and the \
              result is a cell count under 100"
)]
fn progress_cell(progress: Option<f64>) -> String {
    let Some(fraction) = progress else {
        return "-".to_owned();
    };
    // NaN fails both comparisons and lands on 0.0, which is the honest
    // rendering of a number the daemon should not have sent.
    let fraction = if fraction > 1.0 {
        1.0
    } else if fraction > 0.0 {
        fraction
    } else {
        0.0
    };
    let eighths = (fraction * (BAR_CELLS * EIGHTHS.len()) as f64) as usize;
    let full = eighths / EIGHTHS.len();
    let part = eighths % EIGHTHS.len();
    let mut bar = String::with_capacity(BAR_CELLS * 3);
    for _ in 0..full {
        bar.push('█');
    }
    if full < BAR_CELLS && part > 0 {
        bar.push(EIGHTHS[part - 1]);
    }
    while bar.chars().count() < BAR_CELLS {
        bar.push('░');
    }
    format!("{bar} {:>3}%", (fraction * 100.0) as u64)
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
fn human_rate(bytes_per_sec: Option<u64>) -> String {
    match bytes_per_sec {
        Some(0) | None => "-".to_owned(),
        Some(rate) => format!("{}/s", human_size(rate)),
    }
}

/// [`human_rate`] without the decimal, for the listing and its header.
fn whole_rate(bytes_per_sec: Option<u64>) -> String {
    match bytes_per_sec {
        Some(0) | None => "-".to_owned(),
        Some(rate) => format!("{}/s", whole_size(rate)),
    }
}

/// [`human_size`] rounded to whole units: `1 GiB`, `700 MiB`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display only; the rounded value is bounded by the unit table"
)]
fn whole_size(bytes: u64) -> String {
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
    // Same trap as `human_size`, one unit further along: 1023.6 MiB rounds to
    // 1024, which is a size written in units of itself.
    if size.round() >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{} {}", size.round() as u64, UNITS[unit])
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
        // Scrubbed: an error body carries torrent names — the 409 an ambiguous
        // prefix returns lists its candidates — and this string is printed
        // straight to a terminal by `main`. `json::write_string` escapes
        // everything below `0x20`, so the escape sequences cannot survive the
        // daemon's encoder, but the bidirectional overrides can and do.
        return Err(Fail::Failed(format!(
            "daemon returned {} — {}",
            response.status,
            display(detail.trim())
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
        for ok in [
            "Jeffrey Epstein House Oversight Committee Photo Release Dec 19th 2025.zip",
            "Путін — хуйло!",
            "小熊维尼",
            "café",
            "🎉",
        ] {
            assert_eq!(display(ok), ok);
        }

        // And it has to hold through the renderers an operator actually reads,
        // not just the helper.
        let table = render_list(
            &Value::Null,
            &Value::Array(vec![obj(&[
                ("name", Value::from(hostile)),
                ("state", Value::from("seeding\u{1b}[2J")),
                ("info_hash", Value::from("ab".repeat(20))),
            ])]),
        );
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

        // Every string the daemon hands back reaches a terminal through
        // `field_str`, so that is the boundary worth pinning directly.
        assert_eq!(
            field_str(&obj(&[("router", Value::from("lost\u{1b}[2J"))]), "router"),
            "lost.[2J"
        );
        // `status` and the listing's header bar print the router's last word
        // and the SAM address, neither of which is ours either.
        let hostile_status = obj(&[
            ("router", Value::from("lost\u{1b}[2J")),
            ("version", Value::from("2026.08\u{1b}[H")),
            ("sam_address", Value::from("127.0.0.1:7656\u{7}")),
        ]);
        let status = render_status(&hostile_status, &Value::Array(Vec::new()));
        assert!(!status.contains('\u{1b}'), "{status:?}");
        let header = list_header(&hostile_status);
        assert!(!header.contains('\u{1b}'), "{header:?}");
        assert!(!header.contains('\u{7}'), "{header:?}");
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
        // terminal, which is what the width is for. The bound is the fixed
        // columns plus the name's own budget — with fixed widths that is the
        // widest a row can ever be, whatever the daemon sends.
        let fixed: usize = LIST_WIDTHS.iter().sum::<usize>() + 2 * LIST_WIDTHS.len();
        let table = render_list(
            &Value::Null,
            &Value::Array(vec![obj(&[
                ("name", Value::from("n".repeat(400))),
                ("info_hash", Value::from("cd".repeat(20))),
            ])]),
        );
        for line in table.lines() {
            assert!(
                line.chars().count() <= fixed + NAME_WIDTH,
                "{} chars",
                line.chars().count()
            );
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
        let out = render_list(&Value::Null, &Value::Array(Vec::new()));
        // The header bar still prints: "how is the client" is a question an
        // empty listing answers as usefully as a full one.
        assert!(out.starts_with("clove "), "{out}");
        assert!(out.ends_with("no torrents\n"), "{out}");
    }

    #[test]
    fn the_listing_drops_the_hash_and_fills_missing_fields() {
        let full = "58e2fc46a8dc57c78191f079648750b0644d03a2";
        let out = render_list(
            &Value::Null,
            &Value::Array(vec![
                obj(&[
                    ("info_hash", Value::from(full.to_owned())),
                    ("name", Value::from("release.iso".to_owned())),
                    ("size", Value::UInt(1_500_000_000)),
                    ("progress", Value::Float(0.423)),
                    ("state", Value::from("downloading".to_owned())),
                    ("peers", Value::UInt(4)),
                    ("known_peers", Value::UInt(20)),
                ]),
                // A torrent whose metadata has not arrived yet: every optional
                // field is absent and must render as a dash, not as "null".
                obj(&[("info_hash", Value::from("ab".repeat(20)))]),
            ]),
        );
        // The hash is `show`'s business now. A listing that carried it was
        // spending fourteen columns on something no eye reads and no command
        // needs typed back at it — a `#` does that job.
        assert!(!out.contains("58e2fc46a8dc"), "{out}");
        assert!(!out.contains(full), "{out}");
        assert!(!out.contains("INFO-HASH"), "{out}");
        assert!(out.contains("42%"), "{out}");
        // Whole units in the listing, decimals kept for `show`.
        assert!(out.contains("1 GiB"), "{out}");
        assert!(!out.contains("1.4 GiB"), "{out}");
        assert!(out.contains("4/20"), "{out}");
        assert!(!out.contains("null"), "{out}");
        // The torrent with nothing known about it renders dashes throughout,
        // including both halves of the peer column.
        assert!(out.contains("-/-"), "{out}");
    }

    #[test]
    fn a_non_array_listing_does_not_pretend_to_be_a_table() {
        // If the daemon ever answered with something unexpected, the CLI
        // prints it rather than rendering an empty table over it.
        let out = render_list(&Value::Null, &Value::from("unexpected".to_owned()));
        assert!(out.contains("unexpected"), "{out}");
    }

    #[test]
    fn the_listing_holds_its_columns_still() {
        // The whole point of fixed widths: two listings whose contents differ
        // in every measurable way still put every column at the same offset,
        // so a table being repainted by `watch -n 2 clove list` does not
        // shuffle itself under the reader.
        let row = |state: &str, size: u64, rate: u64, peers: u64| {
            Value::Array(vec![obj(&[
                ("name", Value::from("x".to_owned())),
                ("state", Value::from(state.to_owned())),
                ("size", Value::UInt(size)),
                ("progress", Value::Float(0.5)),
                ("down_rate", Value::UInt(rate)),
                ("up_rate", Value::UInt(rate)),
                ("peers", Value::UInt(peers)),
                ("known_peers", Value::UInt(peers)),
            ])])
        };
        let narrow = render_list(&Value::Null, &row("seeding", 1024, 1, 1));
        let wide = render_list(
            &Value::Null,
            &row("waiting-for-router", 1024u64.pow(4), 999 * 1024, 999),
        );
        // Characters, not bytes: the bar and the ▼/▲ headers are multi-byte,
        // and a byte offset would call two aligned columns different.
        let column_of = |line: &str, needle: &str| {
            let byte = line.find(needle).expect("the column");
            line[..byte].chars().count()
        };
        let name_at = |table: &str| column_of(table.lines().last().expect("a row"), "x");
        assert_eq!(name_at(&narrow), name_at(&wide), "{narrow}\n{wide}");
        // And the header sits over the columns it names.
        let header_at =
            |table: &str| column_of(table.lines().nth(2).expect("the column head"), "NAME");
        assert_eq!(header_at(&narrow), name_at(&narrow), "{narrow}");
        assert_eq!(header_at(&wide), name_at(&wide), "{wide}");
    }

    #[test]
    fn the_progress_bar_and_its_figure_agree() {
        // Both round down, so neither claims a torrent is finished before it
        // is — the one thing this column is read to find out.
        assert_eq!(progress_cell(Some(0.0)), "░░░░░░░░░░   0%");
        assert_eq!(progress_cell(Some(1.0)), "██████████ 100%");
        let nearly = progress_cell(Some(0.996));
        assert!(nearly.ends_with(" 99%"), "{nearly}");
        assert!(!nearly.starts_with("██████████"), "{nearly}");
        // Every cell is one character wide and the whole cell is fixed, so the
        // column cannot breathe as a torrent fills.
        for step in 0..=1000 {
            let cell = progress_cell(Some(f64::from(step) / 1000.0));
            assert_eq!(cell.chars().count(), PROGRESS_WIDTH, "{step}: {cell:?}",);
        }
        // Nothing the daemon could send may produce a bar of another width.
        for odd in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 2.0] {
            assert_eq!(progress_cell(Some(odd)).chars().count(), PROGRESS_WIDTH);
        }
        // A magnet with no progress at all is a dash, not a zero it did not
        // measure.
        assert_eq!(progress_cell(None), "-");
    }

    #[test]
    fn whole_sizes_and_rates_drop_the_decimal_without_lying() {
        assert_eq!(whole_size(0), "0 B");
        assert_eq!(whole_size(1023), "1023 B");
        assert_eq!(whole_size(1024), "1 KiB");
        assert_eq!(whole_size(1_500_000_000), "1 GiB");
        assert_eq!(whole_size(700 * 1024 * 1024), "700 MiB");
        assert!(whole_size(u64::MAX).ends_with(" PiB"));
        // Nothing may render as 1024 of a smaller unit, the same trap
        // `human_size` steps around one decimal place earlier.
        for n in [1023u64, 1024, 1024 * 1024 - 1, 1024 * 1024 * 1023 + 1] {
            assert!(
                !whole_size(n).starts_with("1024 "),
                "{n} → {}",
                whole_size(n)
            );
        }
        // Idle still reads as nothing rather than as a zero.
        assert_eq!(whole_rate(None), "-");
        assert_eq!(whole_rate(Some(0)), "-");
        assert_eq!(whole_rate(Some(1024)), "1 KiB/s");
    }

    #[test]
    fn status_answers_both_questions_at_once() {
        // The merge: what `status` used to say about the daemon, and what
        // `stats` used to say about the torrents, in one report.
        let status = obj(&[
            ("version", Value::from("2026.08".to_owned())),
            ("router", Value::from("connected".to_owned())),
            ("sam_address", Value::from("127.0.0.1:7656".to_owned())),
            ("uptime_secs", Value::UInt(11_520)),
            ("down_rate", Value::UInt(84_000)),
            ("up_rate", Value::UInt(9_216)),
            ("peers", Value::UInt(11)),
            ("peer_limit", Value::UInt(200)),
        ]);
        let torrents = Value::Array(vec![
            obj(&[
                ("state", Value::from("downloading".to_owned())),
                ("downloaded", Value::UInt(1024 * 1024)),
                ("uploaded", Value::UInt(512 * 1024)),
            ]),
            obj(&[
                ("state", Value::from("seeding".to_owned())),
                ("downloaded", Value::UInt(1024 * 1024)),
                ("uploaded", Value::UInt(512 * 1024)),
            ]),
            obj(&[("state", Value::from("seeding".to_owned()))]),
        ]);
        let out = render_status(&status, &torrents);
        // The daemon half.
        assert!(out.contains("connected"), "{out}");
        assert!(out.contains("127.0.0.1:7656"), "{out}");
        assert!(out.contains("3h12m"), "{out}");
        // The client half: a count, the states under it, and the lifetime
        // totals that only exist by summing the listing.
        assert!(out.contains("torrents"), "{out}");
        assert!(out.contains("  downloading"), "{out}");
        assert!(out.contains("  seeding"), "{out}");
        assert!(out.contains("11 of 200"), "{out}");
        // Lifetime totals keep their decimal: nothing repaints them, and the
        // difference they carry is the reason they are read.
        assert!(out.contains("2.0 MiB"), "lifetime downloaded: {out}");
        assert!(out.contains("1.0 MiB"), "lifetime uploaded: {out}");
        // Rates lose the decimal here too, so the two reports agree.
        assert!(out.contains("82 KiB/s"), "{out}");
        assert!(out.contains("9 KiB/s"), "{out}");
        // Every value starts at one column, including the indented state
        // rows, so nothing steps right for `waiting-for-router`.
        for line in out.lines().filter(|l| !l.is_empty()) {
            let at: Vec<char> = line.chars().collect();
            assert_eq!(at.get(STATUS_LABEL - 1), Some(&' '), "{line:?}");
            assert!(at.get(STATUS_LABEL).is_some_and(|c| *c != ' '), "{line:?}");
        }
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
        let err = expand(socket, "t", &[], false, false).expect_err("nothing to act on");
        assert!(
            matches!(&err, Fail::Usage(m) if m.contains("prefix")),
            "{err:?}"
        );
        // Mixing them is a usage error rather than a silent preference for one.
        let both = expand(socket, "t", &["3f2a".to_owned()], true, false);
        assert!(matches!(both, Err(Fail::Usage(_))));
        // Plain references never touch the socket, which is why this passes a
        // path that does not exist.
        assert_eq!(
            expand(
                socket,
                "t",
                &["3f2a".to_owned(), "9b1c".to_owned()],
                false,
                false
            )
            .expect("refs"),
            vec!["3f2a".to_owned(), "9b1c".to_owned()]
        );
    }

    /// A reference is checked before it is built into a request path. The
    /// regression: `remove 'abcd?data=1'` reached the daemon as a removal
    /// with `data=1`, and nobody had typed `--data`.
    #[test]
    fn a_reference_that_is_not_one_is_refused_before_it_becomes_a_path() {
        for good in ["abcd", "3f2a", &"0".repeat(40), &"f".repeat(40), "1234"] {
            assert!(is_reference(good), "{good:?} should be a reference");
        }
        for bad in [
            "abcd?data=1",
            "abcd/verify",
            "abcd#x",
            "ABCD",
            "abc",
            &"a".repeat(41),
            "",
            "../..",
            "abcd ",
        ] {
            assert!(!is_reference(bad), "{bad:?} should not be a reference");
        }

        // Through the two doors every command uses. Neither touches the
        // socket for a refused reference, so a path that does not exist does.
        let socket = Path::new("/nonexistent/clove.sock");
        let err = expand(socket, "t", &["abcd?data=1".to_owned()], false, false)
            .expect_err("a query string passed as a torrent");
        assert!(
            matches!(&err, Fail::Usage(m) if m.contains("not a torrent reference")),
            "{err:?}"
        );
        assert!(matches!(
            one_target(socket, "t", "abcd/verify"),
            Err(Fail::Usage(_))
        ));
        assert_eq!(
            one_target(socket, "t", "abcd").expect("a plain prefix"),
            "abcd"
        );
        // A position is still a position, resolved against the listing —
        // which here means an unreachable daemon, not a usage error.
        assert!(matches!(
            one_target(socket, "t", "1"),
            Err(Fail::Unreachable(_))
        ));
    }

    /// `--all` reaches a pending magnet for `remove` and for nothing else.
    #[test]
    fn remove_all_includes_pending_magnets_and_the_other_bulk_commands_do_not() {
        let items = vec![
            obj(&[
                ("info_hash", Value::from("a".repeat(40))),
                ("state", Value::from("seeding")),
            ]),
            obj(&[
                ("info_hash", Value::from("b".repeat(40))),
                ("state", Value::from("fetching-metadata")),
            ]),
            obj(&[
                ("info_hash", Value::from("c".repeat(40))),
                ("state", Value::from("paused")),
            ]),
        ];
        assert_eq!(
            listed_targets(&items, true),
            vec!["a".repeat(40), "b".repeat(40), "c".repeat(40)],
            "remove --all left the magnet behind"
        );
        assert_eq!(
            listed_targets(&items, false),
            vec!["a".repeat(40), "c".repeat(40)],
            "a bulk pause or resume would fail on the magnet"
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
