//! The clove engine: everything between the `i2pnet` boundary below and the
//! daemon/API above. This crate never touches a socket — peer addressing is
//! [`i2pnet::DestHash`] only, enforced by the workspace `clippy.toml`.

/// The release name everything user-facing prints: `YYYY.0M`, a counter
/// appended for a second release in one month (SCOPE §9).
///
/// Written out because semver cannot spell the leading zero in `08`; the
/// manifest carries the same release as `2026.8.0`.
pub const VERSION: &str = "2026.08";

pub mod bencode;
pub mod bitfield;
pub mod budget;
pub mod choker;
pub mod config;
pub mod extension;
pub mod http;
pub mod json;
pub mod magnet;
pub mod metadata;
pub mod metainfo;
pub mod pex;
pub mod picker;
pub mod resume;
pub mod storage;
pub mod swarm;
pub mod text;
pub mod torrent;
pub mod tracker;
pub mod wire;

#[cfg(test)]
mod tests {
    use super::VERSION;

    /// Nothing but this connects the two spellings: a bump that touches only
    /// `Cargo.toml` would ship a daemon reporting last month's release.
    #[test]
    fn version_matches_the_manifest() {
        let major = env!("CARGO_PKG_VERSION_MAJOR");
        let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();
        let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap();
        let expected = if patch == 0 {
            format!("{major}.{minor:02}")
        } else {
            format!("{major}.{minor:02}.{patch}")
        };
        assert_eq!(VERSION, expected);
    }

    /// The scheme, not this release: `2026.8` sorts wrong, `2026.13` is not a
    /// month.
    #[test]
    fn version_is_a_calendar_date() {
        let (year, rest) = VERSION.split_once('.').unwrap();
        let month = rest.split_once('.').map_or(rest, |(m, _)| m);
        assert_eq!(year.len(), 4, "the year is spelled in full: {VERSION}");
        assert_eq!(month.len(), 2, "the month is zero-padded: {VERSION}");
        assert!(year.parse::<u32>().unwrap() >= 2026);
        assert!(
            (1..=12).contains(&month.parse::<u32>().unwrap()),
            "{VERSION}"
        );
    }
}
