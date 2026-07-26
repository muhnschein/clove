//! Model-based sweep over the pure state machines (`docs/SCOPE.md` §9).
//!
//! `hostile.rs` mutates *bytes* and requires the parsers to survive them. This
//! does the same job for *state*: it drives [`Picker`] and [`Choker`] through
//! long random sequences of legal operations and, after every single one, checks
//! the real thing against a model of what it should be.
//!
//! The model tracks state, not policy. It does not decide which piece is
//! rarest or whose turn the optimistic slot is — reimplementing that would
//! just be the implementation twice, brittle against any change of mind about
//! strategy, and would prove nothing. What it tracks is what must be true
//! whatever the strategy: which pieces are held, how many peers hold each one,
//! which blocks are outstanding, and whether there is work left to hand out.
//! The piece geometry is duplicated because it is three lines of arithmetic and
//! every interesting property needs it.
//!
//! That is where the bugs were. Three of the findings in
//! `docs/CODE-REVIEW-2026-07.md` — a duplicate block reopening a finished
//! piece, availability inflated past what the peers justify, a request counted
//! as outstanding that nobody owes — are all "the bookkeeping drifted from
//! reality", and none of them needed a network to reproduce. Two of them
//! survived a green test suite because nothing drove the picker into the state
//! that exposed them.
//!
//! Beyond agreement, the sweeps assert *liveness*: that a picker with work
//! available always offers some, and that every interested peer eventually gets
//! a turn. A picker that quietly stops handing out blocks, or a choker whose
//! optimistic slot freezes, violates no invariant at all — nothing is
//! inconsistent, the download simply never finishes. `check_invariants` cannot
//! see that; only driving the thing over many steps can.
//!
//! Deterministic: every failure names its seed, and re-running that seed
//! reproduces the exact sequence. That matters more than it sounds — the
//! sabotage runs in the review's testing section identify failures by seed and
//! step number, and those are stable.

// Fixtures and helpers sit outside `#[test]` functions, where clippy's
// allow-expect-in-tests does not reach. Each `expect` here names an invariant of
// the harness itself; a broken fixture is a broken test, not a runtime error.
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use clove_core::bitfield::Bitfield;
use clove_core::choker::{Choker, PeerSnapshot};
use clove_core::picker::{Mode, Picker};
use clove_core::wire::BLOCK_LEN;

/// Operations per sweep. Long enough to reach deep states (pieces completed,
/// peers gone, endgame entered and left), short enough that the whole file
/// stays well under a second in a debug build — it runs on every push.
const STEPS: usize = 4_000;

/// xorshift64*, so a failure reproduces exactly from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in.max(1)) == 0
    }
}

// ------------------------------------------------------------- picker model

/// What the picker's state ought to be, tracked independently.
struct Model {
    num_pieces: u32,
    piece_length: u64,
    total_length: u64,
    endgame_blocks: u32,
    /// Pieces reported verified.
    have: BTreeSet<u32>,
    /// Per-piece block state, present only for pieces with accounting open:
    /// `(received, in_flight)` per block, mirroring `Picker`'s `Progress`.
    progress: BTreeMap<u32, Vec<(bool, u32)>>,
    /// Connected peers and what each of them holds. Availability is derived
    /// from this rather than counted separately, which is the whole point: a
    /// count that cannot be justified by a peer is the bug.
    peers: BTreeMap<u64, Bitfield>,
    next_peer: u64,
}

impl Model {
    fn new(num_pieces: u32, blocks_per_piece: u32, tail: u64) -> Model {
        let piece_length = u64::from(blocks_per_piece) * u64::from(BLOCK_LEN);
        let total_length = u64::from(num_pieces - 1) * piece_length + tail;
        Model {
            num_pieces,
            piece_length,
            total_length,
            endgame_blocks: clove_core::picker::DEFAULT_ENDGAME_BLOCKS,
            have: BTreeSet::new(),
            progress: BTreeMap::new(),
            peers: BTreeMap::new(),
            next_peer: 0,
        }
    }

