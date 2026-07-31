//! `clove.conf` parsing — doas/sshd discipline (SCOPE §3).
//!
//! One flat format: `key value` lines, `#` comments, no nesting. Unknown
//! and duplicate keys are fatal: a typo'd option must fail startup loudly,
//! never be silently ignored. An empty (or absent) file is the safe,
//! working default — every key exists to *deviate* from a sane default.
//!
//! This module deliberately stores the SAM address as a string: the engine
//! has no `std::net` vocabulary (Layer 1), so the loopback check here is
//! textual and `i2pnet` performs the real parse at connect time.

use std::fmt;
use std::path::PathBuf;

/// The SAM address assumed when the config doesn't say otherwise — the
/// standard bridge address of a default-configured local router.
pub const DEFAULT_SAM_ADDRESS: &str = "127.0.0.1:7656";

/// Environment-derived default locations, separated from parsing so tests
/// don't depend on the environment.
#[derive(Clone, Debug)]
pub struct Defaults {
    /// XDG data home (`$XDG_DATA_HOME` or `~/.local/share`); the default
    /// data directory is `<data_home>/clove`.
    pub data_home: PathBuf,
    /// `$XDG_RUNTIME_DIR` when set; preferred home of the control socket.
    pub runtime_dir: Option<PathBuf>,
    /// XDG config home (`$XDG_CONFIG_HOME` or `~/.config`); the default
    /// config file is `<config_home>/clove/clove.conf`.
    pub config_home: PathBuf,
}

impl Defaults {
    /// Resolve from `XDG_DATA_HOME`, `HOME`, and `XDG_RUNTIME_DIR`.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`] when neither `XDG_DATA_HOME` nor `HOME` is set —
    /// there is nowhere sane to keep state, and guessing would be worse.
    pub fn from_env() -> Result<Self, Error> {
        let data_home = match nonempty_env("XDG_DATA_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => match nonempty_env("HOME") {
                Some(home) => PathBuf::from(home).join(".local/share"),
                None => return Err(Error::NoHome),
            },
        };
        let runtime_dir = nonempty_env("XDG_RUNTIME_DIR").map(PathBuf::from);
        let config_home = match nonempty_env("XDG_CONFIG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => match nonempty_env("HOME") {
                Some(home) => PathBuf::from(home).join(".config"),
                None => return Err(Error::NoHome),
            },
        };
        Ok(Defaults {
            data_home,
            runtime_dir,
            config_home,
        })
    }

    /// Where `cloved` looks for its config file when `-c` is not given.
    /// Absent is not an error: an empty config is the working default.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.config_home.join("clove/clove.conf")
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Validated daemon configuration.
#[allow(
    clippy::struct_excessive_bools,
    reason = "this struct is the shape of clove.conf(5), which is a flat list of \
              independent `key value` lines. The lint's advice — a state machine, or \
              two-variant enums — is right where booleans encode one answer between \
              them, as it was for the registry's Wanted. These do not: every flag here \
              is a separate operator decision with no combination that means nothing, \
              and grouping them would put the config file and its parsed form into \
              different shapes for no gain."
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// SAM bridge address: `host:port` (loopback unless overridden) or an
    /// absolute unix-socket path.
    pub sam_address: String,
    /// The explicit, ugly, dangerous override for a non-loopback SAM
    /// address (`i_know_sam_is_remote yes`).
    pub i_know_sam_is_remote: bool,
    /// Client state directory (destination keys, resume data, torrents).
    pub data_dir: PathBuf,
    /// Control socket path for the local HTTP API.
    pub api_socket: PathBuf,
    /// Transient identity: never persist destination keys (Q4).
    pub ephemeral: bool,
    /// Grow each file to its full length when a torrent is added, rather than
    /// letting it become sparse as pieces land (SCOPE §4).
    pub preallocate: bool,
    /// Ceiling on peer connections across *every* torrent at once
    /// (`docs/PHASE-H.md` §3).
    pub peer_limit: usize,
    /// Ceiling on peer connections for any one torrent, applied under
    /// [`peer_limit`](Config::peer_limit).
    pub torrent_peer_limit: usize,
    /// How many incomplete torrents may run at once; the rest wait in the
    /// queue (`docs/PHASE-H.md` §4).
    pub max_active_downloads: usize,
    /// How many complete torrents may seed at once.
    pub max_active_seeds: usize,
    /// Stop seeding at this uploaded/downloaded ratio, in **thousandths**;
    /// `0` seeds without limit. Per-torrent overrides sit under it.
    pub seed_ratio_milli: u64,
    /// Stop seeding after this many minutes with no peer attached; `0` never
    /// stops.
    pub seed_idle_minutes: u64,
    /// A directory to watch for `.torrent` files to add, or `None` to watch
    /// none — the default.
    pub watch_dir: Option<PathBuf>,
    /// Serve the Transmission-compatible RPC surface at `/transmission/rpc`
    /// alongside `/v1/` (`docs/PHASE-I.md`). Off by default: the sane default
    /// is that only `clove` talks to `cloved`, and a second authentication
    /// scheme should not exist on a daemon nobody asked to expose one.
    pub transmission_rpc: bool,
}

