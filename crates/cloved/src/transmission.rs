//! A Transmission-compatible RPC surface, so the frontends people already run
//! can drive `cloved` (`docs/PHASE-I.md`).
//!
//! `SCOPE.md` §3 listed this as a v2 candidate — *"worth it for \*arr-style
//! tooling, not worth the constraint now"* — and `DECISIONS.md` S3 records why
//! it moved. The short version: `SCOPE.md` §2 defers a web UI indefinitely, and
//! this is how clove gets a graphical interface without one, at zero new
//! dependencies. tremc, transgui, Transdroid and Flood all speak it.
//!
//! # What this is, and what it is not
//!
//! It is a **presentation layer**. It reads the same JSON `/v1/` emits, reshapes
//! it into Transmission's vocabulary, and turns Transmission's methods back into
//! the registry calls `/v1/` already makes. It holds no engine state, owns
//! nothing a restart would miss, and can be deleted in one commit.
//!
//! Critically, it **never constructs a type**. Transmission's model is
//! IP-shaped — `peers[].port`, `peer-port`, `port-test`, `blocklist-*` — and
//! `SCOPE.md` §5 Layer 1 forbids IP vocabulary in every crate but `i2pnet`,
//! enforced by `clippy.toml` `disallowed_types` and `ci/check-net-deps.sh`.
//! Everything IP-shaped here is a string or a constant, so both gates pass
//! **unchanged**. If landing this had required editing either one, the design
//! would have been wrong.
//!
//! The rule that follows from that, and the one to review against: *no field is
//! invented that a client would act on.* Reporting a real `rateDownload` is
//! compatibility. Fabricating a plausible `peers[].port` would be a bug. Where
//! clove has no answer, the field is a documented constant (`peer-port` is 0,
//! because I2P has no ports) or it is absent (`peers`, because the daemon does
//! not volunteer peer destinations — see [`TORRENT_FIELDS`]).
//!
//! # The one property this trades away
//!
//! `PHASE-F.md` §2 states that the daemon never parses JSON: commands reach it
//! as method + path + typed bodies. An RPC envelope is JSON, so that stops being
//! true the moment this is enabled. It is a deliberate trade, written down
//! rather than quietly falsified. What holds the risk down: the parser is
//! [`clove_core::json::parse`], which is depth-capped, hostile-input hardened
//! and already carries a fuzz target; `MAX_REQUEST_BODY` still bounds the body;
//! and this surface is off unless `transmission_rpc yes` is configured.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use clove_core::base64;
use clove_core::http::{Response, ServerRequest};
use clove_core::json::{self, Value};

/// The path this surface answers on. Transmission's own default, which is what
/// every client fills in for you.
pub(crate) const PATH: &str = "/transmission/rpc";

/// Whether `path` addresses this surface.
///
/// **A trailing slash is part of the protocol in practice.** `transmission
/// -remote` 4.0.5 sends `POST /transmission/rpc/`, not `/transmission/rpc`, and
/// an exact match against [`PATH`] therefore answered a real client with
/// `/v1/`'s "missing or invalid API token" — a 401 about a header it has no
/// reason to send, on a surface it could not tell it had failed to reach.
///
/// Found by running the client rather than by testing the parser, which is the
/// pattern the whole of `docs/PROTOCOL.i2p-bt` is written in: every defect that
/// has mattered here came from contact with a real implementation, and the unit
/// suite was green through this one too.
pub(crate) fn is_rpc_path(path: &str) -> bool {
    path == PATH || path.strip_suffix('/') == Some(PATH)
}

/// The RPC protocol version we claim. 18 is Transmission 4.0's; clients gate
/// features on it, so it has to name a version whose *shape* we implement.
const RPC_VERSION: i64 = 18;

/// The oldest protocol version we accept clients speaking.
const RPC_VERSION_MINIMUM: i64 = 14;

/// What `session-get` reports as `version`.
///
/// It has to be Transmission-shaped or clients refuse to talk, and it carries
/// clove's own identity so that no human reading the field is deceived about
/// what they are connected to. `clove-api(7)` says so too.
const VERSION: &str = concat!("4.0.0 (clove ", env!("CARGO_PKG_VERSION"), ")");

/// Transmission's torrent status codes.
mod status {
    /// Paused.
    pub(super) const STOPPED: i64 = 0;
    /// Hashing.
    pub(super) const CHECK: i64 = 2;
    /// Wants to download, is not.
    pub(super) const DOWNLOAD_WAIT: i64 = 3;
    /// Downloading.
    pub(super) const DOWNLOAD: i64 = 4;
    /// Wants to seed, is not.
    pub(super) const SEED_WAIT: i64 = 5;
    /// Seeding.
    pub(super) const SEED: i64 = 6;
}

/// Transmission's `error` codes. Only two of the four can arise here: clove has
/// no blocklist and does not distinguish a tracker warning from a failure.
mod error_code {
    /// Nothing wrong.
    pub(super) const OK: i64 = 0;
    /// The last announce failed; `errorString` says how.
    pub(super) const TRACKER_ERROR: i64 = 2;
    /// Something local stopped the torrent.
    pub(super) const LOCAL_ERROR: i64 = 3;
}

/// The `torrent-get` fields this surface answers, and the shape of the answer.
///
/// Everything here is either real data from the registry or a constant that is
/// true of every I2P torrent. Requested fields not on this list are **omitted**
/// from the response, which is what Transmission itself does with fields it does
/// not know, and what every client tolerates.
///
/// Deliberately absent, with reasons:
///
/// - **`peers`** — the per-peer array. `SECURITY.md` puts *"leaking the client's
///   destination, or a peer's, to somewhere it does not belong — including
///   logs, error messages, or the local API"* in scope as a vulnerability, and
///   `registry.rs` carries a test asserting a peer's address never reaches the
///   API. Filling this array is that leak, on request, by design. It is
///   omitted: `peersConnected` is real and is what list views actually render,
///   and a GUI's peer tab shows nothing, which reads as "no data" rather than
///   as plausible-but-wrong data. *Trigger for revisiting:* an operator wanting
///   their own node's peer table can have one behind an explicit opt-in — it is
///   their view of their own daemon — but that needs its own argument, not a
///   corner of this.
/// - **`pieces`** — the base64 bitfield. Large, polled often, and no client
///   needs it to function.
/// - **`webseeds`**, **`magnetLink`**, **`comment`**, **`creator`** — clove
///   either has no such concept or does not retain the field.
const TORRENT_FIELDS: &[&str] = &[
    "activityDate",
    "addedDate",
    "bandwidthPriority",
    "downloadDir",
    "downloadLimit",
    "downloadLimited",
    "downloadedEver",
    "error",
    "errorString",
    "eta",
    "fileCount",
    "fileStats",
    "files",
    "hashString",
    "haveUnchecked",
    "haveValid",
    "honorsSessionLimits",
    "id",
    "isFinished",
    "isPrivate",
    "isStalled",
    "labels",
    "leftUntilDone",
    "metadataPercentComplete",
    "name",
    "peer-limit",
    "peersConnected",
    "peersFrom",
    "peersGettingFromUs",
    "peersSendingToUs",
    "percentComplete",
    "percentDone",
    "pieceCount",
    "pieceSize",
    "priorities",
    "queuePosition",
    "rateDownload",
    "rateUpload",
    "recheckProgress",
    "secondsSeeding",
    "seedIdleLimit",
    "seedIdleMode",
    "seedRatioLimit",
    "seedRatioMode",
    "sequentialDownload",
    "sizeWhenDone",
    "status",
    "totalSize",
    "trackerStats",
    "trackers",
    "uploadLimit",
    "uploadLimited",
    "uploadRatio",
    "uploadedEver",
    "wanted",
];

/// The Transmission surface's own state: the CSRF token it hands out, and the
/// integer ids it lends to torrents.
pub(crate) struct Rpc {
    /// The `X-Transmission-Session-Id` value for this daemon run.
    session_id: String,
    /// Where torrents land, reported as `download-dir`.
    download_dir: String,
    ids: Mutex<Ids>,
}

/// The integer id ↔ info-hash mapping.
///
/// Transmission hands clients small integers and clove is keyed by info-hash,
/// so something has to bridge them. Ids are assigned on first sight, monotonic,
/// and **not persisted**: Transmission's own ids do not survive a restart
/// either, so every client already copes with them changing, and persisting
/// them would be a state file whose only job is to agree with another one.
#[derive(Default)]
struct Ids {
    next: i64,
    by_hash: BTreeMap<[u8; 20], i64>,
    by_id: BTreeMap<i64, [u8; 20]>,
    /// What the previous `torrent-get` reported, so `recently-active` can name
    /// what has gone since. Without it a client using that mode never learns a
    /// torrent was removed and shows it for ever.
    last_seen: BTreeSet<[u8; 20]>,
}

impl Ids {
    /// This torrent's id, assigning one if it does not have it yet.
    fn id_for(&mut self, info_hash: [u8; 20]) -> i64 {
        if let Some(id) = self.by_hash.get(&info_hash) {
            return *id;
        }
        // Starts at 1: Transmission's ids do, and a client that treats 0 as
        // "unset" is a bug report we do not need to field.
        self.next += 1;
        let id = self.next;
        self.by_hash.insert(info_hash, id);
        self.by_id.insert(id, info_hash);
        id
    }
}