    fn picker(&self, mode: Mode) -> Picker {
        Picker::new(
            self.num_pieces,
            u32::try_from(self.piece_length).expect("fixture piece length fits"),
            self.total_length,
            mode,
        )
    }

    fn piece_len(&self, index: u32) -> u32 {
        let start = u64::from(index) * self.piece_length;
        if start >= self.total_length {
            return 0;
        }
        u32::try_from((self.total_length - start).min(self.piece_length)).unwrap_or(u32::MAX)
    }

    fn blocks(&self, index: u32) -> u32 {
        self.piece_len(index).div_ceil(BLOCK_LEN)
    }

    fn block_len(&self, index: u32, block: u32) -> u32 {
        let plen = self.piece_len(index);
        let begin = block.saturating_mul(BLOCK_LEN);
        if begin >= plen {
            return 0;
        }
        (plen - begin).min(BLOCK_LEN)
    }

    fn received(&self, index: u32, block: u32) -> bool {
        self.progress
            .get(&index)
            .and_then(|blocks| blocks.get(block as usize))
            .is_some_and(|&(received, _)| received)
    }

    fn in_flight(&self, index: u32, block: u32) -> u32 {
        self.progress
            .get(&index)
            .and_then(|blocks| blocks.get(block as usize))
            .map_or(0, |&(_, count)| count)
    }

    fn received_count(&self, index: u32) -> u32 {
        self.progress.get(&index).map_or(0, |blocks| {
            u32::try_from(blocks.iter().filter(|&&(received, _)| received).count())
                .unwrap_or(u32::MAX)
        })
    }

    fn in_flight_total(&self) -> u64 {
        self.progress
            .values()
            .flat_map(|blocks| blocks.iter())
            .map(|&(_, count)| u64::from(count))
            .sum()
    }

    fn availability(&self, index: u32) -> u32 {
        u32::try_from(self.peers.values().filter(|held| held.has(index)).count())
            .unwrap_or(u32::MAX)
    }

    fn is_complete(&self) -> bool {
        u32::try_from(self.have.len()).unwrap_or(u32::MAX) == self.num_pieces
    }

    /// Blocks nobody has delivered yet, across the whole torrent — the quantity
    /// the endgame threshold is measured against.
    fn remaining_blocks(&self) -> u32 {
        (0..self.num_pieces)
            .filter(|index| !self.have.contains(index))
            .map(|index| self.blocks(index) - self.received_count(index))
            .sum()
    }

    fn in_endgame(&self) -> bool {
        self.endgame_blocks != 0 && self.remaining_blocks() <= self.endgame_blocks
    }

    /// Whether `peer` has any block the picker could legally offer it.
    ///
    /// The liveness question: if this is true and the picker returns nothing,
    /// the download has stalled with work available — which is what a leaked
    /// in-flight count looks like from the outside.
    fn work_for(&self, peer: &Bitfield) -> bool {
        let endgame = self.in_endgame();
        (0..self.num_pieces)
            .filter(|index| peer.has(*index) && !self.have.contains(index))
            .any(|index| {
                (0..self.blocks(index)).any(|block| {
                    !self.received(index, block) && (endgame || self.in_flight(index, block) == 0)
                })
            })
    }

    /// Open block accounting for a piece, as `Picker::progress_mut` does —
    /// including the part that matters: never for a piece already held.
    fn open(&mut self, index: u32) -> bool {
        if self.have.contains(&index) {
            return false;
        }
        if index >= self.num_pieces {
            return false;
        }
        let blocks = self.blocks(index) as usize;
        self.progress
            .entry(index)
            .or_insert_with(|| vec![(false, 0); blocks]);
        true
    }

