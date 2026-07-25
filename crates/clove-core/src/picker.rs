//! Piece selection: rarest-first with an endgame phase, plus a per-torrent
//! sequential mode (SCOPE §3).
//!
//! The picker is pure bookkeeping — no I/O, no peer objects. The engine
//! feeds it peer availability and block outcomes; it answers "what should
//! this peer request next". Requests are handed out a whole block at a time
//! and tracked as in-flight so two peers don't redundantly fetch the same
//! block — until the *endgame*, when few blocks remain and duplicate
//! requests (raced, then cancelled) are the accepted cure for the
//! last-block stall.
//!
//! Verification lives in `storage`; the picker only tracks which blocks
//! have arrived. When a piece's blocks are all in, the engine verifies and
//! reports back [`Picker::set_have`] (passed) or [`Picker::reset_piece`]
//! (failed — re-download).

use crate::bitfield::Bitfield;
use crate::wire::{BLOCK_LEN, BlockRequest};

/// Default endgame trigger: once this few blocks remain unreceived across
/// the whole torrent, allow duplicate in-flight requests. Config-tunable
/// later (R5); a constant for now.
pub const DEFAULT_ENDGAME_BLOCKS: u32 = 32;

/// Piece-selection strategy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Rarest-first: fetch the pieces fewest peers have, to keep the swarm
    /// healthy. The default.
    RarestFirst,
    /// Strictly ascending piece order — nice for streaming media, at some
    /// cost to swarm health.
    Sequential,
}

/// Per-piece block accounting, created lazily when a piece is first touched.
struct Progress {
    received: Vec<bool>,
    in_flight: Vec<u32>,
}

impl Progress {
    fn new(blocks: u32) -> Self {
        let n = blocks as usize;
        Progress {
            received: vec![false; n],
            in_flight: vec![0u32; n],
        }
    }

    fn received_count(&self) -> u32 {
        u32::try_from(self.received.iter().filter(|&&r| r).count()).unwrap_or(u32::MAX)
    }
}

/// Tracks availability and download progress for one torrent and chooses
/// the next blocks to request.
pub struct Picker {
    num_pieces: u32,
    piece_length: u64,
    total_length: u64,
    mode: Mode,
    endgame_blocks: u32,
    /// How many connected peers hold each piece.
    availability: Vec<u32>,
    /// Pieces we have fully verified.
    have: Bitfield,
    /// Block state for started-but-incomplete pieces, indexed by piece.
    progress: Vec<Option<Progress>>,
}

impl Picker {
    /// Check every invariant this structure is supposed to maintain.
    ///
    /// Release builds stay lean; debug builds are dense with these checks
    /// (SCOPE §9's paranoid-debug-builds rule), so a bug in the piece
    /// accounting is caught the moment it is introduced rather than surfacing
    /// later as a stalled or corrupt download. Called after every mutating
    /// operation via [`debug_check`](Picker::debug_check).
    ///
    /// # Panics
    ///
    /// If any invariant is violated. That is the point: an inconsistent
    /// picker is a bug, and a bug should not be allowed to survive contact
    /// with a debug build.
    pub fn check_invariants(&self) {
        assert_eq!(
            self.availability.len(),
            self.num_pieces as usize,
            "availability table does not span the torrent"
        );
        assert_eq!(self.have.len(), self.num_pieces, "have field is missized");
        assert_eq!(
            self.progress.len(),
            self.num_pieces as usize,
            "progress table does not span the torrent"
        );

        for index in 0..self.num_pieces {
            let Some(progress) = &self.progress[index as usize] else {
                continue;
            };
            let blocks = self.blocks_in_piece(index) as usize;
            assert_eq!(
                progress.received.len(),
                blocks,
                "piece {index}: received vector disagrees with its block count"
            );
            assert_eq!(
                progress.in_flight.len(),
                blocks,
                "piece {index}: in-flight vector disagrees with its block count"
            );
            // A piece we already have must not still be accumulating blocks:
            // set_have clears its progress, so anything left here means a
            // completed piece was re-entered without being reset.
            assert!(
                !self.have.has(index),
                "piece {index}: held complete yet still has block progress"
            );
        }

        assert_eq!(
            self.is_complete(),
            self.have.count() == self.num_pieces,
            "completion flag disagrees with the have field"
        );
    }

