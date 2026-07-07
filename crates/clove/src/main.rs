//! `clove(1)` — control CLI for `cloved`.
//!
//! A thin client: hand-rolled arg parsing, one request per invocation over the
//! local API (unix socket), rendering the daemon's JSON. This slice implements
//! `clove status`; the remaining commands, aligned-table rendering (which needs
//! the JSON parser), and `clove watch` arrive in later Phase-F slices
//! (`docs/PHASE-F.md`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clove_core::config::{Config, Defaults};
use clove_core::http::{self, Request};
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
                println!("usage: clove [--socket <path>] <command>");
                println!("commands:");
                println!("  status [--json]   daemon and router status");
                return Ok(());
            }
            other if command.is_none() => command = Some(other.to_owned()),
            other => return Err(Fail::Usage(format!("unexpected argument {other:?}"))),
        }
    }

    match command.as_deref() {
        Some("status") => cmd_status(socket, json),
        Some(other) => Err(Fail::Usage(format!(
            "unknown command {other:?} (try --help)"
        ))),
        None => Err(Fail::Usage("no command given (try --help)".to_owned())),
    }
}

fn cmd_status(socket: Option<PathBuf>, json: bool) -> Result<(), Fail> {
    let (socket, token) = resolve(socket)?;
    let body = request(&socket, &token, "GET", "/v1/status")?;
    // Table rendering needs the JSON parser (next slice); for now the status
    // blob is printed as-is, and `--json` is the same body verbatim.
    let _ = json;
    let text = String::from_utf8_lossy(&body);
    println!("{}", text.trim_end());
    Ok(())
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
fn request(socket: &Path, token: &str, method: &str, target: &str) -> Result<Vec<u8>, Fail> {
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