/// Client-wide peer ceiling when the config does not say otherwise.
///
/// The concurrency `SCOPE.md` R2 actually measured — 200 concurrent streams on
/// one i2pd SAM session, uncorrelated with connect latency across 30 runs
/// (`PROTOCOL.i2p-bt` §2.6e) — rather than a round number. Above it we would be
/// guessing, and the thing being guessed with is the session every torrent
/// shares.
pub const DEFAULT_PEER_LIMIT: usize = 200;

/// Per-torrent peer ceiling when the config does not say otherwise. The
/// long-standing `SwarmConfig::max_peers` default, now a sub-cap rather than
/// the only cap.
pub const DEFAULT_TORRENT_PEER_LIMIT: usize = 50;

/// Concurrently downloading torrents when the config does not say otherwise.
///
/// Three, because on I2P the scarce thing is tunnels rather than bandwidth:
/// three torrents each holding a healthy peer set finish sooner, one after
/// another, than twenty sharing the same ceiling and all crawling.
pub const DEFAULT_MAX_ACTIVE_DOWNLOADS: usize = 3;

/// Concurrently seeding torrents when the config does not say otherwise.
///
/// Higher than the download limit: a seed answers requests rather than chasing
/// pieces, so it costs peers but little else, and being a good swarm citizen
/// is most of what an I2P client is for.
pub const DEFAULT_MAX_ACTIVE_SEEDS: usize = 5;

/// Largest accepted seed ratio, in thousandths (1000 = 1.0).
///
/// A thousand-to-one is already far past any tracker's requirement; past this
/// the value is a typo, and the same reasoning applies as to
/// [`MAX_PEER_LIMIT`].
pub const MAX_SEED_RATIO_MILLI: u64 = 1_000_000;

/// Longest accepted seed idle limit, in minutes — a little over two months.
pub const MAX_SEED_IDLE_MINUTES: u64 = 100_000;

/// Why configuration was rejected. Messages are written for the operator:
/// they name the line, the key, and what to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Neither `XDG_DATA_HOME` nor `HOME` is set.
    NoHome,
    /// A problem on a specific line of the config file.
    Line {
        /// 1-based line number.
        line: usize,
        /// What is wrong with it.
        problem: Problem,
    },
}

