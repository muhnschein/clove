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
//!
//! Cheap to clone; clones share the cache — wrap the session's resolver once
//! and hand copies to every announcer.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::{DestHash, I2pNamingLookup};

/// First negative-cache hold after a failed lookup.
pub const NEGATIVE_INITIAL: Duration = Duration::from_secs(30);

/// Ceiling for the doubling negative-cache hold.
pub const NEGATIVE_MAX: Duration = Duration::from_secs(30 * 60);

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
    entries: Mutex<HashMap<String, Entry>>,
}

enum Entry {
    Hit(DestHash),
    Miss { until: Instant, failures: u32 },
}

impl<N: I2pNamingLookup> NamingCache<N> {
    /// Wrap `resolver` with a fresh cache.
    pub fn new(resolver: N) -> NamingCache<N> {
        NamingCache {
            inner: Arc::new(Inner {
                resolver,
                entries: Mutex::new(HashMap::new()),
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
            match entries.get(name) {
                Some(Entry::Hit(dest)) => return Ok(*dest),
                Some(Entry::Miss { until, failures }) => {
                    if *until > now {
                        // Name the hold. Without it, an operator watching a
                        // stalled announce sees no lookup traffic at all and
                        // concludes the router is broken — when in fact we
                        // decided locally not to ask it for another 27
                        // minutes. The number is the whole message.
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            // Plain, not `{name:?}`: this text is read by an
                            // operator and travels through the daemon's JSON,
                            // and Rust's debug quoting would bracket the host
                            // in escaped quotes in both places for no gain.
                            format!(
                                "naming: {name} is negative-cached after {failures} failed \
                                 lookup(s); not asking the router again for {}s",
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
                    .insert(name.to_owned(), Entry::Hit(dest));
                Ok(dest)
            }
            Err(e) => {
                let failures = prior_failures.saturating_add(1);
                let shift = failures.saturating_sub(1).min(6);
                let hold = NEGATIVE_INITIAL
                    .saturating_mul(1 << shift)
                    .min(NEGATIVE_MAX);
                self.inner
                    .entries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        name.to_owned(),
                        Entry::Miss {
                            until: now + hold,
                            failures,
                        },
                    );
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A resolver that counts calls and fails until told otherwise.
    struct Fake {
        calls: AtomicU32,
        works: std::sync::atomic::AtomicBool,
    }

    impl I2pNamingLookup for &Fake {
        fn lookup(&self, _name: &str) -> io::Result<DestHash> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.works.load(Ordering::Relaxed) {
                Ok(DestHash([7; 32]))
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "no such name"))
            }
        }
    }

    #[test]
    fn positive_results_resolve_once() {
        let fake = Fake {
            calls: AtomicU32::new(0),
            works: std::sync::atomic::AtomicBool::new(true),
        };
        let cache = NamingCache::new(&fake);
        assert_eq!(cache.lookup("t.i2p").unwrap(), DestHash([7; 32]));
        assert_eq!(cache.lookup("t.i2p").unwrap(), DestHash([7; 32]));
        assert_eq!(cache.lookup("t.i2p").unwrap(), DestHash([7; 32]));
        assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn negative_results_back_off() {
        let fake = Fake {
            calls: AtomicU32::new(0),
            works: std::sync::atomic::AtomicBool::new(false),
        };
        let cache = NamingCache::new(&fake);
        assert!(cache.lookup("dead.i2p").is_err());
        // Within the hold: refused locally, resolver untouched.
        assert!(cache.lookup("dead.i2p").is_err());
        assert!(cache.lookup("dead.i2p").is_err());
        assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clones_share_the_cache() {
        let fake = Fake {
            calls: AtomicU32::new(0),
            works: std::sync::atomic::AtomicBool::new(true),
        };
        let cache = NamingCache::new(&fake);
        let clone = cache.clone();
        let _ = cache.lookup("t.i2p");
        let _ = clone.lookup("t.i2p");
        assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
    }
}
