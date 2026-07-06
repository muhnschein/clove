//! SAM session supervision and reconnection (SCOPE §4).
//!
//! This is the state machine the scope calls Suspect #1 for XD-style
//! flakiness: when the router restarts or the SAM control connection drops,
//! the whole session tree must come back on its own, on an exponential
//! backoff, without a thundering-herd reconnect and without the engine
//! above ever seeing a torn-down session as anything but a visible
//! "waiting for router" pause.
//!
//! The policy is kept pure and independent of `yosemite` so it is testable
//! with no router (the SAM backend is Phase D's other half). A [`Supervisor`]
//! owns a [`SessionFactory`] — the only thing that actually touches the
//! network — and drives it through this cycle:
//!
//! ```text
//!            build() Ok
//!   Down ───────────────────► Up(session)
//!    ▲  │                        │
//!    │  │ build() Err            │ session lost (caller reports failure)
//!    │  ▼                        │
//!    └ Backoff(delay) ◄──────────┘
//!        │  delay elapses, retry build()
//!        └► Down (attempt again)
//! ```
//!
//! Backoff doubles from [`ReconnectPolicy::initial`] to a
//! [`ReconnectPolicy::max`] ceiling, with optional jitter to prevent a whole
//! swarm of torrents re-announcing in lockstep (the thundering herd). The
//! supervisor never sleeps internally: it reports *when* the next attempt is
//! due and the caller schedules it, so this stays single-threaded and
//! deterministic under test.

use std::time::Duration;

/// How reconnection backs off. Delays double each failed attempt from
/// `initial`, capped at `max`; `jitter` (0.0–1.0) randomly shortens each
/// delay by up to that fraction to de-synchronize many supervisors.
#[derive(Clone, Copy, Debug)]
pub struct ReconnectPolicy {
    /// Delay after the first failure.
    pub initial: Duration,
    /// Ceiling the doubling delay never exceeds.
    pub max: Duration,
    /// Fraction of each delay that may be randomly shaved off (0.0 = none,
    /// 1.0 = down to zero). De-synchronizes concurrent reconnects.
    pub jitter: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        ReconnectPolicy {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            jitter: 0.2,
        }
    }
}

impl ReconnectPolicy {
    /// The delay for retry attempt `failures` (1 = first retry), doubling
    /// from `initial` and capped at `max`, before jitter.
    #[must_use]
    pub fn base_delay(&self, failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        // Double (failures - 1) times, saturating at max. Work in
        // milliseconds; saturate rather than overflow on long outages.
        let initial_ms = u64::try_from(self.initial.as_millis()).unwrap_or(u64::MAX);
        let max_ms = u64::try_from(self.max.as_millis()).unwrap_or(u64::MAX);
        let shift = failures - 1;
        let delay_ms = if shift >= 63 {
            max_ms
        } else {
            initial_ms.saturating_mul(1u64 << shift).min(max_ms)
        };
        Duration::from_millis(delay_ms)
    }

    /// Apply `jitter` to a base delay. `roll` is a caller-supplied value in
    /// `[0.0, 1.0)` (injected so tests are deterministic and no RNG
    /// dependency is pulled in); the delay is shortened by `roll * jitter`.
    #[must_use]
    pub fn jittered(&self, base: Duration, roll: f64) -> Duration {
        let jitter = self.jitter.clamp(0.0, 1.0);
        let roll = roll.clamp(0.0, 1.0);
        let factor = 1.0 - jitter * roll;
        base.mul_f64(factor)
    }
}

/// Builds SAM sessions. The one piece the supervisor delegates to the
/// network; the real impl (Phase D SAM backend) connects to the router,
/// the test impl fails or succeeds on command.
pub trait SessionFactory {
    /// The session handle produced on success.
    type Session;
    /// Why a build attempt failed.
    type Error;

    /// Attempt to establish a fresh session tree (control connection,
    /// primary session, forwarded listener).
    ///
    /// # Errors
    /// The router is unreachable or rejected the session.
    fn build(&mut self) -> Result<Self::Session, Self::Error>;
}