    /// Everything observable about the picker must match, after every step.
    fn check(&self, real: &Picker, seed: u64, step: usize) {
        let where_ = || format!("seed {seed:#x} step {step}");
        // Its own net first: an inconsistent picker is a bug whether or not the
        // model happens to notice.
        real.check_invariants();

        assert_eq!(
            real.is_complete(),
            self.is_complete(),
            "{}: completion disagrees",
            where_()
        );
        assert_eq!(
            real.in_flight_total(),
            self.in_flight_total(),
            "{}: {} blocks outstanding, model says {}",
            where_(),
            real.in_flight_total(),
            self.in_flight_total()
        );
        for index in 0..self.num_pieces {
            assert_eq!(
                real.has(index),
                self.have.contains(&index),
                "{}: piece {index} held-ness disagrees",
                where_()
            );
            assert_eq!(
                real.availability(index),
                self.availability(index),
                "{}: piece {index} availability {} but {} peers hold it",
                where_(),
                real.availability(index),
                self.availability(index)
            );
            assert_eq!(
                real.piece_len(index),
                self.piece_len(index),
                "{}: piece {index} length disagrees",
                where_()
            );
            assert_eq!(
                real.blocks_in_piece(index),
                self.blocks(index),
                "{}: piece {index} block count disagrees",
                where_()
            );
        }
        assert_eq!(
            real.have_field().count(),
            u32::try_from(self.have.len()).unwrap_or(u32::MAX),
            "{}: have-field count disagrees",
            where_()
        );
    }
}

/// Every `(piece, block)` the model believes is in flight somewhere.
fn outstanding(model: &Model) -> Vec<(u32, u32)> {
    model
        .progress
        .iter()
        .flat_map(|(index, blocks)| {
            blocks
                .iter()
                .enumerate()
                .filter(|&(_, &(_, count))| count > 0)
                .map(move |(block, _)| (*index, u32::try_from(block).unwrap_or(u32::MAX)))
        })
        .collect()
}

/// A random piece set for a joining peer: everything, nothing, or a mixture.
fn random_field(rng: &mut Rng, num_pieces: u32) -> Bitfield {
    match rng.below(4) {
        0 => Bitfield::full(num_pieces),
        1 => Bitfield::empty(num_pieces),
        _ => {
            let mut field = Bitfield::empty(num_pieces);
            for index in 0..num_pieces {
                if rng.chance(2) {
                    field.set(index);
                }
            }
            field
        }
    }
}

/// Torrent shape for a sweep: `(pieces, blocks per piece, last-piece bytes)`.
///
/// Both shapes end in a short piece, because a partial final block is where the
/// length arithmetic gets its chance to be wrong.
type Geometry = (u32, u32, u64);

/// Small enough that the whole torrent is inside the default endgame threshold
/// from the first step: duplicate hand-outs are legal throughout.
const SMALL: Geometry = (6, 3, 500);

/// Forty-eight blocks, so the default threshold of 32 is crossed part-way
/// through and the endgame is a transition rather than a constant. Without this
/// shape the "not handed out twice outside the endgame" rule is never checked on
/// a default-configured picker.
const LARGER: Geometry = (12, 4, 500);