/// Per-line configuration problems.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Key is not one clove knows; typos must fail startup loudly.
    UnknownKey(String),
    /// Key given without a value.
    MissingValue,
    /// Key set twice.
    DuplicateKey,
    /// Boolean value other than `yes` or `no`.
    BadBool,
    /// A count that is not a positive number, or is absurdly large.
    BadCount,
    /// A ratio that is not a non-negative decimal, or is absurdly large.
    BadRatio,
    /// SAM address is neither `host:port` nor an absolute socket path.
    BadSamAddress,
    /// SAM address is not loopback and `i_know_sam_is_remote` is not set.
    RemoteSam,
    /// A path value that is not absolute.
    RelativePath,
    /// A `sam_address` this build cannot actually dial.
    ///
    /// Separate from [`BadSamAddress`](Problem::BadSamAddress), which is about
    /// shape: these are well-formed addresses the runtime has no way to honour,
    /// and accepting them meant `cloved -C` approving a configuration that then
    /// did something else — a unix path left the daemon running with no router
    /// at all, and a remote host was quietly redirected to the same port on
    /// `127.0.0.1`.
    UnsupportedSamTransport(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoHome => {
                write!(
                    f,
                    "config: neither XDG_DATA_HOME nor HOME is set; set one or use data_dir"
                )
            }
            Error::Line { line, problem } => write!(f, "config: line {line}: {problem}"),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::UnknownKey(key) => write!(f, "unknown key \"{key}\""),
            Problem::MissingValue => write!(f, "key has no value"),
            Problem::DuplicateKey => write!(f, "key already set earlier in the file"),
            Problem::BadBool => write!(f, "expected \"yes\" or \"no\""),
            Problem::BadCount => write!(f, "expected a number from 1 to {MAX_PEER_LIMIT}"),
            Problem::BadRatio => write!(
                f,
                "expected a ratio like 2 or 1.75, from 0 (unlimited) to {}",
                MAX_SEED_RATIO_MILLI / 1000
            ),
            Problem::BadSamAddress => {
                write!(
                    f,
                    "sam_address must be host:port or an absolute socket path"
                )
            }
            Problem::UnsupportedSamTransport(what) => write!(
                f,
                "sam_address: {what} (this build dials 127.0.0.1 only; see clove.conf(5))"
            ),
            Problem::RemoteSam => write!(
                f,
                "sam_address is not loopback; if you really run SAM remotely, set i_know_sam_is_remote yes (dangerous: your traffic to the router is unprotected)"
            ),
            Problem::RelativePath => write!(f, "path must be absolute"),
        }
    }
}

impl std::error::Error for Error {}

/// The keys as read so far, each with the line it came from, before defaults
/// are applied.
///
/// A struct rather than a dozen locals in [`Config::parse`], so the per-key
/// dispatch can live in its own function: the loop that reads lines and the
/// table that interprets keys are separate jobs, and keeping them in one body
/// is what pushed it past the length clippy will accept.
#[derive(Default)]
struct Draft {
    sam_address: Option<(usize, String)>,
    i_know_sam_is_remote: Option<(usize, bool)>,
    data_dir: Option<(usize, PathBuf)>,
    api_socket: Option<(usize, PathBuf)>,
    ephemeral: Option<(usize, bool)>,
    preallocate: Option<(usize, bool)>,
    peer_limit: Option<(usize, usize)>,
    torrent_peer_limit: Option<(usize, usize)>,
    max_active_downloads: Option<(usize, usize)>,
    max_active_seeds: Option<(usize, usize)>,
    seed_ratio_milli: Option<(usize, u64)>,
    seed_idle_minutes: Option<(usize, u64)>,
    watch_dir: Option<(usize, PathBuf)>,
    transmission_rpc: Option<(usize, bool)>,
}

