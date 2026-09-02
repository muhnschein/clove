//! Caching wrapper over [`I2pNamingLookup`] (R6, `docs/SCOPE.md` §7).
//!
//! b32 resolution over SAM is slow and can fail transiently; announce loops
//! and peer acquisition must not hammer the router with repeat lookups.
//! Policy:
//!
//! - **Positive results cache forever.** A hostname's destination hash is
//!   stable for practical purposes (changing it is publishing a new name);
//!   a daemon restart naturally refreshes.
//! - **Negative results back off.** A failed name is refused locally until
//!   its retry time, doubling from [`NEGATIVE_INITIAL`] to
//!   [`NEGATIVE_MAX`] on consecutive failures, so one dead tracker URL
//!   cannot generate a lookup per announce attempt.
//! - **A name the resolver refused outright is not cached.** Refusal
//!   (`InvalidInput`: a space, a control character, an absurd length) is
//!   not a failure to resolve, and repeating it from here would repeat the
//!   name in a message of our own.
//! - **The cache is bounded** at [`MAX_ENTRIES`]; past that, the oldest
//!   miss goes first, then the oldest hit.
//!
//! Cheap to clone; clones share the cache — wrap the session's resolver once
//! and hand copies to every announcer.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::sam::scrub_char;
use crate::{DestHash, I2pNamingLookup};

/// First negative-cache hold after a failed lookup.
pub const NEGATIVE_INITIAL: Duration = Duration::from_secs(30);

/// Ceiling for the doubling negative-cache hold.
pub const NEGATIVE_MAX: Duration = Duration::from_secs(30 * 60);

/// Most names the cache holds at once.
///
/// Every name here is a tracker host out of a torrent the operator added, so
/// growth is bounded by what they add — but a torrent is a file a stranger
/// wrote, and a cache with no ceiling is one a thousand such files can fill.
/// Far past any real tracker count.
pub const MAX_ENTRIES: usize = 4096;

/// A shared lookup cache in front of a resolver.
pub struct NamingCache<N> {
    inner: Arc<Inner<N>>,
}