impl Rpc {
    /// Build the surface, generating this run's session id.
    ///
    /// # Errors
    ///
    /// If the system random source fails, the same way the API token's does.
    pub(crate) fn new(download_dir: &std::path::Path) -> std::io::Result<Rpc> {
        let mut raw = [0u8; 16];
        getrandom::getrandom(&mut raw)
            .map_err(|e| std::io::Error::other(format!("getrandom: {e}")))?;
        Ok(Rpc {
            session_id: crate::registry::hex(&raw),
            download_dir: download_dir.to_string_lossy().into_owned(),
            ids: Mutex::new(Ids::default()),
        })
    }

    /// This run's `X-Transmission-Session-Id`.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Whether `password` is the daemon's API token, compared without leaking
/// where it first differs.
fn password_matches(password: &str, token: &str) -> bool {
    crate::is_well_formed_token(token)
        && crate::constant_time_eq(password.as_bytes(), token.as_bytes())
}

/// Check a request's credentials, returning the response to send if they do not
/// pass.
///
/// Two gates, in this order:
///
/// 1. **HTTP Basic.** The password is the daemon's existing API token — no new
///    secret, no new file, and one thing to rotate. The username is ignored, as
///    Transmission's own does not distinguish users either.
/// 2. **The CSRF session id.** A request without a matching
///    `X-Transmission-Session-Id` gets `409` carrying the right one, and the
///    client retries. This is not optional politeness: clients *depend* on the
///    409 to learn the id, and one that never sees it never sends it.
///
/// Order matters. Handing the session id to an unauthenticated caller would let
/// anyone who can reach the socket collect the CSRF token for free, which is
/// most of what it is for.
pub(crate) fn authorize(rpc: &Rpc, request: &ServerRequest, token: &str) -> Result<(), Response> {
    let authorized = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| base64::decode(encoded.trim()))
        .and_then(|raw| String::from_utf8(raw).ok())
        .is_some_and(|pair| {
            // `user:password`, and a password may itself contain a colon, so
            // split once from the left.
            let password = pair.split_once(':').map_or("", |(_, p)| p);
            password_matches(password, token)
        });
    if !authorized {
        let mut response = error_response(401, "authentication required");
        response.headers.push((
            "www-authenticate".to_owned(),
            "Basic realm=\"clove\"".to_owned(),
        ));
        return Err(response);
    }

    let presented = request.header("x-transmission-session-id").unwrap_or("");
    if !crate::constant_time_eq(presented.as_bytes(), rpc.session_id().as_bytes()) {
        let mut response = error_response(
            409,
            "missing or stale X-Transmission-Session-Id; retry with the one in this response",
        );
        response.headers.push((
            "x-transmission-session-id".to_owned(),
            rpc.session_id().to_owned(),
        ));
        return Err(response);
    }
    Ok(())
}

/// An error body in the shape a *browser* expects, for the pre-authentication
/// failures that happen before we know a `tag` to echo.
fn error_response(status: u16, message: &str) -> Response {
    Response::new(status, "text/plain", message.as_bytes().to_vec())
}

/// A successful RPC reply.
fn reply(tag: Option<i64>, arguments: Value) -> Response {
    envelope(tag, "success", arguments)
}

/// A failed RPC reply. Transmission signals failure in the `result` string with
/// HTTP 200, not in the status code, and clients read it there.
fn failure(tag: Option<i64>, reason: &str) -> Response {
    envelope(tag, reason, Value::Object(Vec::new()))
}

fn envelope(tag: Option<i64>, result: &str, arguments: Value) -> Response {
    let mut fields = vec![
        ("result".to_owned(), Value::from(result)),
        ("arguments".to_owned(), arguments),
    ];
    // Echoed only when the client sent one. Transmission omits it otherwise,
    // and a client that matches replies to requests by tag must not be handed
    // a tag it never used.
    if let Some(tag) = tag {
        fields.push(("tag".to_owned(), Value::Int(tag)));
    }
    Response::new(
        200,
        "application/json",
        Value::Object(fields).encode().into_bytes(),
    )
}

/// Read helpers over a JSON object. Every one has a total answer, because a
/// missing field in a response we built ourselves is a bug in *this* file and
/// should render as an obvious zero rather than take the daemon down.
fn get_u64(object: &Value, key: &str) -> u64 {
    object.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn get_f64(object: &Value, key: &str) -> f64 {
    object.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn get_str<'a>(object: &'a Value, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or("")
}

fn get_bool(object: &Value, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Saturating `u64` → `i64`, for the many Transmission fields that are signed
/// integers over quantities that cannot be negative.
fn signed(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Saturating `usize` → `i64`, likewise for counts and limits.
fn signed_len(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A Transmission ratio — a float — as the thousandths clove stores.
///
/// The direction that matters for exactness is the *other* one, which is why
/// clove keeps thousandths at all (`PHASE-H.md` §5: a ratio round-tripped
/// through an `f64` comes back as 1.499 where the operator wrote 1.5). Coming
/// in from a client the float is all there is, so it is rounded once, here,
/// and never again.
///
/// Anything not finite, negative, or past the ceiling `clove.conf(5)` accepts
/// clamps rather than wrapping: a client's slider should not be able to set a
/// ratio the config file would have refused.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "guarded above: finite, positive, and below MAX_SEED_RATIO_MILLI"
)]
fn ratio_to_milli(ratio: f64) -> u64 {
    if !ratio.is_finite() || ratio <= 0.0 {
        // Transmission's "unlimited", and clove's.
        return 0;
    }
    let scaled = (ratio * 1000.0).round();
    let ceiling = clove_core::config::MAX_SEED_RATIO_MILLI;
    if scaled >= from_milli(ceiling) * 1000.0 {
        ceiling
    } else {
        scaled as u64
    }
}

/// Thousandths as the float Transmission's ratio fields want.
///
/// Exact for every ratio anyone configures: the division happens in `f64`, but
/// a value small enough to be a seed ratio is far inside the mantissa's exact
/// integer range on the way in.
#[allow(
    clippy::cast_precision_loss,
    reason = "ratios are thousandths of a small number, exact in an f64 mantissa"
)]
fn from_milli(milli: u64) -> f64 {
    milli as f64 / 1000.0
}

/// Read a JSON number as a signed integer, whichever numeric form the parser
/// chose for it.
///
/// Clients are inconsistent about this — a tag or a torrent id arrives as `3`
/// from one and `3.0` from another — so both are accepted. A number with a
/// real fractional part is neither an id nor a tag, and is refused rather than
/// truncated into one.
#[allow(
    clippy::cast_possible_truncation,
    reason = "guarded: integral, and within i64's exactly-representable range"
)]
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        Value::UInt(n) => i64::try_from(*n).ok(),
        Value::Float(f) if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 => {
            Some(*f as i64)
        }
        _ => None,
    }
}

/// Serve one RPC request.
///
/// Authentication has already passed; this is dispatch only. Everything it can
/// return is an HTTP 200 carrying a `result` string, because that is where
/// Transmission clients look for failure — a 4xx here is read as "the server is
/// broken", not "that request was wrong".
pub(crate) fn dispatch(
    daemon: &std::sync::Arc<crate::Daemon>,
    request: &ServerRequest,
) -> Response {
    if request.method != "POST" {
        return failure(None, "the RPC endpoint takes POST");
    }
    let Ok(text) = std::str::from_utf8(&request.body) else {
        return failure(None, "request body is not UTF-8");
    };
    let Ok(envelope) = json::parse(text) else {
        return failure(None, "request body is not valid JSON");
    };
    // Read the tag before anything can fail on the method, so even a rejected
    // request comes back matched to what the client sent.
    let tag = envelope.get("tag").and_then(as_int);
    let Some(method) = envelope.get("method").and_then(Value::as_str) else {
        return failure(tag, "no method named");
    };
    let empty = Value::Object(Vec::new());
    let args = envelope.get("arguments").unwrap_or(&empty);

    match method {
        "torrent-get" => torrent_get(daemon, args, tag),
        "torrent-add" => torrent_add(daemon, args, tag),
        "torrent-remove" => torrent_remove(daemon, args, tag),
        "torrent-start" | "torrent-start-now" => lifecycle(daemon, args, tag, Lifecycle::Start),
        "torrent-stop" => lifecycle(daemon, args, tag, Lifecycle::Stop),
        "torrent-verify" => lifecycle(daemon, args, tag, Lifecycle::Verify),
        "torrent-reannounce" => lifecycle(daemon, args, tag, Lifecycle::Announce),
        "torrent-set" => torrent_set(daemon, args, tag),
        "session-get" => reply(tag, session_get(daemon)),
        "session-stats" => reply(tag, session_stats(daemon)),
        "free-space" => reply(tag, free_space(daemon)),
        "port-test" => reply(
            tag,
            // Honest rather than encouraging: there is no port to be open.
            // I2P reachability is the router's business and a client that
            // shows a red light here would be reporting on nothing.
            Value::Object(vec![("port-is-open".to_owned(), Value::Bool(false))]),
        ),
        "queue-move-top" | "queue-move-up" | "queue-move-down" | "queue-move-bottom" => {
            queue_move(daemon, args, tag, method)
        }
        "session-set" => failure(
            tag,
            "clove reads its settings from clove.conf and applies them at start; \
             session-set is not honoured. See clove.conf(5).",
        ),
        "torrent-set-location" | "torrent-rename-path" => failure(
            tag,
            "clove keeps every torrent under one downloads directory and cannot move or \
             rename data. See clove-api(7).",
        ),
        "blocklist-update" => failure(tag, "clove has no blocklist: peers are I2P destinations"),
        "session-close" => failure(
            tag,
            "stop the daemon with your service manager, not over RPC",
        ),
        other => failure(tag, &format!("unsupported method: {other}")),
    }
}