impl Draft {
    /// Record one `key value` line, or say what is wrong with it.
    fn accept(&mut self, line: usize, key: &str, value: &str) -> Result<(), Error> {
        let at = |problem| Error::Line { line, problem };
        match key {
            "sam_address" => {
                if !looks_like_sam_address(value) {
                    return Err(at(Problem::BadSamAddress));
                }
                set(&mut self.sam_address, line, value.to_owned())?;
            }
            "i_know_sam_is_remote" => {
                let flag = parse_bool(value).ok_or_else(|| at(Problem::BadBool))?;
                set(&mut self.i_know_sam_is_remote, line, flag)?;
            }
            "data_dir" => set(&mut self.data_dir, line, absolute(value, line)?)?,
            "api_socket" => set(&mut self.api_socket, line, absolute(value, line)?)?,
            "ephemeral" => {
                let flag = parse_bool(value).ok_or_else(|| at(Problem::BadBool))?;
                set(&mut self.ephemeral, line, flag)?;
            }
            "preallocate" => {
                let flag = parse_bool(value).ok_or_else(|| at(Problem::BadBool))?;
                set(&mut self.preallocate, line, flag)?;
            }
            "peer_limit" => {
                let count = parse_count(value).ok_or_else(|| at(Problem::BadCount))?;
                set(&mut self.peer_limit, line, count)?;
            }
            "torrent_peer_limit" => {
                let count = parse_count(value).ok_or_else(|| at(Problem::BadCount))?;
                set(&mut self.torrent_peer_limit, line, count)?;
            }
            "max_active_downloads" => {
                let count = parse_count(value).ok_or_else(|| at(Problem::BadCount))?;
                set(&mut self.max_active_downloads, line, count)?;
            }
            "max_active_seeds" => {
                let count = parse_count(value).ok_or_else(|| at(Problem::BadCount))?;
                set(&mut self.max_active_seeds, line, count)?;
            }
            "seed_ratio" => {
                let milli = parse_seed_ratio(value).ok_or_else(|| at(Problem::BadRatio))?;
                set(&mut self.seed_ratio_milli, line, milli)?;
            }
            "watch_dir" => set(&mut self.watch_dir, line, absolute(value, line)?)?,
            "transmission_rpc" => {
                let flag = parse_bool(value).ok_or_else(|| at(Problem::BadBool))?;
                set(&mut self.transmission_rpc, line, flag)?;
            }
            "seed_idle_minutes" => {
                let minutes = value
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n <= MAX_SEED_IDLE_MINUTES)
                    .ok_or_else(|| at(Problem::BadCount))?;
                set(&mut self.seed_idle_minutes, line, minutes)?;
            }
            other => return Err(at(Problem::UnknownKey(other.to_owned()))),
        }
        Ok(())
    }
}

impl Config {
    /// Parse configuration text against environment-derived defaults.
    /// Empty text yields the fully working default configuration.
    ///
    /// # Errors
    ///
    /// Any [`Problem`] with its line number: unknown or duplicate keys,
    /// missing values, bad booleans, relative paths, or a non-loopback SAM
    /// address without the explicit override.
    pub fn parse(text: &str, defaults: &Defaults) -> Result<Self, Error> {
        let mut draft = Draft::default();

        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (key, value) = match trimmed.split_once(char::is_whitespace) {
                Some((k, v)) if !v.trim().is_empty() => (k, v.trim()),
                _ => {
                    return Err(Error::Line {
                        line,
                        problem: Problem::MissingValue,
                    });
                }
            };
            draft.accept(line, key, value)?;
        }

        let Draft {
            sam_address,
            i_know_sam_is_remote,
            data_dir,
            api_socket,
            ephemeral,
            preallocate,
            peer_limit,
            torrent_peer_limit,
            max_active_downloads,
            max_active_seeds,
            seed_ratio_milli,
            seed_idle_minutes,
            watch_dir,
            transmission_rpc,
        } = draft;

        let i_know_sam_is_remote = i_know_sam_is_remote.is_some_and(|(_, v)| v);
        let (sam_line, sam_address) = sam_address.unwrap_or((0, DEFAULT_SAM_ADDRESS.to_owned()));
        if !i_know_sam_is_remote && !is_loopback_sam(&sam_address) {
            return Err(Error::Line {
                line: sam_line,
                problem: Problem::RemoteSam,
            });
        }
        // Refuse here what the runtime cannot do, rather than accepting it and
        // doing something else. `cloved -C` exists to tell an operator their
        // configuration is good; it should not say so about an address that
        // leaves the daemon with no router, or one it will silently swap for a
        // different host. `docs/PROTOCOL.i2p-bt` §2.1 records why the backend
        // is loopback-only.
        if let Some(what) = unsupported_sam_transport(&sam_address) {
            return Err(Error::Line {
                line: sam_line,
                problem: Problem::UnsupportedSamTransport(what),
            });
        }

