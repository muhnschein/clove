//! Choke/unchoke decisions (BEP 3 §Choking and Optimistic Unchoking).
//!
//! Pure policy, like the picker: the engine samples each peer's recent
//! transfer rate and current choke state into [`PeerSnapshot`]s, calls
//! [`Choker::plan`] once per round, and applies the returned changes as
//! `choke`/`unchoke` messages. The choker holds no peer objects and does no
//! I/O.
//!
//! Algorithm: interested peers are ranked by rate (download rate from them
//! while leeching — tit-for-tat; upload rate to them while seeding) and the
//! top [`Choker::max_unchoked`] are unchoked. Every third round one
//! *optimistic* slot goes to an otherwise-choked interested peer so new and
//! slow peers get a chance to prove themselves; the optimistic pick rotates
//! deterministically (no RNG dependency), which is fair over time and keeps
//! the policy testable. Uninterested peers are always choked.
//!
//! Round cadence and `max_unchoked` are the engine's to schedule; all
//! intervals are config-tunable (R5).

use std::collections::HashSet;

/// Default number of peers unchoked at once (BEP 3's customary 4).
pub const DEFAULT_MAX_UNCHOKED: usize = 4;

/// One peer's inputs to a choke round.
#[derive(Clone, Copy, Debug)]
pub struct PeerSnapshot {
    /// Engine-assigned connection id.
    pub id: u64,
    /// Whether the peer is interested in us.
    pub interested: bool,
    /// Recent transfer rate with this peer (bytes per round); the engine
    /// chooses download- or upload-rate per its leech/seed role.
    pub rate: u64,
    /// Whether we are currently unchoking this peer.
    pub unchoked: bool,
}

/// The state changes to apply after a round: peers to newly unchoke and to
/// newly choke. Peers already in the desired state are omitted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// Send `unchoke` to these.
    pub unchoke: Vec<u64>,
    /// Send `choke` to these.
    pub choke: Vec<u64>,
}

/// The choke scheduler.
pub struct Choker {
    /// Peers unchoked at once, including the optimistic slot.
    pub max_unchoked: usize,
    round: u64,
}

impl Default for Choker {
    fn default() -> Self {
        Choker::new(DEFAULT_MAX_UNCHOKED)
    }
}

impl Choker {
    /// Check that a plan is internally consistent (SCOPE §9's paranoid
    /// debug builds): no peer is told to choke and unchoke in the same round,
    /// every named peer exists, and the resulting unchoked set does not
    /// exceed the slot count.
    #[cfg(debug_assertions)]
    fn check_decision(decision: &Decision, peers: &[PeerSnapshot], max_unchoked: usize) {
        for id in &decision.unchoke {
            assert!(
                !decision.choke.contains(id),
                "peer {id} was told to choke and unchoke in one round"
            );
            assert!(
                peers.iter().any(|p| p.id == *id),
                "plan names peer {id}, which is not connected"
            );
        }
        for id in &decision.choke {
            assert!(
                peers.iter().any(|p| p.id == *id),
                "plan names peer {id}, which is not connected"
            );
        }
        // What the peer set looks like once this plan is applied.
        let after = peers
            .iter()
            .filter(|p| {
                decision.unchoke.contains(&p.id) || (p.unchoked && !decision.choke.contains(&p.id))
            })
            .count();
        assert!(
            after <= max_unchoked,
            "plan leaves {after} peers unchoked, over the {max_unchoked} slots"
        );
    }

    /// A choker that unchokes `max_unchoked` peers at a time.
    #[must_use]
    pub fn new(max_unchoked: usize) -> Self {
        Choker {
            max_unchoked: max_unchoked.max(1),
            round: 0,
        }
    }

