//! The clove engine: everything between the `i2pnet` boundary below and the
//! daemon/API above. This crate never touches a socket — peer addressing is
//! [`i2pnet::DestHash`] only, enforced by the workspace `clippy.toml`.

/// The release name: the full year and the zero-padded month it was cut in
/// (SCOPE §9, calver.org), with a trailing counter only when a month needed a
/// second release — `2026.08`, then `2026.08.1`.
///
/// Everything user-facing prints this: `clove status`, the listing header,
/// `GET /v1/status`, and the tracker client string. It is written out rather
/// than derived from `CARGO_PKG_VERSION` because semver has no way to spell
/// the leading zero in `08`; the manifest carries the same release as
/// `2026.8.0`, and the test below fails if a bump touches only one of them.
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

    /// The release name and the manifest version are two spellings of one
    /// thing, and nothing but this test connects them: a release that bumps
    /// `Cargo.toml` and forgets `VERSION` would ship a daemon reporting last
    /// month's name, which is exactly the field an interoperability report is
    /// asked for.
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

    /// The scheme itself, not this release: a year that is four digits and a
    /// month that is two and lands in the calendar. A typo like `2026.8` or
    /// `2026.13` is a release name that sorts wrong or means nothing.
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