        let data_dir = data_dir.map_or_else(|| defaults.data_home.join("clove"), |(_, v)| v);
        let api_socket = api_socket.map_or_else(
            || {
                defaults
                    .runtime_dir
                    .as_ref()
                    .map_or_else(|| data_dir.join("clove.sock"), |dir| dir.join("clove.sock"))
            },
            |(_, v)| v,
        );

        Ok(Config {
            sam_address,
            i_know_sam_is_remote,
            data_dir,
            api_socket,
            ephemeral: ephemeral.is_some_and(|(_, v)| v),
            preallocate: preallocate.is_some_and(|(_, v)| v),
            peer_limit: peer_limit.map_or(DEFAULT_PEER_LIMIT, |(_, v)| v),
            torrent_peer_limit: torrent_peer_limit.map_or(DEFAULT_TORRENT_PEER_LIMIT, |(_, v)| v),
            max_active_downloads: max_active_downloads
                .map_or(DEFAULT_MAX_ACTIVE_DOWNLOADS, |(_, v)| v),
            max_active_seeds: max_active_seeds.map_or(DEFAULT_MAX_ACTIVE_SEEDS, |(_, v)| v),
            seed_ratio_milli: seed_ratio_milli.map_or(0, |(_, v)| v),
            seed_idle_minutes: seed_idle_minutes.map_or(0, |(_, v)| v),
            watch_dir: watch_dir.map(|(_, v)| v),
            transmission_rpc: transmission_rpc.is_some_and(|(_, v)| v),
        })
    }
}

/// Largest accepted value for either peer ceiling.
///
/// Not a considered engineering limit so much as a refusal to accept a typo:
/// the numbers that make sense here are in the hundreds, and a `peer_limit` of
/// 100000 is a slipped keystroke that would be discovered as a wedged SAM
/// session hours later rather than as a failed start.
pub const MAX_PEER_LIMIT: usize = 10_000;

/// Parse a peer-count value: a plain decimal, at least 1, at most
/// [`MAX_PEER_LIMIT`].
///
/// Zero is refused rather than read as "unlimited". A client configured to
/// hold no peers at all is never what was meant, and the spelling for
/// "as many as you like" is a large number, not an ambiguous one.
/// Parse a ratio written the way a human writes one — `2`, `1.5`, `0.25` —
/// into thousandths.
///
/// Hand-rolled rather than by `f64`: the config format is exact `key value`
/// text, and a ratio that round-trips through a float can persist as
/// `1499` where the operator wrote `1.5`. At most three decimal places, which
/// is more precision than a seeding ratio has ever needed.
///
/// Public because the API takes ratios too, and one grammar for both is the
/// point: `seed_ratio 1.5` in a config and `1.5` over the socket must mean the
/// same number.
///
/// Returns `None` for anything that is not a ratio, or is past
/// [`MAX_SEED_RATIO_MILLI`].
#[must_use]
pub fn parse_seed_ratio(value: &str) -> Option<u64> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac.len() > 3 {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    // Right-pad to exactly three digits: "5" is 500 thousandths, not 5.
    let mut milli = 0u64;
    let mut digits = frac.bytes();
    for _ in 0..3 {
        let d = digits.next().map_or(0, |b| u64::from(b - b'0'));
        milli = milli * 10 + d;
    }
    let total = whole.checked_mul(1000)?.checked_add(milli)?;
    (total <= MAX_SEED_RATIO_MILLI).then_some(total)
}

fn parse_count(value: &str) -> Option<usize> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=MAX_PEER_LIMIT).contains(n))
}

fn set<T>(slot: &mut Option<(usize, T)>, line: usize, value: T) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::Line {
            line,
            problem: Problem::DuplicateKey,
        });
    }
    *slot = Some((line, value));
    Ok(())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn absolute(value: &str, line: usize) -> Result<PathBuf, Error> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(Error::Line {
            line,
            problem: Problem::RelativePath,
        })
    }
}

