//! Rules that are written but not active: the pending fork.
//!
//! The fork switches on at one L1 height that the node is started with. The height is NOT part of [`Game`] or
//! [`Params`]: a Sidepot block header commits to the serialized game state, so a field added there would change
//! the hash of every block already mined. It is handed to [`Game::process_batch`] on every call instead, which
//! leaves the state, its serialized form and so every pre-fork hash exactly as they were, and means no caller can
//! process a block without saying which rules apply.
//!
//! [`Game`]: crate::Game
//! [`Game::process_batch`]: crate::Game::process_batch

use crate::Params;

/// The price steps of the pending fork, in parts per million of the price. The one place to change them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceSteps {
    /// The step with a full round (`start_time` blocks) left on the clock: where the ramp starts.
    pub ramp_start_ppm: u64,
    /// The step the ramp heads for. It is reached only where the final stretch begins (`extension` blocks left),
    /// and there `final_ppm` applies, so the largest ramp step is just below this.
    pub ramp_end_ppm: u64,
    /// The step in the final stretch: `extension` blocks left or fewer.
    pub final_ppm: u64,
}

impl PriceSteps {
    /// 1% rising to 3% before the final stretch, 10% in it.
    pub const PENDING: Self = Self { ramp_start_ppm: 10_000, ramp_end_ppm: 30_000, final_ppm: 100_000 };
}

/// Which rules apply at which L1 height. Not consensus state: every node of a network must be given the same one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fork {
    /// The first L1 block whose buys step the price by the blocks left on the clock ([`Fork::step_ppm`]). Blocks
    /// below it, and every block while this is `None`, step it by the flat `Params::price_step_ppm`.
    pub price_height: Option<u32>,
    pub steps: PriceSteps,
}

impl Fork {
    /// No fork: the rules every network has run so far.
    pub const OFF: Self = Self { price_height: None, steps: PriceSteps::PENDING };

    /// The pending fork, active from the L1 block at `height`; `None` is [`Fork::OFF`].
    pub const fn at(height: Option<u32>) -> Self {
        Self { price_height: height, steps: PriceSteps::PENDING }
    }

    /// Whether the L1 block at `height` steps the price by the blocks left.
    pub fn price_active(&self, height: u32) -> bool {
        self.price_height.is_some_and(|activation| height >= activation)
    }

    /// The price step, in ppm, that the L1 block at `height` applies if it holds an accepted buy, with `left`
    /// blocks left on the round's clock.
    ///
    /// `left` is `deadline - clock`, where the clock already counts this block and the deadline is the one this
    /// block found, before its own buys move it (after the round turned over, if this block ended one). It is one
    /// less than the `deadline - clock` of the state the previous counted block left, which is what a player sees
    /// when sending the buy. A buy counts while `clock <= deadline`, so `left` is 0 at the last block of a round
    /// and never negative.
    ///
    /// Below the activation height: `Params::price_step_ppm`, whatever `left` is. From it on:
    /// - `left <= extension` (the final stretch): `final_ppm`;
    /// - otherwise `ramp_start_ppm + (ramp_end_ppm - ramp_start_ppm) * (start_time - min(left, start_time)) /
    ///   (start_time - extension)`, rounded down: `ramp_start_ppm` with `start_time` blocks left, rising by the
    ///   block towards `ramp_end_ppm`. A `left` above `start_time` (only possible with `max_time > start_time`)
    ///   counts as `start_time`. With the default parameters: 10,000 at 144 left, 29,855 at 7 left.
    /// - With `start_time <= extension` there is no ramp to speak of; a block outside the final stretch (only
    ///   possible with `max_time > start_time`) steps by `ramp_start_ppm`. Nothing divides by zero.
    ///
    /// `Params::price_step_ppm` plays no part once the fork is active.
    pub fn step_ppm(&self, params: &Params, height: u32, left: u32) -> u64 {
        if !self.price_active(height) {
            return params.price_step_ppm;
        }
        let s = &self.steps;
        if left <= params.extension {
            return s.final_ppm;
        }
        let span = params.start_time.saturating_sub(params.extension);
        if span == 0 {
            return s.ramp_start_ppm;
        }
        // Here extension < left and extension < start_time, so 0 <= gone < span.
        let gone = u128::from(params.start_time - left.min(params.start_time));
        let rise = |from: u64, to: u64| (u128::from(to - from) * gone / u128::from(span)) as u64;
        if s.ramp_end_ppm >= s.ramp_start_ppm {
            s.ramp_start_ppm + rise(s.ramp_start_ppm, s.ramp_end_ppm)
        } else {
            s.ramp_start_ppm - rise(s.ramp_end_ppm, s.ramp_start_ppm)
        }
    }
}

/// What the next counted L1 block would do with a buy: see [`Game::next_buy`](crate::Game::next_buy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextBuy {
    /// The round the buy would be in: the next one, if the current round's deadline has passed.
    pub round: u32,
    /// The per-key price it would pay.
    pub price: u64,
    /// Blocks left on the clock as that block is counted (the `left` of [`Fork::step_ppm`]).
    pub left: u32,
    /// The step the block would then apply to the price.
    pub step_ppm: u64,
    /// The price after that step, for the block after.
    pub price_after: u64,
}