    /// Advance one round and compute the choke changes for `peers`.
    pub fn plan(&mut self, peers: &[PeerSnapshot]) -> Decision {
        self.round += 1;
        let optimistic_round = self.round.is_multiple_of(3);

        // Interested peers, best rate first; ties broken by id for
        // determinism.
        let mut interested: Vec<&PeerSnapshot> = peers.iter().filter(|p| p.interested).collect();
        interested.sort_by(|a, b| b.rate.cmp(&a.rate).then(a.id.cmp(&b.id)));

        let mut chosen: Vec<u64> = interested
            .iter()
            .take(self.max_unchoked)
            .map(|p| p.id)
            .collect();

        if optimistic_round {
            let candidates: Vec<u64> = interested
                .iter()
                .map(|p| p.id)
                .filter(|id| !chosen.contains(id))
                .collect();
            if !candidates.is_empty() {
                // Indexed by the *optimistic* round, not the round: every
                // third round with `round % len` visits only `len / 3` of the
                // candidates whenever `len` is a multiple of three, and with
                // a stable order the rest were never unchoked at all.
                let idx = usize::try_from(self.round / 3).unwrap_or(0) % candidates.len();
                let pick = candidates[idx];
                // Free a slot by dropping the lowest-rate regular pick.
                if chosen.len() >= self.max_unchoked {
                    chosen.pop();
                }
                chosen.push(pick);
            }
        }

        let chosen: HashSet<u64> = chosen.into_iter().collect();
        let mut decision = Decision::default();
        for peer in peers {
            let want = chosen.contains(&peer.id);
            if want && !peer.unchoked {
                decision.unchoke.push(peer.id);
            } else if !want && peer.unchoked {
                decision.choke.push(peer.id);
            }
        }
        #[cfg(debug_assertions)]
        Self::check_decision(&decision, peers, self.max_unchoked);
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: u64, interested: bool, rate: u64, unchoked: bool) -> PeerSnapshot {
        PeerSnapshot {
            id,
            interested,
            rate,
            unchoked,
        }
    }

    #[test]
    fn seeder_unchokes_its_one_interested_peer() {
        let mut choker = Choker::default();
        let d = choker.plan(&[peer(1, true, 0, false)]);
        assert_eq!(d.unchoke, vec![1]);
        assert!(d.choke.is_empty());
    }

    #[test]
    fn uninterested_peers_stay_choked() {
        let mut choker = Choker::default();
        let d = choker.plan(&[peer(1, false, 999, false), peer(2, false, 5, true)]);
        assert!(d.unchoke.is_empty());
        assert_eq!(d.choke, vec![2]); // was unchoked, now choked
    }

    #[test]
    fn top_rated_peers_win_the_slots() {
        let mut choker = Choker::new(2);
        // Round 1 is not optimistic; pure rate ranking.
        let peers = [
            peer(1, true, 10, false),
            peer(2, true, 40, false),
            peer(3, true, 30, false),
            peer(4, true, 20, false),
        ];
        let d = choker.plan(&peers);
        let mut got = d.unchoke.clone();
        got.sort_unstable();
        assert_eq!(got, vec![2, 3]); // the two fastest
    }

    #[test]
    fn only_reports_changes() {
        let mut choker = Choker::new(2);
        // Peer 2 already unchoked and still winning: no redundant message.
        let peers = [peer(2, true, 40, true), peer(3, true, 30, false)];
        let d = choker.plan(&peers);
        assert_eq!(d.unchoke, vec![3]);
        assert!(d.choke.is_empty());
    }

    #[test]
    fn optimistic_round_gives_a_choked_peer_a_slot() {
        let mut choker = Choker::new(1);
        // One fast peer would monopolize the single slot; a slow choked
        // peer should get an optimistic turn by round 3.
        let peers = [peer(1, true, 100, false), peer(2, true, 1, false)];
        let mut unchoked_two = false;
        for _ in 0..3 {
            let d = choker.plan(&[peer(1, true, 100, true), peer(2, true, 1, unchoked_two)]);
            if d.unchoke.contains(&2) {
                unchoked_two = true;
            }
        }
        // Peer 1 baseline hidden by the snapshot above; assert peer 2 was
        // eventually unchoked via the optimistic slot.
        assert!(unchoked_two, "slow peer never got an optimistic unchoke");
        let _ = peers;
    }

    /// The rotation must reach every candidate whatever their number. With
    /// three candidates the old `round % len` on rounds 3, 6, 9… only ever
    /// produced index 0.
    #[test]
    fn the_optimistic_slot_visits_every_candidate() {
        for candidates in 1..=12u64 {
            let mut choker = Choker::new(1);
            let mut ever = HashSet::new();
            for _ in 0..(candidates * 6) {
                // Peer 0 is the fixed regular pick; the rest are candidates.
                let peers: Vec<PeerSnapshot> = (0..=candidates)
                    .map(|id| peer(id, true, u64::from(id == 0), id == 0))
                    .collect();
                ever.extend(choker.plan(&peers).unchoke);
            }
            assert_eq!(
                ever.len() as u64,
                candidates,
                "{candidates} candidates: only {} were ever unchoked",
                ever.len()
            );
        }
    }
}
