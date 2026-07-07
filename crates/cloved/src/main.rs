//! `cloved(8)` — the clove daemon.
//!
//! Loads config, opens the data dir, and serves the local `/v1/` HTTP API
//! (hand-rolled HTTP/1.1 + JSON, Q6) over a unix socket. Engine hosting and
//! the full command set arrive in later Phase-F slices (`docs/PHASE-F.md`);
//! this slice serves `GET /v1/status` with token auth — the transport, end to
//! end. Layer-2 self-restriction (Landlock/seccomp) is Phase G.

mod registry;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use clove_core::config::{Config, Defaults};
use clove_core::http::{self, Response};
use clove_core::json::Value;
use i2pnet::api::{ApiListener, ApiStream};

use crate::registry::{ActionError, AddError, Registry, RemoveError};

/// Cap on an API request body (a `.torrent` or magnet; generous for status).
const MAX_REQUEST_BODY: usize = 2 * 1024 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

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

    let registry = Registry::open(&config.data_dir)
        .map_err(|e| format!("opening registry in {}: {e}", config.data_dir.display()))?;
    eprintln!("cloved: {} torrent(s) loaded", registry.count());

    let listener = ApiListener::bind_unix(&config.api_socket)
        .map_err(|e| format!("binding {}: {e}", config.api_socket.display()))?;
    eprintln!("cloved: listening on {}", config.api_socket.display());

    let daemon = Arc::new(Daemon {
        start: Instant::now(),
        sam_address: config.sam_address,
        token,
        registry: Mutex::new(registry),
    });
    serve(&listener, &daemon)
}

/// Daemon state shared across connection threads.
struct Daemon {
    start: Instant,
    sam_address: String,
    token: String,
    registry: Mutex<Registry>,
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
    let method = request.method.as_str();
    let path = request.path();
    match (method, path) {
        ("GET", "/v1/status") => Response::new(200, "application/json", status_json(daemon)),
        ("GET", "/v1/torrents") => {
            let body = lock(&daemon.registry).list().encode().into_bytes();
            Response::new(200, "application/json", body)
        }
        ("POST", "/v1/torrents") => add_torrent(request, daemon),
        (_, p) if p.starts_with("/v1/torrents/") => {
            torrent_action(method, request, daemon, &p["/v1/torrents/".len()..])
        }
        ("GET", _) => error(404, "no such resource"),
        _ => error(405, "method not allowed"),
    }
}

fn add_torrent(request: &http::ServerRequest, daemon: &Daemon) -> Response {
    if request.body.starts_with(b"magnet:") {
        return error(
            501,
            "magnet links need a running SAM session (a later slice)",
        );
    }
    match lock(&daemon.registry).add_torrent(&request.body) {
        Ok(info_hash) => {
            let body = Value::Object(vec![(
                "info_hash".to_owned(),
                Value::from(registry::hex(&info_hash)),
            )])
            .encode()
            .into_bytes();
            Response::new(201, "application/json", body)
        }
        Err(AddError::Parse(e)) => error(400, &e.to_string()),
        Err(AddError::Duplicate) => error(409, "torrent already added"),
        Err(AddError::Io(e)) => error(500, &format!("adding torrent: {e}")),
    }
}

/// Route a request against a specific torrent: `<info-hash>` or
/// `<info-hash>/<action>`.
fn torrent_action(
    method: &str,
    request: &http::ServerRequest,
    daemon: &Daemon,
    rest: &str,
) -> Response {
    let (hex, action) = match rest.split_once('/') {
        Some((hex, action)) => (hex, Some(action)),
        None => (rest, None),
    };
    let Some(info_hash) = registry::parse_info_hash(hex) else {
        return error(400, "info-hash must be 40 lowercase-hex characters");
    };

    match (method, action) {
        ("GET", None) => match lock(&daemon.registry).detail(&info_hash) {
            Some(value) => Response::new(200, "application/json", value.encode().into_bytes()),
            None => error(404, "no such torrent"),
        },
        ("DELETE", None) => {
            let delete_data = request.query().is_some_and(query_has_data);
            match lock(&daemon.registry).remove(&info_hash, delete_data) {
                Ok(()) => ok_json(),
                Err(RemoveError::NotFound) => error(404, "no such torrent"),
                Err(RemoveError::Io(e)) => error(500, &format!("removing torrent: {e}")),
            }
        }
        ("POST", Some("pause")) => {
            action_result(lock(&daemon.registry).set_paused(&info_hash, true))
        }
        ("POST", Some("resume")) => {
            action_result(lock(&daemon.registry).set_paused(&info_hash, false))
        }
        ("POST", Some("verify")) => match lock(&daemon.registry).verify(&info_hash) {
            Ok(verified) => {
                let body = Value::Object(vec![(
                    "verified".to_owned(),
                    Value::UInt(u64::from(verified)),
                )])
                .encode()
                .into_bytes();
                Response::new(200, "application/json", body)
            }
            Err(e) => action_error(&e),
        },
        ("PUT", Some("priorities")) => match parse_priorities(&request.body) {
            Some(priorities) => action_result(
                lock(&daemon.registry)
                    .set_priorities(&info_hash, priorities)
                    .map(|_| ()),
            ),
            None => error(
                400,
                "priorities body must be comma-separated values of 0, 1, or 2",
            ),
        },
        _ => error(405, "method not allowed"),
    }
}

fn ok_json() -> Response {
    Response::new(200, "application/json", b"{\"ok\":true}".to_vec())
}

fn action_result(result: Result<(), ActionError>) -> Response {
    match result {
        Ok(()) => ok_json(),
        Err(e) => action_error(&e),
    }
}

fn action_error(e: &ActionError) -> Response {
    match e {
        ActionError::NotFound => error(404, "no such torrent"),
        ActionError::BadInput(what) => error(400, what),
        ActionError::Io(io) => error(500, &io.to_string()),
    }
}

/// Parse a comma-separated priorities body (`1,0,2`) into per-file bytes.
fn parse_priorities(body: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut out = Vec::new();
    for part in text.trim().split(',') {
        let value: u8 = part.trim().parse().ok()?;
        if value > 2 {
            return None;
        }
        out.push(value);
    }
    Some(out)
}

/// Whether a query string carries a truthy `data` flag (`data`, `data=1`,
/// `data=true`, `data=yes`).
fn query_has_data(query: &str) -> bool {
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "1"));
        key == "data" && matches!(value, "1" | "true" | "yes")
    })
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
        (
            "torrents".to_owned(),
            Value::UInt(u64::try_from(lock(&daemon.registry).count()).unwrap_or(u64::MAX)),
        ),
        // Placeholder until the SAM supervisor is wired.
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
            let token = registry::hex(&raw);
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
