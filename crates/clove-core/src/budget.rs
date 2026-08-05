//! The shared peer budget: one ceiling on how many peer connections exist
//! across every torrent at once.
//!
//! # Discipline
//!
//! A slot is [claimed](PeerBudget::claim) before a connection is registered and
//! returns to the budget when the [`PeerSlot`] is dropped — which is when the
//! peer leaves its torrent's table, connection and threads included. The claim
//! is one atomic compare-exchange, so two torrents attaching at the same
//! instant cannot both take the last slot; [`available`](PeerBudget::available)
//! is the *advisory* read used to avoid dialling peers there is no room for,
//! and is deliberately not authoritative.
//!
//! [`SwarmConfig::max_peers`]: crate::swarm::SwarmConfig::max_peers

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A ceiling on concurrent peer connections, shared by every torrent that was
/// handed it.
#[derive(Debug)]
pub struct PeerBudget {
    limit: usize,
    taken: AtomicUsize,
}

impl PeerBudget {
    /// A budget of `limit` concurrent peer connections.
    #[must_use]
    pub fn new(limit: usize) -> Arc<PeerBudget> {
        Arc::new(PeerBudget {
            limit,
            taken: AtomicUsize::new(0),
        })
    }

    /// A budget that never refuses.
    ///
    /// What a [`Torrent`] built without one gets, so a standalone torrent — a
    /// test, or any use that is not the daemon hosting many — behaves exactly
    /// as it did before there was a budget at all.
    ///
    /// [`Torrent`]: crate::torrent::Torrent
    #[must_use]
    pub fn unlimited() -> Arc<PeerBudget> {
        PeerBudget::new(usize::MAX)
    }

    /// Take one slot, or `None` when the budget is spent.
    ///
    /// Atomic: the compare-exchange is what makes this safe to call from every
    /// torrent's dial workers and the inbound demux at once. A `None` is an
    /// ordinary answer — refuse this connection — not an error.
    #[must_use]
    pub fn claim(self: &Arc<Self>) -> Option<PeerSlot> {
        let mut taken = self.taken.load(Ordering::Relaxed);
        loop {
            if taken >= self.limit {
                return None;
            }
            match self.taken.compare_exchange_weak(
                taken,
                taken + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(PeerSlot {
                        budget: Arc::clone(self),
                    });
                }
                Err(actual) => taken = actual,
            }
        }
    }

    /// Slots in use right now.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.taken.load(Ordering::Relaxed)
    }

    /// The ceiling this budget was built with.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// How many slots are free, as of this instant.
    ///
    /// **Advisory.** It can be stale before the caller reads it, and that is
    /// fine for its only job: keeping a dial sweep from spending a
    /// `dial_timeout` on a peer it would have no room to attach. The answer
    /// that counts is [`claim`](PeerBudget::claim)'s.
    #[must_use]
    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.in_use())
    }
}

/// One claimed connection slot, returned to its budget when dropped.
///
/// Held by the peer it was claimed for, so the accounting cannot drift from
/// the peer table: the slot goes back exactly when the peer is removed, on
/// every path that removes one — idle timeout, protocol violation, pause,
/// session teardown or a panicking thread.
#[derive(Debug)]
pub struct PeerSlot {
    budget: Arc<PeerBudget>,
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        self.budget.taken.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_hands_out_exactly_its_limit() {
        let budget = PeerBudget::new(3);
        assert_eq!(budget.limit(), 3);
        assert_eq!(budget.available(), 3);

        let mut slots: Vec<PeerSlot> = (0..3).filter_map(|_| budget.claim()).collect();
        assert_eq!(slots.len(), 3);
        assert_eq!(budget.in_use(), 3);
        assert_eq!(budget.available(), 0);
        assert!(budget.claim().is_none(), "the fourth must be refused");

        // Returning one makes room for exactly one.
        slots.pop();
        assert_eq!(budget.available(), 1);
        let one = budget.claim();
        assert!(one.is_some());
        assert!(budget.claim().is_none(), "still only the one slot");
        drop(one);
        assert_eq!(budget.in_use(), 2, "the other two are still held");
        drop(slots);
        assert_eq!(budget.in_use(), 0);
    }

    #[test]
    fn an_unlimited_budget_never_refuses() {
        let budget = PeerBudget::unlimited();
        let slots: Vec<PeerSlot> = (0..1000).filter_map(|_| budget.claim()).collect();
        assert_eq!(slots.len(), 1000);
        assert!(budget.claim().is_some());
        // `available` must not wrap or saturate to something surprising. The
        // 1001st claim above was dropped at the end of its statement, so 1000
        // are held here.
        assert_eq!(budget.available(), usize::MAX - 1000);
        assert_eq!(budget.in_use(), 1000);
    }

    #[test]
    fn a_zero_budget_refuses_everything() {
        let budget = PeerBudget::new(0);
        assert!(budget.claim().is_none());
        assert_eq!(budget.available(), 0);
    }

    #[test]
    fn concurrent_claims_never_exceed_the_limit() {
        // The reason `claim` is a compare-exchange and not a load-then-add:
        // every torrent's dial workers and the inbound demux race for the same
        // last slot, and the overshoot a read-modify-write allows is exactly
        // the concurrency this budget exists to bound.
        const LIMIT: usize = 64;
        const THREADS: usize = 16;
        const EACH: usize = 32;
        let budget = PeerBudget::new(LIMIT);
        let peak = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let budget = Arc::clone(&budget);
                let peak = Arc::clone(&peak);
                scope.spawn(move || {
                    // Held, not claimed-and-dropped: releasing immediately
                    // means the ceiling is never pressed against and the test
                    // proves nothing. These go back when the thread ends.
                    let mut held: Vec<PeerSlot> = Vec::new();
                    for _ in 0..EACH {
                        if let Some(slot) = budget.claim() {
                            let now = budget.in_use();
                            peak.fetch_max(now, Ordering::Relaxed);
                            assert!(now <= LIMIT, "{now} slots in use, limit {LIMIT}");
                            held.push(slot);
                        }
                    }
                });
            }
        });

        assert_eq!(budget.in_use(), 0, "every slot came back");
        // The test is worthless if nothing was ever held at once. A single
        // thread that claims all `EACH` of its slots drives `in_use` to at
        // least `EACH` by itself, so this holds however the threads interleave
        // — while `peak == LIMIT` would not, since one thread can finish and
        // release before another starts.
        let peak = peak.load(Ordering::Relaxed);
        assert!(peak >= EACH, "peak {peak} never reached one thread's worth");
        assert!(peak <= LIMIT, "peak {peak} exceeded the limit {LIMIT}");
    }
}