/// Drive one sweep: `STEPS` legal operations, checked against the model after
/// each one.
#[allow(clippy::too_many_lines)] // one operation table; splitting hides the shape
fn sweep(seed: u64, mode: Mode, endgame: Option<u32>, geometry: Geometry) {
    let (pieces, blocks_per_piece, tail) = geometry;
    let mut rng = Rng(seed);
    let mut model = Model::new(pieces, blocks_per_piece, tail);
    let mut picker = model.picker(mode);
    if let Some(blocks) = endgame {
        picker.set_endgame_blocks(blocks);
        model.endgame_blocks = blocks;
    }
    model.check(&picker, seed, 0);

    for step in 1..=STEPS {
        match rng.below(10) {
            // A peer arrives with a piece set.
            0 => {
                let field = random_field(&mut rng, model.num_pieces);
                picker.add_bitfield(&field);
                let id = model.next_peer;
                model.next_peer += 1;
                model.peers.insert(id, field);
            }
            // A peer leaves; its availability goes with it.
            1 if !model.peers.is_empty() => {
                let ids: Vec<u64> = model.peers.keys().copied().collect();
                let id = ids[rng.below(ids.len())];
                let field = model.peers.remove(&id).expect("peer was listed");
                picker.remove_bitfield(&field);
            }
            // A peer announces one more piece. Counted only when the bit
            // actually changes, which is how the engine uses `add_single`.
            2 if !model.peers.is_empty() => {
                let ids: Vec<u64> = model.peers.keys().copied().collect();
                let id = ids[rng.below(ids.len())];
                let index = u32::try_from(rng.below(model.num_pieces as usize)).unwrap_or(0);
                let field = model.peers.get_mut(&id).expect("peer was listed");
                if !field.has(index) {
                    field.set(index);
                    picker.add_single(index);
                }
            }
            // Hand blocks to a peer.
            3..=5 if !model.peers.is_empty() => {
                let ids: Vec<u64> = model.peers.keys().copied().collect();
                let id = ids[rng.below(ids.len())];
                let field = model.peers[&id].clone();
                let want = rng.below(5);
                let endgame_now = model.in_endgame();
                let expect_work = want > 0 && model.work_for(&field);

                let offered = picker.pick(&field, want);

                assert!(
                    offered.len() <= want,
                    "seed {seed:#x} step {step}: asked for {want} blocks, got {}",
                    offered.len()
                );
                assert!(
                    !expect_work || !offered.is_empty(),
                    "seed {seed:#x} step {step}: work was available but nothing was offered \
                     (a stalled download looks exactly like this)"
                );
                let mut handed_out = BTreeSet::new();
                for request in &offered {
                    let block = request.begin / BLOCK_LEN;
                    assert!(
                        handed_out.insert((request.index, block)),
                        "seed {seed:#x} step {step}: piece {} block {block} offered twice in \
                         one pick",
                        request.index
                    );
                    assert!(
                        field.has(request.index),
                        "seed {seed:#x} step {step}: offered piece {} the peer does not hold",
                        request.index
                    );
                    assert!(
                        !model.have.contains(&request.index),
                        "seed {seed:#x} step {step}: offered piece {} we already hold",
                        request.index
                    );
                    assert!(
                        !model.received(request.index, block),
                        "seed {seed:#x} step {step}: offered piece {} block {block}, already \
                         received",
                        request.index
                    );
                    assert!(
                        request.begin.is_multiple_of(BLOCK_LEN),
                        "seed {seed:#x} step {step}: unaligned offset {}",
                        request.begin
                    );
                    assert_eq!(
                        request.length,
                        model.block_len(request.index, block),
                        "seed {seed:#x} step {step}: piece {} block {block} wrong length",
                        request.index
                    );
                    assert!(request.length > 0 && request.length <= BLOCK_LEN);
                    if !endgame_now {
                        assert_eq!(
                            model.in_flight(request.index, block),
                            0,
                            "seed {seed:#x} step {step}: piece {} block {block} handed out \
                             twice outside the endgame",
                            request.index
                        );
                    }
                }
                // Apply: every offered block is now outstanding.
                for request in &offered {
                    let block = request.begin / BLOCK_LEN;
                    assert!(
                        model.open(request.index),
                        "offered a piece it would not open"
                    );
                    let blocks = model
                        .progress
                        .get_mut(&request.index)
                        .expect("just opened it");
                    blocks[block as usize].1 += 1;
                }
            }
            // A block arrives. Usually one that was asked for; sometimes not,
            // which is the late-duplicate and unsolicited case.
            6 | 7 => {
                let outstanding = outstanding(&model);
                let (index, block) = if outstanding.is_empty() || rng.chance(6) {
                    (
                        u32::try_from(rng.below(model.num_pieces as usize + 1)).unwrap_or(0),
                        u32::try_from(rng.below(4)).unwrap_or(0),
                    )
                } else {
                    outstanding[rng.below(outstanding.len())]
                };

                // What the model says should happen, before it happens.
                let expected = if model.have.contains(&index) || index >= model.num_pieces {
                    None
                } else {
                    let blocks = model.blocks(index);
                    (block < blocks).then(|| {
                        let mut count = model.received_count(index);
                        if !model.received(index, block) {
                            count += 1;
                        }
                        count == blocks
                    })
                };
                let complete = picker.block_received(index, block);
                assert_eq!(
                    complete,
                    expected.unwrap_or(false),
                    "seed {seed:#x} step {step}: piece {index} block {block} completion \
                     disagrees"
                );
                // Apply. Note the order the real one uses: accounting is opened
                // before the block index is range-checked.
                if model.open(index) {
                    let blocks = model.progress.get_mut(&index).expect("just opened it");
                    if let Some(slot) = blocks.get_mut(block as usize) {
                        slot.1 = slot.1.saturating_sub(1);
                        slot.0 = true;
                    }
                }
            }
            // A block will not arrive: timeout, reject, or the peer went away.
            8 => {
                let outstanding = outstanding(&model);
                let (index, block) = if outstanding.is_empty() {
                    (
                        u32::try_from(rng.below(model.num_pieces as usize + 1)).unwrap_or(0),
                        u32::try_from(rng.below(4)).unwrap_or(0),
                    )
                } else {
                    outstanding[rng.below(outstanding.len())]
                };
                picker.block_failed(index, block);
                // Unlike a delivery, this opens nothing.
                if let Some(blocks) = model.progress.get_mut(&index)
                    && let Some(slot) = blocks.get_mut(block as usize)
                {
                    slot.1 = slot.1.saturating_sub(1);
                }
            }
            // Verification: a fully-received piece passes, or it fails and goes
            // back for re-download. Occasionally on a piece that is not ready,
            // because nothing in the picker's API says it must be.
            _ => {
                let candidates: Vec<u32> = (0..model.num_pieces)
                    .filter(|index| {
                        !model.have.contains(index)
                            && model.received_count(*index) == model.blocks(*index)
                            && model.progress.contains_key(index)
                    })
                    .collect();
                let index = if candidates.is_empty() || rng.chance(8) {
                    u32::try_from(rng.below(model.num_pieces as usize + 1)).unwrap_or(0)
                } else {
                    candidates[rng.below(candidates.len())]
                };
                if rng.chance(4) {
                    picker.reset_piece(index);
                    if index < model.num_pieces {
                        model.progress.remove(&index);
                        model.have.remove(&index);
                    }
                } else {
                    picker.set_have(index);
                    if index < model.num_pieces {
                        model.have.insert(index);
                        model.progress.remove(&index);
                    }
                }
            }
        }
        model.check(&picker, seed, step);
    }
}