/// Syntactic shape check: absolute path, or `host:port` with a numeric
/// port in range. Says nothing about loopback-ness.
fn looks_like_sam_address(value: &str) -> bool {
    if value.starts_with('/') {
        return true;
    }
    match value.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => port
            .parse::<u16>()
            .is_ok_and(|p| p != 0 && !port.starts_with('+')),
        _ => false,
    }
}

/// Why this build cannot dial `value`, or `None` if it can.
///
/// The SAM backend builds its own loopback TCP connection and takes the port
/// from here and nothing else (`crates/i2pnet/src/sam.rs`), so these two shapes
/// parse and validate and then are not what happens. Both were accepted:
///
/// - a unix-socket path became `"unsupported-sam-address"` at startup and the
///   daemon ran with no router, having been told its configuration was fine;
/// - a remote `host:port` had its host discarded and the port dialed on
///   `127.0.0.1`, which is a different router than the one asked for — and the
///   one case where quietly doing something else is worse than failing, because
///   the operator set a deliberately ugly flag to say they meant it.
fn unsupported_sam_transport(value: &str) -> Option<&'static str> {
    if value.starts_with('/') {
        return Some("a unix-socket SAM address is not supported; use host:port");
    }
    if !is_loopback_sam(value) {
        return Some("a remote SAM bridge is not supported");
    }
    None
}

