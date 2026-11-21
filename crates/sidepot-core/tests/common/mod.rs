//! A fixed script of L1 blocks, the same on every run, for pinning the game's serialized state.
//!
//! The values pinned in tests/fork.rs were produced by running exactly this file against the crate as it was
//! before the fork code existed (rev 31937a5, where `process_batch` took no fork), with `step` calling that
//! `process_batch(height, entries)`.

use secp256k1::{Keypair, SECP256K1};
use sidepot_core::{Action, BatchReport, Bmm, Domain, Entry, Game, Params};

pub const DOMAIN: Domain = Domain { anchor: [7; 32], slot: 88 };
pub const START: u32 = 100;
/// The script's last L1 height.
pub const END: u32 = 1099;
const PLAYERS: u8 = 6;

pub fn keypair(i: u8) -> Keypair {
    Keypair::from_seckey_slice(SECP256K1, &[i + 1; 32]).unwrap()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the header of a Sidepot block commits to for a game state, as the node computes it.
pub fn game_hash(game: &Game) -> [u8; 32] {
    blake3::hash(&bincode::serialize(game).unwrap()).into()
}

pub struct Step {
    pub height: u32,
    /// The game's serialized state after the block.
    pub bytes: Vec<u8>,
    pub price_before: u64,
    pub price_after: u64,
    /// Whether the block held an accepted buy.
    pub bought: bool,
    /// `deadline - clock` of the state before the block, and whether the block ended a round.
    pub left_shown: u32,
    pub round_ended: bool,
}

/// Runs the script: L1 blocks START..=END, about one in ten empty, in alternating busy and quiet stretches so
/// that rounds are extended, run out with and without buys, and turn over. Deposits, buys by six players (some
/// for the wrong round, expired, above their price limit or beyond their balance), cashouts and block bonuses
/// all occur. `step` processes one block with transactions.
pub fn run(mut step: impl FnMut(&mut Game, u32, &[Entry]) -> BatchReport) -> Vec<Step> {
    let mut rng = Rng(0x51de_9075);
    let mut g = Game::new(Params::default(), DOMAIN, START);
    let keys: Vec<[u8; 32]> = (0..PLAYERS).map(|i| keypair(i).x_only_public_key().0.serialize()).collect();
    let mut out = Vec::new();
    for height in START..=END {
        let price_before = g.price;
        let left_shown = g.deadline.saturating_sub(g.clock);
        if rng.below(20) == 0 {
            let p = rng.below(u64::from(PLAYERS)) as usize;
            g.deposit(keys[p], 50_000_000 + rng.below(3_000_000_000));
        }
        let mut bought = false;
        let mut round_ended = false;
        if rng.below(10) == 0 {
            g.process_empty(height);
        } else {
            // Busy for 110 blocks, then quiet for 190: long enough for a round to run out.
            let busy = (height - START) % 300 < 110;
            let mut entries = Vec::new();
            let n = if busy { rng.below(4) } else { u64::from(rng.below(40) == 0) };
            let mut in_batch = [0u64; PLAYERS as usize];
            for _ in 0..n {
                let p = rng.below(u64::from(PLAYERS)) as usize;
                in_batch[p] += 1;
                let nonce = g.accounts.get(&keys[p]).map_or(0, |a| a.nonce) + in_batch[p];
                let kp = keypair(p as u8);
                // The round a wallet would read from the state before this block.
                let round = if g.clock + 1 > g.deadline { g.round + 1 } else { g.round };
                let entry = match rng.below(12) {
                    0 => Entry::buy(&kp, &DOMAIN, round + 1, 3, u64::MAX, nonce, u32::MAX),
                    1 => Entry::buy(&kp, &DOMAIN, round, 2, u64::MAX, nonce, g.clock),
                    2 => Entry::buy(&kp, &DOMAIN, round, 1, 999_999, nonce, u32::MAX),
                    3 => Entry::buy(&kp, &DOMAIN, round, 1000, u64::MAX, nonce, u32::MAX),
                    4 => Entry::cashout(&kp, &DOMAIN, rng.below(40_000_000), [p as u8; 20], nonce, u32::MAX),
                    5 if rng.below(4) == 0 => Entry::cashout(&kp, &DOMAIN, 0, [p as u8; 20], nonce, u32::MAX),
                    _ => {
                        let k = 1 + rng.below(25) as u16;
                        Entry::buy(&kp, &DOMAIN, round, k, u64::MAX, nonce, g.clock + 3)
                    }
                };
                entries.push(entry);
            }
            let report = step(&mut g, height, &entries);
            round_ended = report.round_end.is_some();
            bought = report.accepted.iter().any(|e| matches!(e.action, Action::Buy { .. }));
        }
        let bmm =
            (rng.below(5) != 0).then(|| Bmm { producer: keys[rng.below(2) as usize], bid: 5_000 + rng.below(40_000) });
        g.on_bmm(bmm);
        out.push(Step {
            height,
            bytes: bincode::serialize(&g).unwrap(),
            price_before,
            price_after: g.price,
            bought,
            left_shown,
            round_ended,
        });
    }
    out
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hash of every state up to and including the block at `height`: each block's game hash chained onto the last.
pub fn chained(steps: &[Step], height: u32) -> String {
    let mut acc = [0u8; 32];
    for s in steps.iter().take_while(|s| s.height <= height) {
        let hash: [u8; 32] = blake3::hash(&s.bytes).into();
        acc = blake3::hash(&[acc, hash].concat()).into();
    }
    hex(&acc)
}