/// The lifecycle verbs that take only a set of ids.
#[derive(Clone, Copy)]
enum Lifecycle {
    Start,
    Stop,
    Verify,
    Announce,
}

/// Resolve an `ids` argument to info-hashes, against a listing already taken.
///
/// Transmission accepts an integer id, a 40-character hash string, an array
/// mixing both, the literal `"recently-active"`, or nothing at all — which
/// means *every* torrent, and is what a client's poll loop sends.
fn selected(rpc: &Rpc, args: &Value, listing: &[Value]) -> Vec<[u8; 20]> {
    let all = || {
        listing
            .iter()
            .filter_map(|t| crate::registry::parse_info_hash(get_str(t, "info_hash")))
            .collect::<Vec<_>>()
    };
    let Some(ids) = args.get("ids") else {
        return all();
    };
    if ids.as_str() == Some("recently-active") {
        // Over-reports: everything is "recently active". That is safe — a
        // client re-renders rows that did not change — where under-reporting
        // would leave a stale screen. The half of this mode that has to be
        // exact is `removed`, and that is exact.
        return all();
    }

    let one = |value: &Value| -> Option<[u8; 20]> {
        if let Some(text) = value.as_str() {
            return crate::registry::parse_info_hash(text);
        }
        let id = as_int(value)?;
        rpc.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_id
            .get(&id)
            .copied()
    };
    match ids {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        single => one(single).into_iter().collect(),
    }
}

/// `torrent-get`: the method a client calls every second or two.
fn torrent_get(daemon: &std::sync::Arc<crate::Daemon>, args: &Value, tag: Option<i64>) -> Response {
    let Some(rpc) = daemon.rpc.as_ref() else {
        return failure(tag, "the Transmission surface is not enabled");
    };
    let listing = crate::lock(&daemon.registry).list_detailed();
    let Value::Array(items) = listing else {
        return failure(tag, "internal: listing was not an array");
    };

    let requested: Vec<&str> = args
        .get("fields")
        .and_then(Value::as_array)
        .map(|f| f.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // A client that names no fields gets the lot rather than an empty row,
    // which is friendlier for anyone poking at this with curl.
    let fields: Vec<&str> = if requested.is_empty() {
        TORRENT_FIELDS.to_vec()
    } else {
        requested
    };

    let wanted = selected(rpc, args, &items);
    let mut present = BTreeSet::new();
    let mut out = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let Some(info_hash) = crate::registry::parse_info_hash(get_str(item, "info_hash")) else {
            continue;
        };
        present.insert(info_hash);
        if !wanted.contains(&info_hash) {
            continue;
        }
        let id = rpc
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .id_for(info_hash);
        out.push(torrent_object(
            item,
            id,
            i64::try_from(position).unwrap_or(i64::MAX),
            &rpc.download_dir,
            &fields,
        ));
    }

    // What has gone since the last poll. Exact, because a client in
    // `recently-active` mode learns of a removal only here.
    let removed: Vec<Value> = {
        let mut ids = rpc
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gone: Vec<[u8; 20]> = ids.last_seen.difference(&present).copied().collect();
        ids.last_seen = present;
        gone.iter()
            .filter_map(|hash| ids.by_hash.get(hash).copied())
            .map(Value::Int)
            .collect()
    };

    reply(
        tag,
        Value::Object(vec![
            ("torrents".to_owned(), Value::Array(out)),
            ("removed".to_owned(), Value::Array(removed)),
        ]),
    )
}