#[test]
fn the_picker_matches_its_model_under_random_operations() {
    // Both selection orders, both torrent shapes, and three endgame settings:
    // the default threshold, disabled outright (so a duplicate hand-out is
    // always a bug), and a narrow window entered near the end. Every
    // combination, so no regime depends on a seed happening to reach it.
    let mut seed = 0x5EED_0001_u64;
    for mode in [Mode::RarestFirst, Mode::Sequential] {
        for geometry in [SMALL, LARGER] {
            for endgame in [None, Some(0), Some(4)] {
                sweep(seed, mode, endgame, geometry);
                seed += 1;
            }
        }
    }
}

/// The liveness statement the accounting exists to support: with a peer that
/// has everything and every delivery honoured, a torrent finishes. A leaked
/// in-flight count — a block the picker believes is owed by nobody — shows up
/// here as a download that stops short, which is exactly how it presents in the
/// field.
#[test]
fn a_torrent_always_drains_to_completion() {
    for seed in [0xD4A1_0001_u64, 0xD4A1_0002, 0xD4A1_0003] {
        let mut rng = Rng(seed);
        let mut model = Model::new(5, 4, 1);
        let mut picker = model.picker(Mode::RarestFirst);

        // One peer that holds everything, plus churn: peers that come and go
        // while owing blocks, which is what returns those blocks to the pool.
        let everything = Bitfield::full(model.num_pieces);
        picker.add_bitfield(&everything);
        model.peers.insert(0, everything.clone());
        model.next_peer = 1;

        let total_blocks: u32 = (0..model.num_pieces).map(|index| model.blocks(index)).sum();
        let budget = 200 * total_blocks as usize;
        let mut steps = 0usize;

        while !model.is_complete() {
            steps += 1;
            assert!(
                steps < budget,
                "seed {seed:#x}: stalled at {} of {} pieces after {steps} steps",
                model.have.len(),
                model.num_pieces
            );

            // Churn, sometimes.
            if rng.chance(20) {
                let field = random_field(&mut rng, model.num_pieces);
                picker.add_bitfield(&field);
                let id = model.next_peer;
                model.next_peer += 1;
                model.peers.insert(id, field);
            }
            if rng.chance(25) && model.peers.len() > 1 {
                let ids: Vec<u64> = model.peers.keys().copied().filter(|&id| id != 0).collect();
                if !ids.is_empty() {
                    let id = ids[rng.below(ids.len())];
                    let field = model.peers.remove(&id).expect("peer was listed");
                    picker.remove_bitfield(&field);
                }
            }

            let offered = picker.pick(&everything, 4);
            if offered.is_empty() {
                // Nothing outstanding to deliver either: that would be a stall.
                assert!(
                    model.in_flight_total() > 0 || !model.work_for(&everything),
                    "seed {seed:#x}: nothing offered and nothing outstanding, with work left"
                );
            }
            for request in &offered {
                let block = request.begin / BLOCK_LEN;
                model.open(request.index);
                model
                    .progress
                    .get_mut(&request.index)
                    .expect("just opened it")[block as usize]
                    .1 += 1;

                // Most blocks arrive; a few are dropped and must come back.
                if rng.chance(8) {
                    picker.block_failed(request.index, block);
                    if let Some(blocks) = model.progress.get_mut(&request.index) {
                        blocks[block as usize].1 = blocks[block as usize].1.saturating_sub(1);
                    }
                    continue;
                }
                let complete = picker.block_received(request.index, block);
                if let Some(blocks) = model.progress.get_mut(&request.index) {
                    blocks[block as usize].1 = blocks[block as usize].1.saturating_sub(1);
                    blocks[block as usize].0 = true;
                }
                if complete {
                    // Verification passes most of the time; when it fails the
                    // piece must become available again, or the torrent can
                    // never finish.
                    if rng.chance(10) {
                        picker.reset_piece(request.index);
                        model.progress.remove(&request.index);
                        model.have.remove(&request.index);
                    } else {
                        picker.set_have(request.index);
                        model.have.insert(request.index);
                        model.progress.remove(&request.index);
                    }
                }
            }
            model.check(&picker, seed, steps);
        }

        assert!(picker.is_complete());
        assert_eq!(
            picker.in_flight_total(),
            0,
            "seed {seed:#x}: finished with blocks still counted as outstanding"
        );
    }
}