/// Supervisor lifecycle, exposed so the engine can surface "waiting for
/// router" to torrents and the CLI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// No session; an attempt should be made now.
    Down,
    /// A session is established.
    Up,
    /// A recent attempt failed; waiting out the backoff before retrying.
    WaitingForRouter,
}

/// Drives a [`SessionFactory`] through connect/backoff/reconnect. Holds no
/// threads and never sleeps: [`poll`](Self::poll) does one unit of work and
/// reports the next deadline for the caller to schedule.
pub struct Supervisor<F: SessionFactory> {
    factory: F,
    policy: ReconnectPolicy,
    session: Option<F::Session>,
    /// Consecutive failures since the last success; drives backoff.
    failures: u32,
    phase: Phase,
}

/// What [`Supervisor::poll`] did and what the caller should do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Poll {
    /// A session is up; nothing to do until it is reported lost.
    Up,
    /// An attempt just failed; do not retry until `retry_in` has elapsed,
    /// then poll again.
    Backoff {
        /// Delay before the next [`poll`](Supervisor::poll) should run.
        retry_in: Duration,
        /// Which consecutive failure this was (1 = first).
        failures: u32,
    },
}

impl<F: SessionFactory> Supervisor<F> {
    /// A supervisor that will build sessions with `factory` under `policy`,
    /// starting [`Phase::Down`] (no session yet).
    pub fn new(factory: F, policy: ReconnectPolicy) -> Self {
        Supervisor {
            factory,
            policy,
            session: None,
            failures: 0,
            phase: Phase::Down,
        }
    }

    /// Current lifecycle phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The live session, if [`Phase::Up`].
    #[must_use]
    pub fn session(&self) -> Option<&F::Session> {
        self.session.as_ref()
    }

    /// Consecutive failures since the last successful build.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Report that the live session was lost (control socket dropped, router
    /// gone). Transitions back to [`Phase::Down`] so the next
    /// [`poll`](Self::poll) attempts a rebuild. The dropped session's failure
    /// is not itself counted — only failed *rebuilds* grow the backoff — so
    /// a healthy session that dies once retries promptly.
    pub fn report_lost(&mut self) {
        self.session = None;
        self.phase = Phase::Down;
    }