    /// Total in-flight block requests the picker believes are outstanding.
    /// Cross-checked against the peer table in debug builds — the two must
    /// agree, or blocks have leaked (never re-offered) or been double-counted.
    #[must_use]
    pub fn in_flight_total(&self) -> u64 {
        self.progress
            .iter()
            .flatten()
            .flat_map(|p| p.in_flight.iter())
            .map(|&n| u64::from(n))
            .sum()
    }

    /// Run [`check_invariants`](Picker::check_invariants) in debug builds only.
    #[inline]
    fn debug_check(&self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
    }

    /// A picker for a torrent of `num_pieces` pieces.
    #[must_use]
    pub fn new(num_pieces: u32, piece_length: u32, total_length: u64, mode: Mode) -> Self {
        Picker {
            num_pieces,
            piece_length: u64::from(piece_length),
            total_length,
            mode,
            endgame_blocks: DEFAULT_ENDGAME_BLOCKS,
            availability: vec![0u32; num_pieces as usize],
            have: Bitfield::empty(num_pieces),
            progress: (0..num_pieces).map(|_| None).collect(),
        }
    }

    /// The piece-selection mode in force.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switch piece-selection mode.
    ///
    /// Only the *next* pick is affected: blocks already requested stay
    /// requested, so a torrent switched to sequential mid-download finishes
    /// its outstanding pieces before walking forward in order. That is the
    /// intended behaviour — cancelling in-flight work to obey a preference
    /// wastes exactly the bandwidth the preference is trying to spend well.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Override the endgame threshold (blocks remaining). Zero disables
    /// endgame entirely.
    pub fn set_endgame_blocks(&mut self, blocks: u32) {
        self.endgame_blocks = blocks;
    }

    /// Length of piece `index` (final piece may be short).
    #[must_use]
    pub fn piece_len(&self, index: u32) -> u32 {
        let start = u64::from(index) * self.piece_length;
        if start >= self.total_length {
            return 0;
        }
        u32::try_from((self.total_length - start).min(self.piece_length)).unwrap_or(u32::MAX)
    }

    /// Number of blocks in piece `index`.
    #[must_use]
    pub fn blocks_in_piece(&self, index: u32) -> u32 {
        self.piece_len(index).div_ceil(BLOCK_LEN)
    }

    /// Byte length of block `block` within piece `index`.
    #[must_use]
    pub fn block_len(&self, index: u32, block: u32) -> u32 {
        let plen = self.piece_len(index);
        let begin = block.saturating_mul(BLOCK_LEN);
        if begin >= plen {
            return 0;
        }
        (plen - begin).min(BLOCK_LEN)
    }

    /// Whether we have verified piece `index`.
    #[must_use]
    pub fn has(&self, index: u32) -> bool {
        self.have.has(index)
    }

    /// Our verified-piece set, e.g. to send as a bitfield.
    #[must_use]
    pub fn have_field(&self) -> &Bitfield {
        &self.have
    }

