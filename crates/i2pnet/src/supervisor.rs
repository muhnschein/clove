//! SAM session reconnection policy (SCOPE §4).
//!
//! This is the arithmetic behind the state machine the scope calls Suspect #1
//! for XD-style flakiness: when the router restarts or the SAM control
//! connection drops, the whole session tree must come back on its own, on an
//! exponential backoff, without a thundering-herd reconnect and without the
//! engine above ever seeing a torn-down session as anything but a visible
//! "waiting for router" pause.
//!
//! The cycle itself lives in `cloved` — it owns the session, the forwarded
//! listener, the inbound demux and the registry, and the order in which those
//! are built and torn down is the daemon's business:
//!
//! ```text
//!            build() Ok
//!   Down ───────────────────► Up(session)
//!    ▲  │                        │
//!    │  │ build() Err            │ session lost (health probe fails)
//!    │  ▼                        │
//!    └ Backoff(delay) ◄──────────┘
//!        │  delay elapses, retry build()
//!        └► Down (attempt again)
//! ```
//!
//! What lives *here* is the part worth testing on its own and reusing
//! unchanged: how long to wait before attempt *n*, and how much to shave off
//! that wait so a host running several daemons does not retry in lockstep.
//! Keeping it pure means the daemon's loop has no arithmetic of its own to get
//! wrong, and this file needs no router to test.
//!
//! There used to be a `Supervisor` here that drove a `SessionFactory` through
//! the cycle above. It was never used: the session tree is three objects with
//! different owners (an `Arc` session, a listener moved into an accept loop, a
//! registry to attach), and threading that through a generic factory cost more
//! than it explained. The daemon's own loop is the one that runs, so the
//! untested duplicate is gone (SCOPE §9, culture of deletion) and the policy it
//! calls is what is tested below.

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
        // `clamp` passes NaN through, and `Duration::mul_f64` panics on it, so
        // a roll that is not a number becomes no jitter rather than a crash.
        let roll = if roll.is_nan() {
            0.0
        } else {
            roll.clamp(0.0, 1.0)
        };
        let factor = 1.0 - jitter * roll;
        base.mul_f64(factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(p.base_delay(u32::MAX), Duration::from_secs(60));
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
        // A roll outside [0,1] — or not a number at all — must not stretch the
        // delay or produce something Duration cannot hold.
        assert_eq!(p.jittered(base, -1.0), base);
        assert_eq!(p.jittered(base, 5.0), Duration::from_secs(5));
        assert!(p.jittered(base, f64::NAN) <= base);
    }

    #[test]
    fn no_thundering_herd_with_jitter() {
        // Two daemons failing in lockstep get different retry deadlines once
        // jitter is applied with different rolls — the whole point of it.
        let p = ReconnectPolicy {
            jitter: 0.5,
            ..policy()
        };
        let base = p.base_delay(4);
        assert_ne!(
            p.jittered(base, 0.1),
            p.jittered(base, 0.9),
            "different rolls must de-sync retries"
        );
    }

    #[test]
    fn the_default_policy_is_sane() {
        let p = ReconnectPolicy::default();
        assert!(p.base_delay(1) >= Duration::from_millis(500));
        assert!(p.base_delay(100) <= p.max);
        assert!(
            p.jitter > 0.0,
            "the default must de-sync concurrent daemons"
        );
    }
}
