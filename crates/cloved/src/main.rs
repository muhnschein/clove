//! `cloved(8)` — the clove daemon.
//!
//! Loads config, opens the data dir, and serves the local `/v1/` HTTP API
//! (hand-rolled HTTP/1.1 + JSON, Q6) over a unix socket. Engine hosting and
//! the full command set arrive in later Phase-F slices (`docs/PHASE-F.md`);
//! this slice serves `GET /v1/status` with token auth — the transport, end to
//! end. Layer-2 self-restriction (Landlock/seccomp) is Phase G.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clove_core::config::{Config, Defaults};
use clove_core::http::{self, Response};
use clove_core::json::Value;
use i2pnet::api::{ApiListener, ApiStream};

/// Cap on an API request body (a `.torrent` or magnet; generous for status).
const MAX_REQUEST_BODY: usize = 2 * 1024 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cloved: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parsed command line: `cloved [-C|--check] [-c <config>]`.
struct Args {
    check: bool,
    config_path: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut check = false;
    let mut config_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-C" | "--check" => check = true,
            "-c" | "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| format!("{arg} needs a path argument"))?;
                config_path = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                println!("usage: cloved [-C|--check] [-c <config>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(Args { check, config_path })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let defaults = Defaults::from_env().map_err(|e| e.to_string())?;
    let text = match &args.config_path {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?
        }
        None => String::new(),
    };
    let config = Config::parse(&text, &defaults).map_err(|e| e.to_string())?;

    if args.check {
        println!("cloved: configuration OK");
        println!("  data_dir   {}", config.data_dir.display());
        println!("  api_socket {}", config.api_socket.display());
        println!("  sam_address {}", config.sam_address);
        return Ok(());
    }

    std::fs::create_dir_all(&config.data_dir)
        .map_err(|e| format!("creating data dir {}: {e}", config.data_dir.display()))?;
    if let Some(parent) = config.api_socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating socket dir {}: {e}", parent.display()))?;
    }
    let token = load_or_create_token(&config.data_dir).map_err(|e| e.to_string())?;

    let listener = ApiListener::bind_unix(&config.api_socket)
        .map_err(|e| format!("binding {}: {e}", config.api_socket.display()))?;
    eprintln!("cloved: listening on {}", config.api_socket.display());

    let daemon = Arc::new(Daemon {
        start: Instant::now(),
        sam_address: config.sam_address,
        token,
    });
    serve(&listener, &daemon)
}

/// Daemon state shared across connection threads. The engine host (torrent
/// registry, persistence) attaches here in the next slice.
struct Daemon {
    start: Instant,
    sam_address: String,
    token: String,
}

/// Accept loop: one thread per connection (Q5; API load is tiny). Only a fatal
/// accept error returns.
fn serve(listener: &ApiListener, daemon: &Arc<Daemon>) -> Result<(), String> {
    loop {
        match listener.accept() {
            Ok(stream) => {
                let daemon = Arc::clone(daemon);
                std::thread::spawn(move || {
                    if let Err(e) = handle(stream, &daemon) {
                        eprintln!("cloved: connection error: {e}");
                    }
                });
            }
            Err(e) => return Err(format!("accept failed: {e}")),
        }
    }
}

/// Serve one request: parse, authenticate, route, respond.
fn handle(mut stream: ApiStream, daemon: &Daemon) -> std::io::Result<()> {
    let Ok(request) = http::read_request(&mut stream, MAX_REQUEST_BODY) else {
        return write_response(&mut stream, &error(400, "malformed request"));
    };

    // Token auth on every request, unix socket included (SCOPE §3).
    let ok = request
        .header("x-clove-token")
        .is_some_and(|got| constant_time_eq(got.as_bytes(), daemon.token.as_bytes()));
    if !ok {
        return write_response(&mut stream, &error(401, "missing or invalid API token"));
    }

    let response = route(&request, daemon);
    write_response(&mut stream, &response)
}

fn route(request: &http::ServerRequest, daemon: &Daemon) -> Response {
    match (request.method.as_str(), request.path()) {
        ("GET", "/v1/status") => Response::new(200, "application/json", status_json(daemon)),
        ("GET", _) => error(404, "no such resource"),
        _ => error(405, "method not allowed"),
    }
}

fn status_json(daemon: &Daemon) -> Vec<u8> {
    Value::Object(vec![
        ("version".to_owned(), Value::from(env!("CARGO_PKG_VERSION"))),
        (
            "uptime_secs".to_owned(),
            Value::UInt(daemon.start.elapsed().as_secs()),
        ),
        (
            "sam_address".to_owned(),
            Value::from(daemon.sam_address.clone()),
        ),
        // Placeholders until the engine host and SAM supervisor are wired.
        ("torrents".to_owned(), Value::UInt(0)),
        ("router".to_owned(), Value::from("not-connected")),
    ])
    .encode()
    .into_bytes()
}

/// A JSON error body with the given status.
fn error(status: u16, message: &str) -> Response {
    let body = Value::Object(vec![("error".to_owned(), Value::from(message))])
        .encode()
        .into_bytes();
    Response::new(status, "application/json", body)
}

fn write_response(stream: &mut ApiStream, response: &Response) -> std::io::Result<()> {
    stream.write_all(&response.encode())?;
    stream.flush()
}

/// Read the API token from `<data_dir>/token`, creating it (32 random bytes,
/// hex, `0600`) on first run.
fn load_or_create_token(data_dir: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;

    let path = data_dir.join("token");
    match std::fs::read_to_string(&path) {
        Ok(existing) => Ok(existing.trim().to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut raw = [0u8; 32];
            getrandom::getrandom(&mut raw)
                .map_err(|e| std::io::Error::other(format!("getrandom: {e}")))?;
            let token = hex(&raw);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(token.as_bytes())?;
            Ok(token)
        }
        Err(e) => Err(e),
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[(b >> 4) as usize]));
        out.push(char::from(HEX[(b & 0x0f) as usize]));
    }
    out
}

/// Length-independent byte comparison, so token checks don't leak length or a
/// prefix match through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}