// -------------------------------------------------------------- choker model

/// A choke plan must be applicable, and the result must respect the slots.
///
/// Applied round after round with the previous round's state fed back, which is
/// the only way the "only reports changes" and slot-count rules can be checked
/// at all: both are statements about the transition, not the snapshot.
#[test]
fn choke_plans_are_always_applicable() {
    for seed in [0xC40C_0001_u64, 0xC40C_0002, 0xC40C_0003] {
        for slots in [1usize, 2, 4, 8] {
            let mut rng = Rng(seed ^ (slots as u64) << 32);
            let mut choker = Choker::new(slots);
            let mut peers: BTreeMap<u64, (bool, u64, bool)> = BTreeMap::new(); // interested, rate, unchoked
            let mut next_id = 0u64;

            for round in 1..=400 {
                // Churn: peers join, leave, change their mind, transfer.
                if rng.chance(3) || peers.is_empty() {
                    peers.insert(next_id, (rng.chance(2), rng.next() % 1000, false));
                    next_id += 1;
                }
                if rng.chance(6) && !peers.is_empty() {
                    let ids: Vec<u64> = peers.keys().copied().collect();
                    peers.remove(&ids[rng.below(ids.len())]);
                }
                for state in peers.values_mut() {
                    if rng.chance(8) {
                        state.0 = !state.0;
                    }
                    if rng.chance(4) {
                        state.1 = rng.next() % 1000;
                    }
                }

                let snapshots: Vec<PeerSnapshot> = peers
                    .iter()
                    .map(|(&id, &(interested, rate, unchoked))| PeerSnapshot {
                        id,
                        interested,
                        rate,
                        unchoked,
                    })
                    .collect();
                let decision = choker.plan(&snapshots);
                let at = || format!("seed {seed:#x} slots {slots} round {round}");

                for id in &decision.unchoke {
                    assert!(
                        peers.contains_key(id),
                        "{}: told to unchoke {id}, which is not connected",
                        at()
                    );
                    assert!(
                        !decision.choke.contains(id),
                        "{}: {id} told to choke and unchoke at once",
                        at()
                    );
                    let (interested, _, unchoked) = peers[id];
                    assert!(
                        interested,
                        "{}: unchoked {id}, which is not interested",
                        at()
                    );
                    assert!(!unchoked, "{}: unchoked {id}, which already was", at());
                }
                for id in &decision.choke {
                    assert!(
                        peers.contains_key(id),
                        "{}: told to choke {id}, which is not connected",
                        at()
                    );
                    let (_, _, unchoked) = peers[id];
                    assert!(unchoked, "{}: choked {id}, which already was", at());
                }

                // Apply, then the slot count must hold.
                for id in &decision.unchoke {
                    peers.get_mut(id).expect("named peer exists").2 = true;
                }
                for id in &decision.choke {
                    peers.get_mut(id).expect("named peer exists").2 = false;
                }
                let unchoked = peers.values().filter(|&&(_, _, up)| up).count();
                assert!(
                    unchoked <= slots,
                    "{}: {unchoked} peers unchoked, over the {slots} slots",
                    at()
                );
                for (id, &(interested, _, up)) in &peers {
                    assert!(
                        !up || interested,
                        "{}: {id} left unchoked while not interested",
                        at()
                    );
                }
            }
        }
    }
}