/// One torrent, rendered into the fields the client asked for.
///
/// `view` is the object `/v1/torrents/{ih}` would return, so there is exactly
/// one place torrent facts are computed and this only restates them.
#[allow(
    clippy::too_many_lines,
    reason = "one field table; splitting it hides it"
)]
fn torrent_object(
    view: &Value,
    id: i64,
    position: i64,
    download_dir: &str,
    fields: &[&str],
) -> Value {
    let state = get_str(view, "state");
    let pending = state == "fetching-metadata";
    let progress = get_f64(view, "progress");
    let total = get_u64(view, "size");
    let up_rate = get_u64(view, "up_rate");
    let down_rate = get_u64(view, "down_rate");

    // Bytes the operator actually asked for: files set to skip are not part of
    // what "done" means, exactly as `Hosted::wanted_and_held` decides it for
    // the state string. Falls back to the total when there is no file list yet.
    let files = view.get("files").and_then(Value::as_array);
    let size_when_done = files.map_or(total, |list| {
        list.iter()
            .filter(|f| get_u64(f, "priority") > 0)
            .map(|f| get_u64(f, "length"))
            .sum()
    });
    // Piece-granular, because `progress` is: it counts whole verified pieces
    // over wanted pieces. A byte-exact figure would need per-file accounting
    // the engine does not keep, and this is the same truth at the resolution
    // clove actually knows it.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "byte counts within an f64's exact range, clamped below"
    )]
    let have_bytes = ((size_when_done as f64) * progress.clamp(0.0, 1.0)) as u64;
    let left = size_when_done.saturating_sub(have_bytes);
    let complete = matches!(state, "seeding" | "complete") || (left == 0 && !pending);

    let eta = if complete || down_rate == 0 || pending {
        -1
    } else {
        signed(left / down_rate)
    };

    let value = |field: &str| -> Option<Value> {
        Some(match field {
            "id" => Value::Int(id),
            "hashString" => Value::from(get_str(view, "info_hash")),
            "name" => Value::from(get_str(view, "name")),
            "queuePosition" => Value::Int(position),
            "downloadDir" => Value::from(download_dir),
            "addedDate" => Value::Int(signed(get_u64(view, "added") / 1000)),
            // clove does not record a last-activity time. Reporting "now"
            // while bytes are moving and the add time otherwise is coarse but
            // true; it is not a stored timestamp and `clove-api(7)` says so.
            "activityDate" => Value::Int(if up_rate + down_rate > 0 {
                signed(now_secs())
            } else {
                signed(get_u64(view, "added") / 1000)
            }),
            "status" => Value::Int(status_of(state, complete)),
            "error" => Value::Int(error_of(view, state)),
            "errorString" => Value::from(error_string(view, state)),
            "isFinished" => Value::Bool(complete),
            // "Stalled" is Transmission's word for wanted, running, and moving
            // nothing. On I2P that is a normal minute, not a fault, so it is
            // reported but never dressed up as an error.
            "isStalled" => Value::Bool(state == "downloading" && down_rate == 0),
            "isPrivate" => Value::Bool(get_bool(view, "private")),
            "percentDone" | "percentComplete" => Value::Float(progress),
            "metadataPercentComplete" => Value::Float(if pending { 0.0 } else { 1.0 }),
            // clove's hash pass does not report how far through it is, so
            // this is 0 during a verify as well as outside one. A client shows
            // an empty bar rather than a moving one.
            "recheckProgress" => Value::Float(0.0),
            "totalSize" => Value::Int(signed(total)),
            "sizeWhenDone" => Value::Int(signed(size_when_done)),
            "leftUntilDone" => Value::Int(signed(left)),
            "haveValid" => Value::Int(signed(have_bytes)),

            "pieceCount" => Value::Int(signed(get_u64(view, "pieces"))),
            "pieceSize" => Value::Int(signed(get_u64(view, "piece_length"))),
            "rateDownload" => Value::Int(signed(down_rate)),
            "rateUpload" => Value::Int(signed(up_rate)),
            "downloadedEver" => Value::Int(signed(get_u64(view, "downloaded"))),
            "uploadedEver" => Value::Int(signed(get_u64(view, "uploaded"))),
            "uploadRatio" => Value::Float(ratio_of(view)),
            "eta" => Value::Int(eta),
            "peersConnected" => Value::Int(signed(get_u64(view, "peers"))),

            "peersFrom" => peers_from(view),
            "files" => file_list(files),
            "fileStats" => file_stats(files),
            "fileCount" => Value::Int(signed_len(files.map_or(0, <[Value]>::len))),
            "wanted" => Value::Array(
                files
                    .map(|list| {
                        list.iter()
                            .map(|f| Value::Bool(get_u64(f, "priority") > 0))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            "priorities" => Value::Array(
                files
                    .map(|list| {
                        list.iter()
                            .map(|f| Value::Int(to_transmission_priority(get_u64(f, "priority"))))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            "trackers" => tracker_list(view),
            "trackerStats" => tracker_stats(view),
            "sequentialDownload" => Value::Bool(get_bool(view, "sequential")),
            "seedRatioLimit" => Value::Float(from_milli(get_u64(view, "seed_ratio"))),
            // 0 = follow the session's limit, 1 = use this torrent's own.
            "seedRatioMode" => Value::Int(i64::from(get_u64(view, "seed_ratio") > 0)),
            // Every remaining integer clove has no answer for, gathered into
            // one arm rather than repeated:
            //
            // - `haveUnchecked`: everything clove holds has been SHA-1'd, so
            //   there is no unchecked category.
            // - `peersGettingFromUs`/`peersSendingToUs`: clove counts peers,
            //   not directions. Reporting the connected count under both would
            //   double it in a client's summary; the true number is in
            //   `peersConnected`.
            // - `seedIdleLimit`/`seedIdleMode`: the idle limit is a daemon
            //   setting, reported by `session-get`, not a per-torrent one.
            // - the rate limits and `bandwidthPriority`: deferred with a
            //   trigger (`PHASE-H.md` §11); 0 is Transmission's "unlimited".
            // - `secondsSeeding`: not recorded. Absent would be cleaner, but
            //   clients sort on it and a missing key upsets a couple of them.
            "haveUnchecked"
            | "peersGettingFromUs"
            | "peersSendingToUs"
            | "seedIdleLimit"
            | "seedIdleMode"
            | "uploadLimit"
            | "downloadLimit"
            | "bandwidthPriority"
            // - `peer-limit`: clove's per-torrent ceiling is a daemon
            //   setting, not a per-torrent one; `session-get` reports it.
            | "secondsSeeding"
            | "peer-limit" => Value::Int(0),
            "uploadLimited" | "downloadLimited" => Value::Bool(false),
            "honorsSessionLimits" => Value::Bool(true),
            // clove has no labels (`PHASE-H.md` §11 defers them with a
            // trigger); an empty array is the true answer, not a placeholder.
            "labels" => Value::Array(Vec::new()),
            _ => return None,
        })
    };

    Value::Object(
        fields
            .iter()
            .filter_map(|field| value(field).map(|v| ((*field).to_owned(), v)))
            .collect(),
    )
}

/// Seconds since the Unix epoch, or 0 if the clock is before it.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// clove's state string as a Transmission status code.
fn status_of(state: &str, complete: bool) -> i64 {
    match state {
        "verifying" => status::CHECK,
        "seeding" => status::SEED,
        "downloading" | "fetching-metadata" => status::DOWNLOAD,
        // Wanted but not running, for one of two reasons clove distinguishes
        // and Transmission does not. Both map to a "waiting" status, chosen by
        // which budget the torrent is waiting on.
        "queued" | "waiting-for-router" | "complete" => {
            if complete {
                status::SEED_WAIT
            } else {
                status::DOWNLOAD_WAIT
            }
        }
        // "paused", and any state a later clove teaches the registry that
        // nobody taught this. Stopped is the safe reading of an unknown state:
        // a client acts on it by showing the torrent as not running, which is
        // true of everything that is not one of the above.
        _ => status::STOPPED,
    }
}

/// Whether this torrent has something wrong with it, in Transmission's terms.
fn error_of(view: &Value, state: &str) -> i64 {
    if state == "paused" && !get_str(view, "paused_because").is_empty() {
        // Stopped by a rule rather than by the operator. Not an error in
        // clove's terms, but this is the only field a client will show the
        // reason in, and a silent stop is the bug report this avoids.
        return error_code::LOCAL_ERROR;
    }
    if get_str(view, "last_announce_error").is_empty() {
        error_code::OK
    } else {
        error_code::TRACKER_ERROR
    }
}

/// The human half of the above.
fn error_string(view: &Value, state: &str) -> String {
    let why = get_str(view, "paused_because");
    if state == "paused" && !why.is_empty() {
        return why.to_owned();
    }
    get_str(view, "last_announce_error").to_owned()
}

/// The torrent's ratio as a float, from the thousandths the registry keeps.
fn ratio_of(view: &Value) -> f64 {
    let downloaded = get_u64(view, "downloaded");
    if downloaded == 0 {
        // Transmission's sentinel for "no ratio": nothing was downloaded, so
        // uploaded/downloaded is not a number. -1 renders as a dash; 0 would
        // render as a torrent that has never uploaded, which is different.
        return -1.0;
    }
    from_milli(get_u64(view, "ratio"))
}

/// Where this torrent's peers came from. Three of clove's sources map onto
/// Transmission's; the rest have no I2P equivalent and are honestly zero.
fn peers_from(view: &Value) -> Value {
    let pex = signed(get_u64(view, "pex_peers"));
    let incoming = signed(get_u64(view, "inbound_peers"));
    let known = signed(get_u64(view, "known_peers"));
    Value::Object(vec![
        ("fromPex".to_owned(), Value::Int(pex)),
        ("fromIncoming".to_owned(), Value::Int(incoming)),
        ("fromTracker".to_owned(), Value::Int((known - pex).max(0))),
        ("fromDht".to_owned(), Value::Int(0)),
        ("fromLpd".to_owned(), Value::Int(0)),
        ("fromLtep".to_owned(), Value::Int(0)),
        ("fromCache".to_owned(), Value::Int(0)),
    ])
}

/// clove's file priority (0 skip, 1 normal, 2 high) as Transmission's
/// (-1 low, 0 normal, 1 high). A skipped file keeps a normal priority and is
/// marked unwanted instead, which is how Transmission spells the same thing.
fn to_transmission_priority(clove: u64) -> i64 {
    match clove {
        2 => 1,
        _ => 0,
    }
}

/// Transmission's file priority as clove's. clove has no "low", so low and
/// normal both land on normal — stated rather than silently rounded.
fn from_transmission_priority(transmission: i64) -> u8 {
    if transmission >= 1 { 2 } else { 1 }
}

fn file_list(files: Option<&[Value]>) -> Value {
    Value::Array(
        files
            .map(|list| {
                list.iter()
                    .map(|f| {
                        let length = signed(get_u64(f, "length"));
                        Value::Object(vec![
                            ("name".to_owned(), Value::from(get_str(f, "path"))),
                            ("length".to_owned(), Value::Int(length)),
                            // Per-file completion is not tracked; a file's
                            // pieces are the torrent's pieces. Reported as the
                            // whole length once done and 0 otherwise would be a
                            // worse lie than 0, which reads as "not known".
                            ("bytesCompleted".to_owned(), Value::Int(0)),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn file_stats(files: Option<&[Value]>) -> Value {
    Value::Array(
        files
            .map(|list| {
                list.iter()
                    .map(|f| {
                        let priority = get_u64(f, "priority");
                        Value::Object(vec![
                            ("bytesCompleted".to_owned(), Value::Int(0)),
                            ("wanted".to_owned(), Value::Bool(priority > 0)),
                            (
                                "priority".to_owned(),
                                Value::Int(to_transmission_priority(priority)),
                            ),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn tracker_list(view: &Value) -> Value {
    let trackers = view.get("trackers").and_then(Value::as_array);
    Value::Array(
        trackers
            .map(|list| {
                list.iter()
                    .enumerate()
                    .map(|(i, url)| {
                        Value::Object(vec![
                            ("id".to_owned(), Value::Int(i64::try_from(i).unwrap_or(0))),
                            (
                                "announce".to_owned(),
                                Value::from(url.as_str().unwrap_or("")),
                            ),
                            ("scrape".to_owned(), Value::from("")),
                            // clove tracks announce URLs independently rather
                            // than in strict BEP 12 tiers (`PHASE-F.md` §7 5d),
                            // so every tracker reports as its own tier.
                            ("tier".to_owned(), Value::Int(i64::try_from(i).unwrap_or(0))),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn tracker_stats(view: &Value) -> Value {
    let trackers = view.get("trackers").and_then(Value::as_array);
    let last_error = get_str(view, "last_announce_error");
    // clove counts announces per torrent, not per tracker, so the counters are
    // the torrent's and every tracker row carries the same pair. Stated in
    // `clove-api(7)`; the alternative was to attribute them to the first
    // tracker and report zero for the rest, which is wrong more precisely.
    let ok = signed(get_u64(view, "announces_ok"));
    let failed = signed(get_u64(view, "announces_failed"));
    let announced = ok + failed > 0;
    Value::Array(
        trackers
            .map(|list| {
                list.iter()
                    .enumerate()
                    .map(|(i, url)| {
                        let announce = url.as_str().unwrap_or("");
                        let id = i64::try_from(i).unwrap_or(0);
                        Value::Object(vec![
                            ("id".to_owned(), Value::Int(id)),
                            ("announce".to_owned(), Value::from(announce)),
                            ("host".to_owned(), Value::from(origin_of(announce))),
                            ("sitename".to_owned(), Value::from(sitename_of(announce))),
                            // clove tracks announce URLs independently rather
                            // than in strict BEP 12 tiers (`PHASE-F.md` §7 5d),
                            // so every tracker reports as its own tier and none
                            // is a backup for another.
                            ("tier".to_owned(), Value::Int(id)),
                            ("isBackup".to_owned(), Value::Bool(false)),
                            // 1 = waiting for its next announce, which is what
                            // a torrent between announces is doing.
                            ("announceState".to_owned(), Value::Int(1)),
                            ("hasAnnounced".to_owned(), Value::Bool(announced)),
                            ("lastAnnounceSucceeded".to_owned(), Value::Bool(ok > 0)),
                            ("lastAnnounceTimedOut".to_owned(), Value::Bool(false)),
                            ("lastAnnouncePeerCount".to_owned(), Value::Int(0)),
                            ("lastAnnounceResult".to_owned(), Value::from(last_error)),
                            // clove keeps no announce timestamps, and these are
                            // the fields a client renders a tracker row from at
                            // all: 0 is Transmission's "never", which is a true
                            // statement about a time we did not record.
                            ("lastAnnounceTime".to_owned(), Value::Int(0)),
                            ("lastAnnounceStartTime".to_owned(), Value::Int(0)),
                            ("nextAnnounceTime".to_owned(), Value::Int(0)),
                            // Scrape is not implemented — clove announces only
                            // — so every scrape field says so rather than
                            // implying a scrape that failed.
                            ("scrape".to_owned(), Value::from("")),
                            ("scrapeState".to_owned(), Value::Int(0)),
                            ("hasScraped".to_owned(), Value::Bool(false)),
                            ("lastScrapeSucceeded".to_owned(), Value::Bool(false)),
                            ("lastScrapeTimedOut".to_owned(), Value::Bool(false)),
                            ("lastScrapeResult".to_owned(), Value::from("")),
                            ("lastScrapeTime".to_owned(), Value::Int(0)),
                            ("lastScrapeStartTime".to_owned(), Value::Int(0)),
                            ("nextScrapeTime".to_owned(), Value::Int(0)),
                            // -1 is Transmission's "unknown", and it is: these
                            // come from a scrape clove never makes.
                            ("seederCount".to_owned(), Value::Int(-1)),
                            ("leecherCount".to_owned(), Value::Int(-1)),
                            ("downloadCount".to_owned(), Value::Int(-1)),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// `scheme://host` of an announce URL, which is what Transmission's `host`
/// field means. Textual, because this crate has no URL type and wants none.
fn origin_of(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    format!("{scheme}://{host}")
}

/// The bare host, which Transmission 4 shows in its tracker list.
fn sitename_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#', ':']).next().unwrap_or(rest);
    host.to_owned()
}

/// `torrent-add`: a magnet in `filename`, or a base64 `.torrent` in `metainfo`.
fn torrent_add(daemon: &std::sync::Arc<crate::Daemon>, args: &Value, tag: Option<i64>) -> Response {
    let Some(rpc) = daemon.rpc.as_ref() else {
        return failure(tag, "the Transmission surface is not enabled");
    };

    // `download-dir` is honoured only when it names the one directory clove
    // has. Silently ignoring it would put files somewhere the client did not
    // ask for and then report that directory back as if it had agreed.
    if let Some(dir) = args.get("download-dir").and_then(Value::as_str)
        && dir.trim_end_matches('/') != rpc.download_dir.trim_end_matches('/')
    {
        {
            return failure(
                tag,
                &format!(
                    "clove keeps every torrent under {}; it cannot add one elsewhere",
                    rpc.download_dir
                ),
            );
        }
    }

    let body = match add_body(args) {
        Ok(body) => body,
        Err(reason) => return failure(tag, reason),
    };

    let options = crate::registry::AddOptions {
        // Transmission's own `paused` argument, meaning the same thing.
        paused: args.get("paused").and_then(Value::as_bool).unwrap_or(false),
        sequential: args
            .get("sequentialDownload")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };

    match crate::add_from_body(daemon, &body, options) {
        Ok(added) => {
            let info_hash = match added {
                crate::Added::Torrent(h) | crate::Added::Magnet(h) => h,
            };
            let id = rpc
                .ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .id_for(info_hash);
            let name = name_of(daemon, info_hash);
            reply(
                tag,
                Value::Object(vec![(
                    "torrent-added".to_owned(),
                    Value::Object(vec![
                        ("id".to_owned(), Value::Int(id)),
                        ("name".to_owned(), Value::from(name)),
                        (
                            "hashString".to_owned(),
                            Value::from(crate::registry::hex(&info_hash)),
                        ),
                    ]),
                )]),
            )
        }
        // Not an error to a client: it asked for a torrent to be present and
        // it is. Transmission answers with `torrent-duplicate` and every
        // client — the \*arr ones especially — relies on that to be idempotent.
        Err(crate::AddError::Duplicate) => {
            let info_hash = info_hash_of_body(&body);
            let (id, name) = match info_hash {
                Some(hash) => (
                    rpc.ids
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .id_for(hash),
                    name_of(daemon, hash),
                ),
                None => (0, String::new()),
            };
            reply(
                tag,
                Value::Object(vec![(
                    "torrent-duplicate".to_owned(),
                    Value::Object(vec![
                        ("id".to_owned(), Value::Int(id)),
                        ("name".to_owned(), Value::from(name)),
                        (
                            "hashString".to_owned(),
                            Value::from(
                                info_hash
                                    .map(|h| crate::registry::hex(&h))
                                    .unwrap_or_default(),
                            ),
                        ),
                    ]),
                )]),
            )
        }
        Err(e) => failure(tag, &e.to_string()),
    }
}

/// The bytes `torrent-add` was actually asked to add, or why it cannot be.
///
/// Transmission's `filename` accepts a magnet, a URL or a local path. Only the
/// first is servable here, and the refusal for a URL is the one in this file
/// that is not about convenience: fetching a torrent over HTTP is a clearnet
/// request, which clove is architecturally incapable of making (`SCOPE.md` §5)
/// and would not make if it could (§10). A local path is refused too — reading
/// an arbitrary file on behalf of whoever is on the socket is not a favour
/// worth doing, and Landlock would refuse most of them anyway.
fn add_body(args: &Value) -> Result<Vec<u8>, &'static str> {
    if let Some(filename) = args.get("filename").and_then(Value::as_str) {
        let filename = filename.trim();
        if filename.starts_with("magnet:") {
            return Ok(filename.as_bytes().to_vec());
        }
        if filename.starts_with("http://") || filename.starts_with("https://") {
            return Err(
                "clove cannot fetch a torrent from a URL: it has no clearnet access by \
                 design. Pass the file's bytes as `metainfo`, or a magnet link.",
            );
        }
        return Err("`filename` must be a magnet link; pass a .torrent as base64 in `metainfo`");
    }
    if let Some(encoded) = args.get("metainfo").and_then(Value::as_str) {
        return base64::decode(encoded).ok_or("`metainfo` is not valid base64");
    }
    Err("torrent-add needs `filename` (magnet) or `metainfo`")
}

/// The info-hash of an add body, for naming a duplicate the registry refused.
fn info_hash_of_body(body: &[u8]) -> Option<[u8; 20]> {
    if body.starts_with(b"magnet:") {
        let uri = std::str::from_utf8(body).ok()?;
        return clove_core::magnet::Magnet::parse(uri.trim())
            .ok()
            .map(|m| m.info_hash);
    }
    clove_core::metainfo::MetaInfo::parse(body)
        .ok()
        .map(|m| m.info_hash.0)
}

/// A torrent's display name, or its hash if it has not got one yet.
fn name_of(daemon: &std::sync::Arc<crate::Daemon>, info_hash: [u8; 20]) -> String {
    crate::lock(&daemon.registry)
        .detail(&info_hash)
        .as_ref()
        .map_or_else(
            || crate::registry::hex(&info_hash),
            |view| get_str(view, "name").to_owned(),
        )
}

/// `torrent-remove`, with `delete-local-data` meaning `clove remove --data`.
fn torrent_remove(
    daemon: &std::sync::Arc<crate::Daemon>,
    args: &Value,
    tag: Option<i64>,
) -> Response {
    let Some(rpc) = daemon.rpc.as_ref() else {
        return failure(tag, "the Transmission surface is not enabled");
    };
    let delete = args
        .get("delete-local-data")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let targets = resolve_ids(daemon, rpc, args);
    for info_hash in targets {
        // A torrent already gone is the outcome asked for, so `NotFound` is
        // not reported: a client retrying a removal must not see a failure for
        // having succeeded the first time.
        let _ = crate::lock(&daemon.registry).remove(&info_hash, delete);
    }
    reply(tag, Value::Object(Vec::new()))
}

/// The lifecycle methods, which differ only in the registry call they make.
fn lifecycle(
    daemon: &std::sync::Arc<crate::Daemon>,
    args: &Value,
    tag: Option<i64>,
    what: Lifecycle,
) -> Response {
    let Some(rpc) = daemon.rpc.as_ref() else {
        return failure(tag, "the Transmission surface is not enabled");
    };
    let targets = resolve_ids(daemon, rpc, args);
    let mut first_error = None;
    for info_hash in targets {
        let outcome = match what {
            Lifecycle::Start => crate::lock(&daemon.registry).set_paused(&info_hash, false),
            Lifecycle::Stop => crate::lock(&daemon.registry).set_paused(&info_hash, true),
            Lifecycle::Announce => crate::lock(&daemon.registry).announce_now(&info_hash),
            Lifecycle::Verify => {
                // `verify` is two steps and the second must happen with the
                // registry unlocked, exactly as `/v1/` does it: the hash pass
                // reads every byte, and holding the lock for it stops the
                // whole daemon.
                match crate::lock(&daemon.registry).begin_verify(&info_hash) {
                    Ok(job) => crate::run_scan(daemon, &job).map(|_| ()),
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(e) = outcome {
            first_error.get_or_insert(e.to_string());
        }
    }
    match first_error {
        Some(reason) => failure(tag, &reason),
        None => reply(tag, Value::Object(Vec::new())),
    }
}

/// Resolve the `ids` argument against a fresh listing.
fn resolve_ids(daemon: &std::sync::Arc<crate::Daemon>, rpc: &Rpc, args: &Value) -> Vec<[u8; 20]> {
    let listing = crate::lock(&daemon.registry).list();
    let items = match listing {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    selected(rpc, args, &items)
}

/// `torrent-set`: the per-torrent settings clove has an equivalent for.
fn torrent_set(daemon: &std::sync::Arc<crate::Daemon>, args: &Value, tag: Option<i64>) -> Response {
    let Some(rpc) = daemon.rpc.as_ref() else {
        return failure(tag, "the Transmission surface is not enabled");
    };
    let targets = resolve_ids(daemon, rpc, args);
    let mut first_error = None;

    for info_hash in targets {
        if let Some(sequential) = args.get("sequentialDownload").and_then(Value::as_bool)
            && let Err(e) = crate::lock(&daemon.registry).set_sequential(&info_hash, sequential)
        {
            first_error.get_or_insert(e.to_string());
        }
        if let Some(limit) = args.get("seedRatioLimit").and_then(Value::as_f64) {
            let milli = ratio_to_milli(limit);
            if let Err(e) = crate::lock(&daemon.registry).set_seed_ratio(&info_hash, milli) {
                first_error.get_or_insert(e.to_string());
            }
        }
        if let Some(priorities) = wanted_priorities(daemon, info_hash, args)
            && let Err(e) = crate::lock(&daemon.registry).set_priorities(&info_hash, priorities)
        {
            first_error.get_or_insert(e.to_string());
        }
    }

    match first_error {
        Some(reason) => failure(tag, &reason),
        None => reply(tag, Value::Object(Vec::new())),
    }
}

/// Fold `files-wanted` / `files-unwanted` / `priority-*` into a full priority
/// vector, or `None` if the request said nothing about files.
///
/// Transmission sends *deltas* — indices to change — so the current vector has
/// to be read first and edited, not rebuilt. Rebuilding is how "make file 3
/// high priority" quietly un-skips every file the operator had skipped.
fn wanted_priorities(
    daemon: &std::sync::Arc<crate::Daemon>,
    info_hash: [u8; 20],
    args: &Value,
) -> Option<Vec<u8>> {
    const KEYS: [&str; 5] = [
        "files-wanted",
        "files-unwanted",
        "priority-high",
        "priority-normal",
        "priority-low",
    ];
    if !KEYS.iter().any(|k| args.get(k).is_some()) {
        return None;
    }
    let view = crate::lock(&daemon.registry).detail(&info_hash)?;
    let files = view.get("files").and_then(Value::as_array)?;
    let mut priorities: Vec<u8> = files
        .iter()
        .map(|f| u8::try_from(get_u64(f, "priority")).unwrap_or(1))
        .collect();

    // Transmission sends *deltas*, and an empty array means "every file", so
    // each key is resolved against the vector we already have.
    let indices = |key: &str, len: usize| -> Option<Vec<usize>> {
        let listed = args.get(key)?;
        let chosen: Vec<usize> = listed
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_u64)
                    .filter_map(|i| usize::try_from(i).ok())
                    .collect()
            })
            .unwrap_or_default();
        Some(if chosen.is_empty() {
            (0..len).collect()
        } else {
            chosen
        })
    };

    // Wanted-ness first, then priority: a file named in both `files-wanted`
    // and `priority-high` should end up high, not merely wanted.
    let len = priorities.len();
    if let Some(chosen) = indices("files-unwanted", len) {
        for i in chosen {
            if let Some(slot) = priorities.get_mut(i) {
                *slot = 0;
            }
        }
    }
    if let Some(chosen) = indices("files-wanted", len) {
        for i in chosen {
            // Wanting a file back restores it to normal, not to whatever
            // priority it had before it was skipped — clove does not keep that,
            // and Transmission sends the priority separately when it means one.
            if let Some(slot) = priorities.get_mut(i)
                && *slot == 0
            {
                *slot = 1;
            }
        }
    }
    for (key, level) in [
        ("priority-low", -1i64),
        ("priority-normal", 0),
        ("priority-high", 1),
    ] {
        if let Some(chosen) = indices(key, len) {
            for i in chosen {
                // A file the operator skipped stays skipped: Transmission
                // spells "do not fetch this" as unwanted, not as a priority,
                // so a priority change must not un-skip it.
                if let Some(slot) = priorities.get_mut(i)
                    && *slot != 0
                {
                    *slot = from_transmission_priority(level);
                }
            }
        }
    }
    Some(priorities)
}

/// The queue-move family.
///
/// clove's queue position *is* its add order (`PHASE-H.md` §4: derived, not
/// stored), so there is no position to write. One of the four still has an
/// exact equivalent — moving to the top is "run this one now", which is what
/// `clove start` does — and the other three say plainly that they cannot be
/// honoured rather than reporting a success that changes nothing.
fn queue_move(
    daemon: &std::sync::Arc<crate::Daemon>,
    args: &Value,
    tag: Option<i64>,
    method: &str,
) -> Response {
    if method != "queue-move-top" {
        return failure(
            tag,
            "clove's queue order is the order torrents were added and cannot be rearranged; \
             queue-move-top (or `clove start`) forces one to run now. See clove-api(7).",
        );
    }
    let Some(rpc) = daemon.rpc.as_ref() else {
        return failure(tag, "the Transmission surface is not enabled");
    };
    let targets = resolve_ids(daemon, rpc, args);
    let mut first_error = None;
    for info_hash in targets {
        if let Err(e) = crate::lock(&daemon.registry).force_start(&info_hash) {
            first_error.get_or_insert(e.to_string());
        }
    }
    match first_error {
        Some(reason) => failure(tag, &reason),
        None => reply(tag, Value::Object(Vec::new())),
    }
}

/// `session-get`: what clove is configured to do, in Transmission's vocabulary.
///
/// Three kinds of key live here and it is worth knowing which is which when
/// reading a client's preferences dialog against this:
///
/// - **Real settings**, read from `clove.conf`: the peer ceilings, the queue
///   sizes, the seed limits, the downloads directory.
/// - **Honest constants** describing what clove *is*: no DHT, no local peer
///   discovery, no blocklist, no port, encryption always on — every one of
///   those is a true statement about an I2P-only client, not a stub.
/// - **`version` and `rpc-version`**, which name Transmission rather than
///   clove because clients gate features on them. `version` carries clove's
///   own version alongside so that a human reading the field is not deceived.
fn session_get(daemon: &std::sync::Arc<crate::Daemon>) -> Value {
    let config = &daemon.config;
    let download_dir = daemon
        .rpc
        .as_ref()
        .map(|rpc| rpc.download_dir.clone())
        .unwrap_or_default();
    Value::Object(vec![
        ("version".to_owned(), Value::from(VERSION)),
        ("rpc-version".to_owned(), Value::Int(RPC_VERSION)),
        (
            "rpc-version-minimum".to_owned(),
            Value::Int(RPC_VERSION_MINIMUM),
        ),
        ("download-dir".to_owned(), Value::from(download_dir)),
        ("incomplete-dir-enabled".to_owned(), Value::Bool(false)),
        (
            "peer-limit-global".to_owned(),
            Value::Int(signed_len(config.peer_limit)),
        ),
        (
            "peer-limit-per-torrent".to_owned(),
            Value::Int(signed_len(config.torrent_peer_limit)),
        ),
        // The queue is always on: clove has no mode in which every torrent
        // runs regardless, which is what disabling it would mean.
        ("download-queue-enabled".to_owned(), Value::Bool(true)),
        (
            "download-queue-size".to_owned(),
            Value::Int(signed_len(config.max_active_downloads)),
        ),
        ("seed-queue-enabled".to_owned(), Value::Bool(true)),
        (
            "seed-queue-size".to_owned(),
            Value::Int(signed_len(config.max_active_seeds)),
        ),
        (
            "seedRatioLimited".to_owned(),
            Value::Bool(config.seed_ratio_milli > 0),
        ),
        (
            "seedRatioLimit".to_owned(),
            Value::Float(from_milli(config.seed_ratio_milli)),
        ),
        (
            "idle-seeding-limit-enabled".to_owned(),
            Value::Bool(config.seed_idle_minutes > 0),
        ),
        (
            "idle-seeding-limit".to_owned(),
            Value::Int(signed(config.seed_idle_minutes)),
        ),
        // True of every I2P torrent client, not placeholders. A peer is a
        // destination, so there is no port to forward, nothing to test, and
        // nothing an IP blocklist could name.
        ("peer-port".to_owned(), Value::Int(0)),
        ("peer-port-random-on-start".to_owned(), Value::Bool(false)),
        ("port-forwarding-enabled".to_owned(), Value::Bool(false)),
        ("dht-enabled".to_owned(), Value::Bool(false)),
        ("lpd-enabled".to_owned(), Value::Bool(false)),
        ("utp-enabled".to_owned(), Value::Bool(false)),
        ("blocklist-enabled".to_owned(), Value::Bool(false)),
        ("blocklist-size".to_owned(), Value::Int(0)),
        // i2p_pex, which clove speaks (SCOPE §3).
        ("pex-enabled".to_owned(), Value::Bool(true)),
        // Every I2P stream is encrypted end to end by the network itself;
        // there is no unencrypted mode to fall back to.
        ("encryption".to_owned(), Value::from("required")),
        // Rate limiting is deferred with a trigger (`PHASE-H.md` §11). These
        // report "no limit", which is the truth today.
        ("speed-limit-down-enabled".to_owned(), Value::Bool(false)),
        ("speed-limit-down".to_owned(), Value::Int(0)),
        ("speed-limit-up-enabled".to_owned(), Value::Bool(false)),
        ("speed-limit-up".to_owned(), Value::Int(0)),
        ("alt-speed-enabled".to_owned(), Value::Bool(false)),
        ("alt-speed-down".to_owned(), Value::Int(0)),
        ("alt-speed-up".to_owned(), Value::Int(0)),
        ("alt-speed-time-enabled".to_owned(), Value::Bool(false)),
        ("start-added-torrents".to_owned(), Value::Bool(true)),
        ("rename-partial-files".to_owned(), Value::Bool(false)),
        ("script-torrent-done-enabled".to_owned(), Value::Bool(false)),
        (
            "trash-original-torrent-files".to_owned(),
            Value::Bool(false),
        ),
        ("units".to_owned(), units()),
    ])
}

/// The unit table clients use to render sizes. Transmission's own values;
/// clients that omit this fall back to the same, but tremc reads it directly.
fn units() -> Value {
    let names = |list: [&str; 4]| Value::Array(list.iter().map(|s| Value::from(*s)).collect());
    Value::Object(vec![
        (
            "speed-units".to_owned(),
            names(["kB/s", "MB/s", "GB/s", "TB/s"]),
        ),
        ("speed-bytes".to_owned(), Value::Int(1000)),
        ("size-units".to_owned(), names(["kB", "MB", "GB", "TB"])),
        ("size-bytes".to_owned(), Value::Int(1000)),
        (
            "memory-units".to_owned(),
            names(["KiB", "MiB", "GiB", "TiB"]),
        ),
        ("memory-bytes".to_owned(), Value::Int(1024)),
    ])
}

/// `session-stats`: the totals `clove stats` reports, reshaped.
fn session_stats(daemon: &std::sync::Arc<crate::Daemon>) -> Value {
    let (count, totals, active, paused) = {
        let mut registry = crate::lock(&daemon.registry);
        let count = registry.count();
        let totals = registry.totals();
        let listing = registry.list();
        let (mut active, mut paused) = (0i64, 0i64);
        if let Value::Array(items) = &listing {
            for item in items {
                match get_str(item, "state") {
                    "downloading" | "seeding" => active += 1,
                    "paused" => paused += 1,
                    _ => {}
                }
            }
        }
        (count, totals, active, paused)
    };
    // Lifetime totals are per-torrent in clove and there is no session-wide
    // accumulator, so the "current" and "cumulative" stats blocks carry the
    // same figures. `clove-api(7)` says so; inventing a distinct lifetime
    // number would be a statistic nobody recorded.
    let stats = Value::Object(vec![
        ("uploadedBytes".to_owned(), Value::Int(0)),
        ("downloadedBytes".to_owned(), Value::Int(0)),
        ("filesAdded".to_owned(), Value::Int(signed_len(count))),
        ("sessionCount".to_owned(), Value::Int(1)),
        (
            "secondsActive".to_owned(),
            Value::Int(signed(daemon.start.elapsed().as_secs())),
        ),
    ]);
    Value::Object(vec![
        ("activeTorrentCount".to_owned(), Value::Int(active)),
        ("pausedTorrentCount".to_owned(), Value::Int(paused)),
        ("torrentCount".to_owned(), Value::Int(signed_len(count))),
        (
            "downloadSpeed".to_owned(),
            Value::Int(signed(totals.down_rate)),
        ),
        ("uploadSpeed".to_owned(), Value::Int(signed(totals.up_rate))),
        ("cumulative-stats".to_owned(), stats.clone()),
        ("current-stats".to_owned(), stats),
    ])
}

/// `free-space`: how much room the downloads directory has.
///
/// The path in the request is not consulted. clove has exactly one downloads
/// directory and Landlock confines it to that after start (SCOPE §5 Layer 2),
/// so an answer about any other path would be either a lie or a refusal. The
/// reply names the directory it measured, so a client asking about somewhere
/// else can see it did not get what it asked for.
fn free_space(daemon: &std::sync::Arc<crate::Daemon>) -> Value {
    let dir = daemon
        .rpc
        .as_ref()
        .map(|rpc| rpc.download_dir.clone())
        .unwrap_or_default();
    let free =
        rustix::fs::statvfs(dir.as_str()).map_or(0, |st| st.f_bavail.saturating_mul(st.f_frsize));
    Value::Object(vec![
        ("path".to_owned(), Value::from(dir)),
        ("size-bytes".to_owned(), Value::Int(signed(free))),
        ("total_size".to_owned(), Value::Int(signed(free))),
    ])
}

#[cfg(test)]
mod tests {
    //! The pure half: the mappings between clove's vocabulary and
    //! Transmission's. The surface as a whole — authentication, the CSRF
    //! handshake, dispatch over a real socket — is tested in `main.rs`, where
    //! `handle` can be driven end to end.

    use super::{
        Value, as_int, from_transmission_priority, ratio_to_milli, status, status_of,
        to_transmission_priority, torrent_object,
    };

    /// A detail object shaped like the one `/v1/torrents/{ih}` returns.
    fn view(state: &str, fields: Vec<(&str, Value)>) -> Value {
        let mut object = vec![
            (
                "info_hash".to_owned(),
                Value::from("3f2a91c0d4e5b6a7889900112233445566778899"),
            ),
            ("name".to_owned(), Value::from("demo")),
            ("state".to_owned(), Value::from(state)),
            ("size".to_owned(), Value::UInt(1000)),
            ("pieces".to_owned(), Value::UInt(10)),
            ("piece_length".to_owned(), Value::UInt(100)),
            ("progress".to_owned(), Value::Float(0.5)),
            ("added".to_owned(), Value::UInt(1_700_000_000_000)),
        ];
        for (key, value) in fields {
            object.retain(|(k, _)| k != key);
            object.push((key.to_owned(), value));
        }
        Value::Object(object)
    }

    fn render(view: &Value, fields: &[&str]) -> Value {
        torrent_object(view, 1, 0, "/downloads", fields)
    }

    #[test]
    fn only_the_requested_fields_come_back() {
        let object = render(&view("downloading", vec![]), &["id", "name"]);
        let fields = object.as_object().expect("an object");
        assert_eq!(fields.len(), 2, "{fields:?}");
        assert!(object.get("id").is_some());
        assert!(object.get("name").is_some());
        assert!(object.get("status").is_none());
    }

    #[test]
    fn a_field_we_do_not_answer_is_omitted_rather_than_faked() {
        // The two that matter: a client asking for a peer table gets no key at
        // all, which reads as "no data", rather than an array of invented rows.
        let object = render(
            &view("downloading", vec![]),
            &["peers", "pieces", "magnetLink", "id"],
        );
        for absent in ["peers", "pieces", "magnetLink"] {
            assert!(
                object.get(absent).is_none(),
                "{absent} should be omitted, not answered"
            );
        }
        assert!(object.get("id").is_some(), "the real field still came back");
    }

    #[test]
    fn every_clove_state_maps_to_a_status_a_client_understands() {
        // The mapping is the one place a client's whole display hangs off a
        // string comparison, so every state the registry can produce is named
        // here rather than left to the wildcard.
        assert_eq!(status_of("verifying", false), status::CHECK);
        assert_eq!(status_of("downloading", false), status::DOWNLOAD);
        assert_eq!(status_of("fetching-metadata", false), status::DOWNLOAD);
        assert_eq!(status_of("seeding", true), status::SEED);
        assert_eq!(status_of("paused", false), status::STOPPED);
        assert_eq!(status_of("paused", true), status::STOPPED);
        // The two "wanted but not running" states split on what it is waiting
        // to do, which is the only thing Transmission's codes distinguish.
        assert_eq!(status_of("queued", false), status::DOWNLOAD_WAIT);
        assert_eq!(status_of("queued", true), status::SEED_WAIT);
        assert_eq!(
            status_of("waiting-for-router", false),
            status::DOWNLOAD_WAIT
        );
        assert_eq!(status_of("complete", true), status::SEED_WAIT);
        // A state nobody taught this reads as stopped, never as running.
        assert_eq!(status_of("something-new", false), status::STOPPED);
    }

    #[test]
    fn a_magnet_reports_itself_as_metadata_incomplete() {
        // The signal a client uses to show "fetching metadata" rather than a
        // torrent stuck at 0%.
        let object = render(
            &view("fetching-metadata", vec![]),
            &["metadataPercentComplete", "status"],
        );
        assert_eq!(
            object
                .get("metadataPercentComplete")
                .and_then(Value::as_f64),
            Some(0.0)
        );
        assert_eq!(
            object.get("status").and_then(Value::as_u64),
            Some(status::DOWNLOAD as u64)
        );
    }

    #[test]
    fn size_and_progress_count_only_the_files_that_were_asked_for() {
        // A torrent with a skipped file is finished when the wanted files are,
        // exactly as `Hosted::wanted_and_held` decides it — so the byte figures
        // a client draws its bar from must agree with `progress`.
        let files = Value::Array(vec![
            Value::Object(vec![
                ("path".to_owned(), Value::from("a")),
                ("length".to_owned(), Value::UInt(600)),
                ("priority".to_owned(), Value::UInt(1)),
            ]),
            Value::Object(vec![
                ("path".to_owned(), Value::from("b")),
                ("length".to_owned(), Value::UInt(400)),
                ("priority".to_owned(), Value::UInt(0)),
            ]),
        ]);
        let object = render(
            &view("downloading", vec![("files", files)]),
            &[
                "totalSize",
                "sizeWhenDone",
                "leftUntilDone",
                "haveValid",
                "wanted",
                "fileCount",
            ],
        );
        assert_eq!(object.get("totalSize").and_then(Value::as_u64), Some(1000));
        // 600, not 1000: the skipped file is not part of what "done" means.
        assert_eq!(
            object.get("sizeWhenDone").and_then(Value::as_u64),
            Some(600)
        );
        assert_eq!(object.get("haveValid").and_then(Value::as_u64), Some(300));
        assert_eq!(
            object.get("leftUntilDone").and_then(Value::as_u64),
            Some(300)
        );
        assert_eq!(object.get("fileCount").and_then(Value::as_u64), Some(2));
        let wanted = object
            .get("wanted")
            .and_then(Value::as_array)
            .expect("wanted");
        assert_eq!(
            wanted.iter().filter_map(Value::as_bool).collect::<Vec<_>>(),
            vec![true, false]
        );
    }

    #[test]
    fn an_eta_is_only_reported_when_one_can_be_computed() {
        let downloading = |down: u64| {
            render(
                &view("downloading", vec![("down_rate", Value::UInt(down))]),
                &["eta"],
            )
            .get("eta")
            .and_then(Value::as_f64)
            .expect("eta")
        };
        // 500 bytes left at 100 B/s.
        assert!((downloading(100) - 5.0).abs() < f64::EPSILON);
        // Transmission's "unknown", not a division by zero and not a zero,
        // which would render as "done in no time".
        assert!((downloading(0) - -1.0).abs() < f64::EPSILON);

        let seeding = render(&view("seeding", vec![]), &["eta"])
            .get("eta")
            .and_then(Value::as_f64)
            .expect("eta");
        assert!((seeding - -1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_torrent_that_downloaded_nothing_has_no_ratio() {
        // -1 is Transmission's "not a number"; 0 would claim it had uploaded
        // nothing, which is a different and possibly false statement.
        let object = render(&view("seeding", vec![]), &["uploadRatio"]);
        assert!(
            (object
                .get("uploadRatio")
                .and_then(Value::as_f64)
                .expect("ratio")
                - -1.0)
                .abs()
                < f64::EPSILON
        );

        let object = render(
            &view(
                "seeding",
                vec![
                    ("downloaded", Value::UInt(1000)),
                    ("ratio", Value::UInt(1500)),
                ],
            ),
            &["uploadRatio"],
        );
        let ratio = object
            .get("uploadRatio")
            .and_then(Value::as_f64)
            .expect("ratio");
        assert!((ratio - 1.5).abs() < 1e-9, "{ratio}");
    }

    #[test]
    fn a_stop_reason_reaches_the_only_field_a_client_shows_it_in() {
        // A torrent the daemon stopped for a rule must say so somewhere the
        // operator will look, or it is a bug report about a torrent that
        // "stopped working".
        let object = render(
            &view(
                "paused",
                vec![("paused_because", Value::from("seed ratio 2.0 reached"))],
            ),
            &["error", "errorString"],
        );
        assert_eq!(object.get("error").and_then(Value::as_u64), Some(3));
        assert_eq!(
            object.get("errorString").and_then(Value::as_str),
            Some("seed ratio 2.0 reached")
        );

        // An operator's own pause is not an error.
        let object = render(&view("paused", vec![]), &["error", "errorString"]);
        assert_eq!(object.get("error").and_then(Value::as_u64), Some(0));
        assert_eq!(object.get("errorString").and_then(Value::as_str), Some(""));
    }

    #[test]
    fn priorities_round_trip_through_transmission_spelling() {
        // clove: 0 skip, 1 normal, 2 high. Transmission: -1 low, 0 normal,
        // 1 high, plus a separate wanted flag.
        assert_eq!(to_transmission_priority(1), 0);
        assert_eq!(to_transmission_priority(2), 1);
        // A skipped file has a *priority* of normal and is marked unwanted;
        // reporting it as low would be a different claim.
        assert_eq!(to_transmission_priority(0), 0);

        assert_eq!(from_transmission_priority(1), 2);
        assert_eq!(from_transmission_priority(0), 1);
        // clove has no "low", so low lands on normal rather than on skip —
        // quietly turning "download this last" into "do not download this"
        // would lose the operator a file.
        assert_eq!(from_transmission_priority(-1), 1);
    }

    #[test]
    fn a_ratio_from_a_client_is_clamped_rather_than_wrapped() {
        assert_eq!(ratio_to_milli(1.5), 1500);
        assert_eq!(ratio_to_milli(0.0), 0);
        // Nothing a slider can produce should set a ratio the config file
        // would have refused, or wrap into a small one.
        assert_eq!(ratio_to_milli(-1.0), 0);
        assert_eq!(ratio_to_milli(f64::NAN), 0);
        assert_eq!(ratio_to_milli(f64::INFINITY), 0);
        assert_eq!(
            ratio_to_milli(1e30),
            clove_core::config::MAX_SEED_RATIO_MILLI
        );
    }

    #[test]
    fn a_tracker_row_carries_what_a_client_needs_to_render_it() {
        // Found by running transmission-remote: with only `announce`, `tier`
        // and the counters, its tracker section printed *nothing at all* — the
        // timing and scrape fields are what it decides a row exists from. The
        // values are honest (clove keeps no announce timestamps and never
        // scrapes); their presence is the point.
        let trackers = Value::Array(vec![Value::from("http://tracker.i2p:6969/a?x=1")]);
        let object = render(
            &view("downloading", vec![("trackers", trackers)]),
            &["trackerStats"],
        );
        let rows = object
            .get("trackerStats")
            .and_then(Value::as_array)
            .expect("trackerStats");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        for key in [
            "announce",
            "host",
            "sitename",
            "tier",
            "announceState",
            "nextAnnounceTime",
            "lastAnnounceTime",
            "scrapeState",
            "seederCount",
        ] {
            assert!(row.get(key).is_some(), "{key} missing from a tracker row");
        }
        assert_eq!(
            row.get("host").and_then(Value::as_str),
            Some("http://tracker.i2p:6969")
        );
        assert_eq!(
            row.get("sitename").and_then(Value::as_str),
            Some("tracker.i2p")
        );
        // Never scraped, and saying so rather than reporting a failed scrape.
        assert_eq!(row.get("hasScraped").and_then(Value::as_bool), Some(false));
        assert_eq!(row.get("seederCount").and_then(Value::as_f64), Some(-1.0));
    }

    #[test]
    fn an_announce_url_is_split_without_a_url_type() {
        use super::{origin_of, sitename_of};
        assert_eq!(origin_of("http://tracker.i2p/a"), "http://tracker.i2p");
        assert_eq!(
            origin_of("http://tracker.i2p:6969/announce?x=1"),
            "http://tracker.i2p:6969"
        );
        assert_eq!(sitename_of("http://tracker.i2p:6969/a"), "tracker.i2p");
        assert_eq!(sitename_of("http://tracker.i2p"), "tracker.i2p");
        // Anything that is not a URL comes back whole rather than half-parsed
        // into something that reads like a host.
        assert_eq!(origin_of("not a url"), "not a url");
        assert_eq!(sitename_of(""), "");
    }

    #[test]
    fn the_rpc_path_is_matched_with_or_without_its_trailing_slash() {
        // The regression for the one defect a real client found and no unit
        // test would have: transmission-remote 4.0.5 posts to
        // `/transmission/rpc/`, and an exact match answered it with `/v1/`'s
        // token error.
        assert!(super::is_rpc_path("/transmission/rpc"));
        assert!(super::is_rpc_path("/transmission/rpc/"));
        // And nothing else, in particular nothing that merely starts with it —
        // this decides which authentication scheme runs.
        for other in [
            "/transmission/rpc//",
            "/transmission/rpc/x",
            "/transmission/rpcx",
            "/transmission",
            "/transmission/",
            "/v1/status",
            "/",
            "",
        ] {
            assert!(!super::is_rpc_path(other), "{other} should not route here");
        }
    }

    #[test]
    fn integers_are_read_in_whichever_spelling_a_client_sent() {
        assert_eq!(as_int(&Value::UInt(3)), Some(3));
        assert_eq!(as_int(&Value::Int(-3)), Some(-3));
        assert_eq!(as_int(&Value::Float(3.0)), Some(3));
        // Not an id and not a tag: refused rather than truncated to 3, which
        // would silently act on a different torrent.
        assert_eq!(as_int(&Value::Float(3.5)), None);
        assert_eq!(as_int(&Value::from("3")), None);
        assert_eq!(as_int(&Value::Null), None);
    }
}