    /// Whether every piece is verified.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.have.count() == self.num_pieces
    }

    /// Record that a peer holds every piece in `field` (its bitfield, or a
    /// have-all). Call once per peer when it announces its set.
    pub fn add_bitfield(&mut self, field: &Bitfield) {
        for index in 0..self.num_pieces {
            if field.has(index) {
                self.availability[index as usize] += 1;
            }
        }
        self.debug_check();
    }

    /// Undo [`add_bitfield`] when a peer disconnects.
    pub fn remove_bitfield(&mut self, field: &Bitfield) {
        for index in 0..self.num_pieces {
            if field.has(index) {
                let a = &mut self.availability[index as usize];
                *a = a.saturating_sub(1);
            }
        }
        self.debug_check();
    }

    /// Record a single new piece a peer announced via `have`.
    pub fn add_single(&mut self, index: u32) {
        if index < self.num_pieces {
            self.availability[index as usize] += 1;
        }
        self.debug_check();
    }

    /// Availability of piece `index` (how many peers hold it).
    #[must_use]
    pub fn availability(&self, index: u32) -> u32 {
        self.availability.get(index as usize).copied().unwrap_or(0)
    }

    /// Mark piece `index` verified. Clears its block progress.
    pub fn set_have(&mut self, index: u32) {
        if index < self.num_pieces {
            self.have.set(index);
            self.progress[index as usize] = None;
        }
        self.debug_check();
    }

    /// Discard a piece's downloaded blocks after a failed verification, so
    /// it is re-downloaded from scratch.
    pub fn reset_piece(&mut self, index: u32) {
        if index < self.num_pieces {
            self.progress[index as usize] = None;
        }
        self.debug_check();
    }

    /// Record that block `block` of piece `index` arrived. Returns `true`
    /// when that completes the piece (all blocks received) and the engine
    /// should verify it.
    pub fn block_received(&mut self, index: u32, block: u32) -> bool {
        let blocks = self.blocks_in_piece(index);
        let Some(prog) = self.progress_mut(index, blocks) else {
            return false;
        };
        let Some(slot) = prog.received.get_mut(block as usize) else {
            return false;
        };
        // The caller only reports blocks it had in flight, so the request is
        // settled either way. Decrementing only on the first arrival would
        // leak a phantom in-flight count for every duplicate endgame
        // delivery, leaving the picker believing a block is owed that nobody
        // will send.
        let inflight = &mut prog.in_flight[block as usize];
        *inflight = inflight.saturating_sub(1);
        *slot = true;
        let complete = prog.received_count() == blocks;
        self.debug_check();
        complete
    }

    /// Release an in-flight block that will not arrive (timeout, reject, or
    /// the peer disconnected), so it can be handed out again.
    pub fn block_failed(&mut self, index: u32, block: u32) {
        if let Some(Some(prog)) = self.progress.get_mut(index as usize)
            && let Some(inflight) = prog.in_flight.get_mut(block as usize)
        {
            *inflight = inflight.saturating_sub(1);
        }
        self.debug_check();
    }

    /// Choose up to `want` blocks for a peer whose piece set is `peer_has`,
    /// marking them in-flight. Prefers finishing already-started pieces,
    /// then opens new ones in the configured order.
    pub fn pick(&mut self, peer_has: &Bitfield, want: usize) -> Vec<BlockRequest> {
        let mut out = Vec::new();
        if want == 0 {
            return out;
        }
        let endgame = self.in_endgame();
        for index in self.candidate_order(peer_has) {
            if out.len() >= want {
                break;
            }
            let blocks = self.blocks_in_piece(index);
            let plen = self.piece_len(index);
            let Some(prog) = self.progress_mut(index, blocks) else {
                continue;
            };
            for block in 0..blocks {
                if out.len() >= want {
                    break;
                }
                let b = block as usize;
                if prog.received[b] {
                    continue;
                }
                if !endgame && prog.in_flight[b] > 0 {
                    continue;
                }
                prog.in_flight[b] += 1;
                let begin = block * BLOCK_LEN;
                out.push(BlockRequest {
                    index,
                    begin,
                    length: (plen - begin).min(BLOCK_LEN),
                });
            }
        }
        self.debug_check();
        out
    }

    fn in_endgame(&self) -> bool {
        if self.endgame_blocks == 0 {
            return false;
        }
        let mut remaining = 0u32;
        for index in 0..self.num_pieces {
            if self.have.has(index) {
                continue;
            }
            let received = match &self.progress[index as usize] {
                Some(p) => p.received_count(),
                None => 0,
            };
            remaining = remaining.saturating_add(self.blocks_in_piece(index) - received);
            if remaining > self.endgame_blocks {
                return false;
            }
        }
        true
    }

    /// Pieces this peer can serve that we still need, ordered: started
    /// pieces first (finish what we've begun), then unstarted, each group in
    /// the configured order (index, or rarest-first).
    fn candidate_order(&self, peer_has: &Bitfield) -> Vec<u32> {
        let mut candidates: Vec<u32> = (0..self.num_pieces)
            .filter(|&i| peer_has.has(i) && !self.have.has(i))
            .collect();
        candidates.sort_by_key(|&i| {
            let started = self.progress[i as usize].is_some();
            let group = u8::from(!started); // started (0) before unstarted (1)
            let rarity = match self.mode {
                Mode::RarestFirst => self.availability[i as usize],
                Mode::Sequential => 0,
            };
            (group, rarity, i)
        });
        candidates
    }

    fn progress_mut(&mut self, index: u32, blocks: u32) -> Option<&mut Progress> {
        let slot = self.progress.get_mut(index as usize)?;
        Some(slot.get_or_insert_with(|| Progress::new(blocks)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Torrent geometry helper: `pieces` pieces of one block each unless a
    // custom total is given.
    fn one_block_pieces(n: u32) -> Picker {
        // piece_length = BLOCK_LEN so each piece is exactly one block.
        Picker::new(
            n,
            BLOCK_LEN,
            u64::from(n) * u64::from(BLOCK_LEN),
            Mode::RarestFirst,
        )
    }

    fn field(len: u32, present: &[u32]) -> Bitfield {
        let mut bf = Bitfield::empty(len);
        for &p in present {
            bf.set(p);
        }
        bf
    }

    #[test]
    fn block_geometry_last_piece_short() {
        // 3 pieces, piece_length 3*BLOCK, total = 2 full pieces + 100 bytes.
        let total = 2 * u64::from(BLOCK_LEN) * 3 + 100;
        let p = Picker::new(3, 3 * BLOCK_LEN, total, Mode::RarestFirst);
        assert_eq!(p.blocks_in_piece(0), 3);
        assert_eq!(p.piece_len(2), 100);
        assert_eq!(p.blocks_in_piece(2), 1);
        assert_eq!(p.block_len(2, 0), 100);
        assert_eq!(p.block_len(0, 2), BLOCK_LEN);
    }

    #[test]
    fn rarest_first_orders_by_availability() {
        let mut p = one_block_pieces(4);
        // Peer A has all; make piece 2 rarest, piece 0 most common.
        p.add_bitfield(&field(4, &[0, 1, 2, 3])); // all +1
        p.add_bitfield(&field(4, &[0, 1, 3])); // 2 stays rarest
        p.add_bitfield(&field(4, &[0, 3])); // 0 and 3 most common
        let peer = field(4, &[0, 1, 2, 3]);
        let picks = p.pick(&peer, 4);
        let order: Vec<u32> = picks.iter().map(|b| b.index).collect();
        assert_eq!(order, vec![2, 1, 0, 3]); // ascending availability, tie by index
    }

    #[test]
    fn sequential_orders_by_index() {
        let mut p = Picker::new(4, BLOCK_LEN, 4 * u64::from(BLOCK_LEN), Mode::Sequential);
        p.add_bitfield(&field(4, &[0, 1, 2, 3]));
        p.add_bitfield(&field(4, &[3])); // make 3 most common; sequential ignores it
        let picks = p.pick(&field(4, &[0, 1, 2, 3]), 4);
        let order: Vec<u32> = picks.iter().map(|b| b.index).collect();
        assert_eq!(order, vec![0, 1, 2, 3]);
    }

    #[test]
    fn switching_mode_changes_the_next_pick_only() {
        let mut p = Picker::new(4, BLOCK_LEN, 4 * u64::from(BLOCK_LEN), Mode::RarestFirst);
        // Endgame would hand the same block out twice and obscure the point.
        p.set_endgame_blocks(0);
        let peer = field(4, &[0, 1, 2, 3]);
        p.add_bitfield(&peer);
        p.add_bitfield(&field(4, &[0, 1, 2])); // piece 3 is now rarest
        assert_eq!(p.pick(&peer, 1)[0].index, 3);
        assert_eq!(p.mode(), Mode::RarestFirst);

        p.set_mode(Mode::Sequential);
        assert_eq!(p.mode(), Mode::Sequential);
        // Piece 3 stays in flight — the switch does not cancel work — and the
        // next pick walks from the front regardless of rarity.
        assert_eq!(p.pick(&peer, 1)[0].index, 0);
    }

    #[test]
    fn does_not_double_request_outside_endgame() {
        let mut p = one_block_pieces(3);
        p.set_endgame_blocks(0); // disable endgame
        let peer = field(3, &[0, 1, 2]);
        let first = p.pick(&peer, 3);
        assert_eq!(first.len(), 3);
        // Everything is in-flight now; a second peer gets nothing new.
        assert!(p.pick(&peer, 3).is_empty());
        // Release one; it becomes available again.
        p.block_failed(1, 0);
        let again = p.pick(&peer, 3);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].index, 1);
    }

    #[test]
    fn endgame_allows_duplicate_requests() {
        let mut p = one_block_pieces(2); // 2 blocks total <= default threshold
        assert!(p.in_endgame());
        let peer = field(2, &[0, 1]);
        let a = p.pick(&peer, 2);
        assert_eq!(a.len(), 2);
        // In endgame the same blocks can be handed to a second peer.
        let b = p.pick(&peer, 2);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn prefers_finishing_started_pieces() {
        // Two pieces of two blocks each; start piece 1, then a fresh pick
        // should continue piece 1 before opening piece 0.
        let total = 2 * 2 * u64::from(BLOCK_LEN);
        let mut p = Picker::new(2, 2 * BLOCK_LEN, total, Mode::RarestFirst);
        p.set_endgame_blocks(0);
        p.add_bitfield(&field(2, &[0, 1]));
        // Force piece 1 to look started by taking one of its blocks first.
        // With equal availability and tie-by-index, index 0 sorts first, so
        // request one block, fail it on piece 0, and manually start piece 1.
        let _ = p.pick(&field(2, &[1]), 1); // peer only has piece 1 -> starts it
        let picks = p.pick(&field(2, &[0, 1]), 1);
        assert_eq!(picks[0].index, 1, "should finish started piece 1 first");
    }

    #[test]
    fn full_download_lifecycle() {
        let mut p = one_block_pieces(3);
        let peer = field(3, &[0, 1, 2]);
        let picks = p.pick(&peer, 10);
        assert_eq!(picks.len(), 3);
        for (i, b) in picks.iter().enumerate() {
            let complete = p.block_received(b.index, b.begin / BLOCK_LEN);
            assert!(complete, "one-block piece completes on its block");
            p.set_have(b.index);
            assert!(p.has(b.index), "piece {i}");
        }
        assert!(p.is_complete());
        // Nothing left to pick.
        assert!(p.pick(&peer, 10).is_empty());
    }

    #[test]
    fn failed_verification_redownloads() {
        let mut p = one_block_pieces(1);
        let peer = field(1, &[0]);
        let b = p.pick(&peer, 1);
        assert!(p.block_received(0, 0));
        // Verification failed: reset and it must be re-pickable.
        p.reset_piece(0);
        assert!(!p.has(0));
        let again = p.pick(&peer, 1);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0], b[0]);
    }

    #[test]
    fn availability_removed_on_peer_leave() {
        let mut p = one_block_pieces(2);
        let f = field(2, &[0, 1]);
        p.add_bitfield(&f);
        assert_eq!(p.availability(0), 1);
        p.remove_bitfield(&f);
        assert_eq!(p.availability(0), 0);
        // Never underflows.
        p.remove_bitfield(&f);
        assert_eq!(p.availability(0), 0);
    }
}