impl<N> Clone for NamingCache<N> {
    fn clone(&self) -> Self {
        NamingCache {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<N> {
    resolver: N,
    entries: Mutex<Cache>,
}

/// The entries, plus a counter that says which was put in when: eviction
/// is by insertion order, and a counter is exact where a clock read can tie.
#[derive(Default)]
struct Cache {
    by_name: HashMap<String, Entry>,
    inserted: u64,
}

enum Entry {
    Hit {
        dest: DestHash,
        /// Value of [`Cache::inserted`] when this went in.
        since: u64,
    },
    Miss {
        until: Instant,
        failures: u32,
        since: u64,
    },
}

impl Entry {
    fn since(&self) -> u64 {
        match self {
            Entry::Hit { since, .. } | Entry::Miss { since, .. } => *since,
        }
    }
}

impl Cache {
    /// Put `name` in, making room if need be. A miss is evicted before any
    /// hit — it expires on its own and costs one lookup to rebuild, where a
    /// hit is the whole point of the cache — and among equals the one that
    /// has sat longest goes first.
    fn insert(&mut self, name: &str, entry: impl FnOnce(u64) -> Entry) {
        if self.by_name.len() >= MAX_ENTRIES && !self.by_name.contains_key(name) {
            let victim = self
                .oldest(|e| matches!(e, Entry::Miss { .. }))
                .or_else(|| self.oldest(|_| true));
            if let Some(victim) = victim {
                self.by_name.remove(&victim);
            }
        }
        self.inserted = self.inserted.wrapping_add(1);
        self.by_name.insert(name.to_owned(), entry(self.inserted));
    }

    fn oldest(&self, of: impl Fn(&Entry) -> bool) -> Option<String> {
        self.by_name
            .iter()
            .filter(|(_, e)| of(e))
            .min_by_key(|(_, e)| e.since())
            .map(|(name, _)| name.clone())
    }
}

impl<N: I2pNamingLookup> NamingCache<N> {
    /// Wrap `resolver` with a fresh cache.
    pub fn new(resolver: N) -> NamingCache<N> {
        NamingCache {
            inner: Arc::new(Inner {
                resolver,
                entries: Mutex::new(Cache::default()),
            }),
        }
    }
}

impl<N: I2pNamingLookup> I2pNamingLookup for NamingCache<N> {
    fn lookup(&self, name: &str) -> io::Result<DestHash> {
        let now = Instant::now();
        let prior_failures = {
            let entries = self
                .inner
                .entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match entries.by_name.get(name) {
                Some(Entry::Hit { dest, .. }) => return Ok(*dest),
                Some(Entry::Miss {
                    until, failures, ..
                }) => {
                    if *until > now {
                        // Name the hold. Without it, an operator watching a
                        // stalled announce sees no lookup traffic at all and
                        // concludes the router is broken — when in fact we
                        // decided locally not to ask it for another 27
                        // minutes. The number is the whole message.
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            // Scrubbed rather than `{name:?}`: this text is
                            // read by an operator and travels through the
                            // daemon's JSON, and Rust's debug quoting would
                            // bracket the host in escaped quotes in both
                            // places for no gain — but the name is a
                            // stranger's text, and reaches a terminal.
                            format!(
                                "naming: {} is negative-cached after {failures} failed \
                                 lookup(s); not asking the router again for {}s",
                                name.chars().map(scrub_char).collect::<String>(),
                                until.saturating_duration_since(now).as_secs()
                            ),
                        ));
                    }
                    *failures
                }
                None => 0,
            }
        };
        // Resolve outside the lock: SAM lookups are slow, and a stampede of
        // duplicate lookups for one name costs less than serializing every
        // name behind one in-flight resolution.
        match self.inner.resolver.lookup(name) {
            Ok(dest) => {
                self.inner
                    .entries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name, |since| Entry::Hit { dest, since });
                Ok(dest)
            }
            Err(e) => {
                // Refused before any router was asked: not a name that failed
                // to resolve, and not one to remember. The resolver's own
                // refusal — which scrubs — is then what every lookup sees.
                if e.kind() == io::ErrorKind::InvalidInput {
                    return Err(e);
                }
                let failures = prior_failures.saturating_add(1);
                let shift = failures.saturating_sub(1).min(6);
                let hold = NEGATIVE_INITIAL
                    .saturating_mul(1 << shift)
                    .min(NEGATIVE_MAX);
                let mut entries = self
                    .inner
                    .entries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                // A lookup of the same name that ran alongside this one may
                // have succeeded meanwhile; a miss must not bury its hit.
                if !matches!(entries.by_name.get(name), Some(Entry::Hit { .. })) {
                    entries.insert(name, |since| Entry::Miss {
                        until: now + hold,
                        failures,
                        since,
                    });
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;

    /// A resolver that counts calls and fails until told otherwise.
    struct Fake {
        calls: AtomicU32,
        works: AtomicBool,
        /// What a failure looks like.
        refusal: io::ErrorKind,
    }

    impl Fake {
        fn new(works: bool) -> Fake {
            Fake {
                calls: AtomicU32::new(0),
                works: AtomicBool::new(works),
                refusal: io::ErrorKind::NotFound,
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl I2pNamingLookup for &Fake {
        fn lookup(&self, _name: &str) -> io::Result<DestHash> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.works.load(Ordering::Relaxed) {
                Ok(DestHash([7; 32]))
            } else {
                Err(io::Error::new(self.refusal, "no such name"))
            }
        }
    }

    fn entries<N>(cache: &NamingCache<N>) -> usize {
        cache
            .inner
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_name
            .len()
    }

    #[test]
    fn positive_results_resolve_once() {
        let fake = Fake::new(true);
        let cache = NamingCache::new(&fake);
        assert_eq!(cache.lookup("t.i2p").unwrap(), DestHash([7; 32]));
        assert_eq!(cache.lookup("t.i2p").unwrap(), DestHash([7; 32]));
        assert_eq!(cache.lookup("t.i2p").unwrap(), DestHash([7; 32]));
        assert_eq!(fake.calls(), 1);
    }

    #[test]
    fn negative_results_back_off() {
        let fake = Fake::new(false);
        let cache = NamingCache::new(&fake);
        assert!(cache.lookup("dead.i2p").is_err());
        // Within the hold: refused locally, resolver untouched.
        assert!(cache.lookup("dead.i2p").is_err());
        assert!(cache.lookup("dead.i2p").is_err());
        assert_eq!(fake.calls(), 1);
    }

    #[test]
    fn clones_share_the_cache() {
        let fake = Fake::new(true);
        let cache = NamingCache::new(&fake);
        let clone = cache.clone();
        let _ = cache.lookup("t.i2p");
        let _ = clone.lookup("t.i2p");
        assert_eq!(fake.calls(), 1);
    }

    /// A name the resolver would not put in a command is refused every time
    /// by the resolver, whose refusal scrubs it — never by a cached miss.
    #[test]
    fn a_refused_name_is_not_negative_cached() {
        let fake = Fake {
            refusal: io::ErrorKind::InvalidInput,
            ..Fake::new(false)
        };
        let cache = NamingCache::new(&fake);
        for _ in 0..3 {
            let e = cache.lookup("bad name.i2p").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(fake.calls(), 3, "a refusal was cached");
        assert_eq!(entries(&cache), 0);
    }

    /// The cached-miss message repeats the name, and the name is a tracker
    /// host out of a file a stranger wrote.
    #[test]
    fn a_negative_cached_name_cannot_forge_a_log_line() {
        let fake = Fake::new(false);
        let cache = NamingCache::new(&fake);
        let name = "tracker\u{1b}[2J\r\ncloved: all is well.i2p";
        let _ = cache.lookup(name);
        let e = cache.lookup(name).unwrap_err();
        let text = e.to_string();
        assert!(text.contains("negative-cached"), "{text}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\r') && !text.contains('\n'),
            "{text:?}"
        );
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_misses_before_hits() {
        let fake = Fake::new(true);
        let cache = NamingCache::new(&fake);
        assert!(cache.lookup("keep.i2p").is_ok());
        fake.works.store(false, Ordering::Relaxed);
        for i in 0..MAX_ENTRIES {
            let _ = cache.lookup(&format!("dead{i}.i2p"));
        }
        assert_eq!(entries(&cache), MAX_ENTRIES, "the cap was not applied");
        // The one hit outlived every miss...
        let before = fake.calls();
        assert!(cache.lookup("keep.i2p").is_ok());
        assert_eq!(fake.calls(), before, "the hit was evicted for a miss");
        // ...and it was the oldest miss that went: `dead1` is still held
        // (refused locally), `dead0` is gone (looked up again). In that
        // order, since re-inserting `dead0` into a full cache evicts the next
        // oldest miss, which is `dead1`.
        let before = fake.calls();
        let _ = cache.lookup("dead1.i2p");
        assert_eq!(fake.calls(), before, "a younger miss was evicted");
        let _ = cache.lookup("dead0.i2p");
        assert_eq!(fake.calls(), before + 1, "the oldest miss survived");
    }

    #[test]
    fn a_full_cache_of_hits_evicts_the_oldest() {
        let fake = Fake::new(true);
        let cache = NamingCache::new(&fake);
        for i in 0..=MAX_ENTRIES {
            assert!(cache.lookup(&format!("h{i}.i2p")).is_ok());
        }
        assert_eq!(entries(&cache), MAX_ENTRIES);
        let before = fake.calls();
        assert!(cache.lookup("h1.i2p").is_ok());
        assert_eq!(fake.calls(), before, "a younger hit was evicted");
        assert!(cache.lookup("h0.i2p").is_ok());
        assert_eq!(fake.calls(), before + 1, "the oldest hit survived");
    }

    /// A resolver whose first call blocks until released and then fails,
    /// while every later call succeeds: two lookups of one name in flight,
    /// the slow one losing.
    struct Gated {
        calls: AtomicU32,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl I2pNamingLookup for &Gated {
        fn lookup(&self, _name: &str) -> io::Result<DestHash> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let gate = self
                    .release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such name"));
            }
            Ok(DestHash([7; 32]))
        }
    }

    #[test]
    fn a_late_miss_does_not_bury_a_hit_that_landed_meanwhile() {
        let (release, gate) = mpsc::sync_channel(1);
        let gated = Gated {
            calls: AtomicU32::new(0),
            release: Mutex::new(Some(gate)),
        };
        let cache = NamingCache::new(&gated);
        std::thread::scope(|s| {
            let slow = s.spawn(|| cache.lookup("t.i2p"));
            // Once the slow lookup is inside the resolver, a second one
            // resolves and caches a hit.
            while gated.calls.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(cache.lookup("t.i2p").is_ok());
            release.send(()).unwrap();
            assert!(slow.join().unwrap().is_err());
        });
        // The hit is still there: no resolver call, no refusal.
        let before = gated.calls.load(Ordering::SeqCst);
        assert!(cache.lookup("t.i2p").is_ok());
        assert_eq!(gated.calls.load(Ordering::SeqCst), before);
    }
}