    /// Do one unit of work: if a session is up, report [`Poll::Up`];
    /// otherwise attempt a rebuild, returning [`Poll::Up`] on success or
    /// [`Poll::Backoff`] (with the deadline the caller should wait) on
    /// failure. `roll` supplies jitter in `[0.0, 1.0)` — pass `0.0` for no
    /// jitter, or a per-attempt random value in production.
    pub fn poll(&mut self, roll: f64) -> Poll {
        if self.session.is_some() {
            self.phase = Phase::Up;
            return Poll::Up;
        }
        if let Ok(session) = self.factory.build() {
            self.session = Some(session);
            self.failures = 0;
            self.phase = Phase::Up;
            Poll::Up
        } else {
            self.failures = self.failures.saturating_add(1);
            self.phase = Phase::WaitingForRouter;
            let base = self.policy.base_delay(self.failures);
            Poll::Backoff {
                retry_in: self.policy.jittered(base, roll),
                failures: self.failures,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A factory that fails its first `fail_first` builds, then succeeds,
    /// counting total attempts.
    struct FlakyFactory {
        fail_first: u32,
        attempts: u32,
    }

    impl SessionFactory for FlakyFactory {
        type Session = u32; // a stand-in session id
        type Error = ();

        fn build(&mut self) -> Result<u32, ()> {
            self.attempts += 1;
            if self.attempts <= self.fail_first {
                Err(())
            } else {
                Ok(self.attempts)
            }
        }
    }

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            jitter: 0.0,
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = policy();
        assert_eq!(p.base_delay(0), Duration::ZERO);
        assert_eq!(p.base_delay(1), Duration::from_secs(1));
        assert_eq!(p.base_delay(2), Duration::from_secs(2));
        assert_eq!(p.base_delay(3), Duration::from_secs(4));
        assert_eq!(p.base_delay(7), Duration::from_secs(60)); // 64 -> capped
        assert_eq!(p.base_delay(1000), Duration::from_secs(60)); // no overflow
    }

    #[test]
    fn jitter_only_shortens_and_stays_bounded() {
        let p = ReconnectPolicy {
            jitter: 0.5,
            ..policy()
        };
        let base = Duration::from_secs(10);
        assert_eq!(p.jittered(base, 0.0), base); // no roll, no change
        assert_eq!(p.jittered(base, 1.0), Duration::from_secs(5)); // max shave
        let mid = p.jittered(base, 0.5);
        assert!(mid <= base && mid >= Duration::from_secs(5));
    }

    #[test]
    fn connects_first_try() {
        let mut sup = Supervisor::new(
            FlakyFactory {
                fail_first: 0,
                attempts: 0,
            },
            policy(),
        );
        assert_eq!(sup.phase(), Phase::Down);
        assert_eq!(sup.poll(0.0), Poll::Up);
        assert_eq!(sup.phase(), Phase::Up);
        assert!(sup.session().is_some());
        assert_eq!(sup.failures(), 0);
    }

    #[test]
    fn backs_off_then_recovers() {
        // Router down for two attempts, then up.
        let mut sup = Supervisor::new(
            FlakyFactory {
                fail_first: 2,
                attempts: 0,
            },
            policy(),
        );

        assert_eq!(
            sup.poll(0.0),
            Poll::Backoff {
                retry_in: Duration::from_secs(1),
                failures: 1
            }
        );
        assert_eq!(sup.phase(), Phase::WaitingForRouter);

        assert_eq!(
            sup.poll(0.0),
            Poll::Backoff {
                retry_in: Duration::from_secs(2),
                failures: 2
            }
        );

        // Third attempt succeeds; backoff resets.
        assert_eq!(sup.poll(0.0), Poll::Up);
        assert_eq!(sup.phase(), Phase::Up);
        assert_eq!(sup.failures(), 0);
    }

    #[test]
    fn a_healthy_session_that_dies_retries_promptly() {
        let mut sup = Supervisor::new(
            FlakyFactory {
                fail_first: 0,
                attempts: 0,
            },
            policy(),
        );
        assert_eq!(sup.poll(0.0), Poll::Up);

        // Router restarts: session lost. We go Down, not straight to a long
        // backoff — the next poll retries immediately (and here succeeds).
        sup.report_lost();
        assert_eq!(sup.phase(), Phase::Down);
        assert!(sup.session().is_none());
        assert_eq!(sup.poll(0.0), Poll::Up);
        assert_eq!(sup.failures(), 0);
    }

    #[test]
    fn poll_while_up_is_idempotent_and_does_not_rebuild() {
        let mut sup = Supervisor::new(
            FlakyFactory {
                fail_first: 0,
                attempts: 0,
            },
            policy(),
        );
        assert_eq!(sup.poll(0.0), Poll::Up);
        let first = *sup.session().unwrap();
        // Polling again must not build a new session.
        assert_eq!(sup.poll(0.0), Poll::Up);
        assert_eq!(*sup.session().unwrap(), first);
    }

    #[test]
    fn no_thundering_herd_with_jitter() {
        // Two supervisors failing in lockstep get different retry deadlines
        // once jitter is applied with different rolls.
        let p = ReconnectPolicy {
            jitter: 0.5,
            ..policy()
        };
        let mut a = Supervisor::new(
            FlakyFactory {
                fail_first: 5,
                attempts: 0,
            },
            p,
        );
        let mut b = Supervisor::new(
            FlakyFactory {
                fail_first: 5,
                attempts: 0,
            },
            p,
        );
        let Poll::Backoff { retry_in: da, .. } = a.poll(0.1) else {
            panic!("expected backoff");
        };
        let Poll::Backoff { retry_in: db, .. } = b.poll(0.9) else {
            panic!("expected backoff");
        };
        assert_ne!(da, db, "different jitter rolls must de-sync retries");
    }
}