/// The optimistic slot's whole purpose: a peer that arrives after the slots are
/// taken must still get a turn. With every rate equal, the ranked picks never
/// change, so only the optimistic slot can ever unchoke the peers at the back —
/// and if it stops rotating, they wait for ever.
#[test]
fn every_interested_peer_gets_a_turn_eventually() {
    for slots in [1usize, 2, 4] {
        let count = slots + 4;
        let mut choker = Choker::new(slots);
        let mut unchoked: BTreeSet<u64> = BTreeSet::new();
        let mut ever: BTreeSet<u64> = BTreeSet::new();

        for round in 1..=400 {
            let snapshots: Vec<PeerSnapshot> = (0..count as u64)
                .map(|id| PeerSnapshot {
                    id,
                    interested: true,
                    // Equal rates: nothing but the optimistic slot can reorder
                    // who gets served.
                    rate: 0,
                    unchoked: unchoked.contains(&id),
                })
                .collect();
            let decision = choker.plan(&snapshots);
            for id in &decision.unchoke {
                unchoked.insert(*id);
                ever.insert(*id);
            }
            for id in &decision.choke {
                unchoked.remove(id);
            }
            assert!(
                unchoked.len() <= slots,
                "slots {slots} round {round}: {} unchoked",
                unchoked.len()
            );
            if ever.len() == count {
                break;
            }
        }
        assert_eq!(
            ever.len(),
            count,
            "slots {slots}: only {} of {count} peers were ever unchoked; the optimistic slot \
             is not rotating",
            ever.len()
        );
    }
}
