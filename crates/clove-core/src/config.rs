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
        Ok(Defaults {
            data_home,
            runtime_dir,
        })
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Validated daemon configuration.
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
}

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
    /// SAM address is neither `host:port` nor an absolute socket path.
    BadSamAddress,
    /// SAM address is not loopback and `i_know_sam_is_remote` is not set.
    RemoteSam,
    /// A path value that is not absolute.
    RelativePath,
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
            Problem::BadSamAddress => {
                write!(
                    f,
                    "sam_address must be host:port or an absolute socket path"
                )
            }
            Problem::RemoteSam => write!(
                f,
                "sam_address is not loopback; if you really run SAM remotely, set i_know_sam_is_remote yes (dangerous: your traffic to the router is unprotected)"
            ),
            Problem::RelativePath => write!(f, "path must be absolute"),
        }
    }
}

impl std::error::Error for Error {}

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
        let mut sam_address: Option<(usize, String)> = None;
        let mut i_know_sam_is_remote: Option<(usize, bool)> = None;
        let mut data_dir: Option<(usize, PathBuf)> = None;
        let mut api_socket: Option<(usize, PathBuf)> = None;
        let mut ephemeral: Option<(usize, bool)> = None;

        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let at = |problem| Error::Line { line, problem };
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (key, value) = match trimmed.split_once(char::is_whitespace) {
                Some((k, v)) if !v.trim().is_empty() => (k, v.trim()),
                _ => return Err(at(Problem::MissingValue)),
            };
            match key {
                "sam_address" => {
                    if !looks_like_sam_address(value) {
                        return Err(at(Problem::BadSamAddress));
                    }
                    set(&mut sam_address, line, value.to_owned())?;
                }
                "i_know_sam_is_remote" => {
                    let flag = parse_bool(value).ok_or_else(|| at(Problem::BadBool))?;
                    set(&mut i_know_sam_is_remote, line, flag)?;
                }
                "data_dir" => set(&mut data_dir, line, absolute(value, line)?)?,
                "api_socket" => set(&mut api_socket, line, absolute(value, line)?)?,
                "ephemeral" => {
                    let flag = parse_bool(value).ok_or_else(|| at(Problem::BadBool))?;
                    set(&mut ephemeral, line, flag)?;
                }
                other => return Err(at(Problem::UnknownKey(other.to_owned()))),
            }
        }

        let i_know_sam_is_remote = i_know_sam_is_remote.is_some_and(|(_, v)| v);
        let (sam_line, sam_address) = sam_address.unwrap_or((0, DEFAULT_SAM_ADDRESS.to_owned()));
        if !i_know_sam_is_remote && !is_loopback_sam(&sam_address) {
            return Err(Error::Line {
                line: sam_line,
                problem: Problem::RemoteSam,
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
        })
    }
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
        }
    }

    #[test]
    fn empty_config_is_the_working_default() {
        let c = Config::parse("", &defaults()).unwrap();
        assert_eq!(c.sam_address, DEFAULT_SAM_ADDRESS);
        assert!(!c.i_know_sam_is_remote);
        assert!(!c.ephemeral);
        assert_eq!(c.data_dir, PathBuf::from("/home/u/.local/share/clove"));
        assert_eq!(c.api_socket, PathBuf::from("/run/user/1000/clove.sock"));
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
";
        let c = Config::parse(text, &defaults()).unwrap();
        assert_eq!(c.sam_address, "127.0.0.1:7657");
        assert_eq!(c.data_dir, PathBuf::from("/srv/clove"));
        assert_eq!(c.api_socket, PathBuf::from("/run/clove.sock"));
        assert!(c.ephemeral);
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

        let c = Config::parse("sam_address 10.0.0.2:7656\ni_know_sam_is_remote yes\n", &d).unwrap();
        assert!(c.i_know_sam_is_remote);

        // Loopback spellings that pass; unix socket too.
        for addr in [
            "127.0.0.1:7656",
            "localhost:7656",
            "[::1]:7656",
            "/run/sam.sock",
        ] {
            let text = format!("sam_address {addr}\n");
            assert!(Config::parse(&text, &d).is_ok(), "rejected {addr}");
        }
        // Exotic loopback does not get the benefit of the doubt.
        assert!(Config::parse("sam_address 127.0.0.2:7656\n", &d).is_err());
    }
}