/// Textual loopback check. Deliberately exact-match — `localhost` is the
/// single permitted hostname anywhere in clove (SCOPE §5: no DNS), and
/// exotic loopback spellings (`127.0.0.2`) don't get the benefit of the
/// doubt.
fn is_loopback_sam(value: &str) -> bool {
    if value.starts_with('/') {
        return true; // unix socket: local by nature
    }
    match value.rsplit_once(':') {
        Some((host, _)) => matches!(host, "127.0.0.1" | "localhost" | "[::1]"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Defaults {
        Defaults {
            data_home: PathBuf::from("/home/u/.local/share"),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            config_home: PathBuf::from("/home/u/.config"),
        }
    }

    #[test]
    fn empty_config_is_the_working_default() {
        let c = Config::parse("", &defaults()).unwrap();
        assert_eq!(c.sam_address, DEFAULT_SAM_ADDRESS);
        assert!(!c.i_know_sam_is_remote);
        assert!(!c.ephemeral);
        // Sparse by default: preallocation costs the full size at add time,
        // which is a deviation an operator opts into, not basic function.
        assert!(!c.preallocate);
        assert_eq!(c.peer_limit, DEFAULT_PEER_LIMIT);
        assert_eq!(c.torrent_peer_limit, DEFAULT_TORRENT_PEER_LIMIT);
        assert_eq!(c.max_active_downloads, DEFAULT_MAX_ACTIVE_DOWNLOADS);
        assert_eq!(c.max_active_seeds, DEFAULT_MAX_ACTIVE_SEEDS);
        // Seeding without limit is the default: stopping is a deviation an
        // operator opts into.
        assert_eq!(c.seed_ratio_milli, 0);
        assert_eq!(c.seed_idle_minutes, 0);
        assert_eq!(c.watch_dir, None);
        // Only `clove` talks to `cloved` unless asked otherwise: a second
        // authentication scheme is a deviation, not basic function.
        assert!(!c.transmission_rpc);
        assert_eq!(c.data_dir, PathBuf::from("/home/u/.local/share/clove"));
        assert_eq!(c.api_socket, PathBuf::from("/run/user/1000/clove.sock"));
    }

    #[test]
    fn default_config_path_is_xdg() {
        assert_eq!(
            defaults().config_path(),
            PathBuf::from("/home/u/.config/clove/clove.conf")
        );
    }

    #[test]
    fn api_socket_falls_back_to_data_dir() {
        let d = Defaults {
            runtime_dir: None,
            ..defaults()
        };
        let c = Config::parse("", &d).unwrap();
        assert_eq!(
            c.api_socket,
            PathBuf::from("/home/u/.local/share/clove/clove.sock")
        );
    }

    #[test]
    fn parses_a_full_config() {
        let text = "\
# clove.conf
sam_address\t127.0.0.1:7657

data_dir /srv/clove
api_socket /run/clove.sock
ephemeral yes
preallocate yes
";
        let c = Config::parse(text, &defaults()).unwrap();
        assert_eq!(c.sam_address, "127.0.0.1:7657");
        assert_eq!(c.data_dir, PathBuf::from("/srv/clove"));
        assert_eq!(c.api_socket, PathBuf::from("/run/clove.sock"));
        assert!(c.ephemeral);
        assert!(c.preallocate);
    }

    #[test]
    fn peer_ceilings_are_counts_with_a_refused_zero() {
        let d = defaults();
        let c = Config::parse("peer_limit 120\ntorrent_peer_limit 30\n", &d).unwrap();
        assert_eq!(c.peer_limit, 120);
        assert_eq!(c.torrent_peer_limit, 30);
        assert_eq!(Config::parse("peer_limit 1\n", &d).unwrap().peer_limit, 1);
        assert_eq!(
            Config::parse(&format!("peer_limit {MAX_PEER_LIMIT}\n"), &d)
                .unwrap()
                .peer_limit,
            MAX_PEER_LIMIT
        );

        // Zero is not "unlimited" — a client that may hold no peers is never
        // what was meant — and a slipped keystroke fails the start rather
        // than being discovered later as a wedged session.
        for bad in [
            "peer_limit 0",
            "peer_limit -1",
            "peer_limit 1.5",
            "peer_limit lots",
            "peer_limit 10001",
            "torrent_peer_limit 0",
            "torrent_peer_limit 99999999999999999999",
        ] {
            match Config::parse(bad, &d) {
                Err(Error::Line { problem, .. }) => {
                    assert_eq!(problem, Problem::BadCount, "{bad}");
                }
                other => panic!("{bad}: expected BadCount, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_ratio_is_read_exactly_as_written() {
        let d = defaults();
        let ratio = |text: &str| Config::parse(text, &d).map(|c| c.seed_ratio_milli);
        assert_eq!(ratio("seed_ratio 2\n").unwrap(), 2000);
        assert_eq!(ratio("seed_ratio 1.5\n").unwrap(), 1500);
        assert_eq!(ratio("seed_ratio 0.25\n").unwrap(), 250);
        assert_eq!(ratio("seed_ratio 0.001\n").unwrap(), 1);
        // A bare fraction, and a trailing dot, both mean what they look like.
        assert_eq!(ratio("seed_ratio .5\n").unwrap(), 500);
        assert_eq!(ratio("seed_ratio 3.\n").unwrap(), 3000);
        // Zero is "seed without limit", the default, and is spelled the same
        // way the config omits the key.
        assert_eq!(ratio("seed_ratio 0\n").unwrap(), 0);
        // The fraction is padded, not parsed as an integer: `1.5` is 1500
        // thousandths and not 1005.
        assert_ne!(ratio("seed_ratio 1.5\n").unwrap(), 1005);

        for bad in [
            "seed_ratio -1",
            "seed_ratio 1.2345",
            "seed_ratio two",
            "seed_ratio 1.5.2",
            "seed_ratio .",
            "seed_ratio 1e3",
            "seed_ratio 1000001",
        ] {
            match Config::parse(bad, &d) {
                Err(Error::Line { problem, .. }) => {
                    assert_eq!(problem, Problem::BadRatio, "{bad}");
                }
                other => panic!("{bad}: expected BadRatio, got {other:?}"),
            }
        }

        let idle = |text: &str| Config::parse(text, &d).map(|c| c.seed_idle_minutes);
        assert_eq!(idle("seed_idle_minutes 30\n").unwrap(), 30);
        // Zero means never, so unlike a peer limit it is accepted.
        assert_eq!(idle("seed_idle_minutes 0\n").unwrap(), 0);
        assert!(idle("seed_idle_minutes -5\n").is_err());
        assert!(idle("seed_idle_minutes soon\n").is_err());
    }

    #[test]
    fn transmission_rpc_is_a_bool_like_every_other_flag() {
        let d = defaults();
        assert!(
            Config::parse("transmission_rpc yes\n", &d)
                .unwrap()
                .transmission_rpc
        );
        assert!(
            !Config::parse("transmission_rpc no\n", &d)
                .unwrap()
                .transmission_rpc
        );
        assert!(Config::parse("transmission_rpc maybe\n", &d).is_err());
        assert!(Config::parse("transmission_rpc\n", &d).is_err());
    }

    #[test]
    fn preallocate_is_a_bool_like_every_other_flag() {
        let d = defaults();
        assert!(Config::parse("preallocate yes\n", &d).unwrap().preallocate);
        assert!(!Config::parse("preallocate no\n", &d).unwrap().preallocate);
    }

    #[test]
    fn unknown_key_is_fatal_with_line_number() {
        let err = Config::parse("\n\nsam_adress 127.0.0.1:7656\n", &defaults()).unwrap_err();
        assert_eq!(
            err,
            Error::Line {
                line: 3,
                problem: Problem::UnknownKey("sam_adress".into())
            }
        );
    }

    #[test]
    fn rejects_line_problems() {
        let d = defaults();
        let cases: Vec<(&str, Problem)> = vec![
            ("ephemeral", Problem::MissingValue),
            ("ephemeral \t", Problem::MissingValue),
            ("ephemeral maybe", Problem::BadBool),
            ("data_dir relative/path", Problem::RelativePath),
            ("sam_address 127.0.0.1", Problem::BadSamAddress),
            ("sam_address 127.0.0.1:0", Problem::BadSamAddress),
            ("sam_address 127.0.0.1:99999", Problem::BadSamAddress),
            ("sam_address :7656", Problem::BadSamAddress),
            ("ephemeral yes\nephemeral yes", Problem::DuplicateKey),
            ("preallocate", Problem::MissingValue),
            ("preallocate sometimes", Problem::BadBool),
            ("preallocate yes\npreallocate no", Problem::DuplicateKey),
        ];
        for (text, problem) in cases {
            match Config::parse(text, &d) {
                Err(Error::Line { problem: p, .. }) => assert_eq!(p, problem, "text {text:?}"),
                other => panic!("text {text:?}: expected {problem:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn remote_sam_needs_the_ugly_flag() {
        let d = defaults();
        let err = Config::parse("sam_address 10.0.0.2:7656\n", &d).unwrap_err();
        assert_eq!(
            err,
            Error::Line {
                line: 1,
                problem: Problem::RemoteSam
            }
        );

        // The ugly flag gets you past the loopback rule and straight into the
        // truth: this build dials 127.0.0.1 and cannot do otherwise, so saying
        // "I know" does not make a remote bridge work. It used to be accepted
        // here and then quietly redirected to the local router — a different
        // machine than the one the operator deliberately asked for.
        assert_eq!(
            Config::parse("sam_address 10.0.0.2:7656\ni_know_sam_is_remote yes\n", &d).unwrap_err(),
            Error::Line {
                line: 1,
                problem: Problem::UnsupportedSamTransport("a remote SAM bridge is not supported")
            }
        );

        // A unix socket is well-formed, was accepted, and left the daemon with
        // no router at all — `cloved -C` said the configuration was good.
        assert_eq!(
            Config::parse("sam_address /run/sam.sock\n", &d).unwrap_err(),
            Error::Line {
                line: 1,
                problem: Problem::UnsupportedSamTransport(
                    "a unix-socket SAM address is not supported; use host:port"
                )
            }
        );

        // Loopback spellings still pass, or the check above is just refusing
        // everything.
        for addr in ["127.0.0.1:7656", "localhost:7656", "[::1]:7656"] {
            let text = format!("sam_address {addr}\n");
            assert!(Config::parse(&text, &d).is_ok(), "rejected {addr}");
        }
        // Exotic loopback does not get the benefit of the doubt.
        assert!(Config::parse("sam_address 127.0.0.2:7656\n", &d).is_err());
    }
}
