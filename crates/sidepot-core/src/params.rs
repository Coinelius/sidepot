use serde::{Deserialize, Serialize};

/// Game parameters (spec section 7). Shares are in basis points of the amount they split.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    /// L1 data is processed this many blocks deep (K).
    pub depth: u32,
    /// Per-key price at the start of a round, in sats.
    pub start_price: u64,
    /// Price growth per key bought, in parts per million.
    pub price_step_ppm: u64,
    /// L1 blocks a round lasts without buys.
    pub start_time: u32,
    /// The deadline never moves further than this past the latest buy.
    pub max_time: u32,
    /// A buy pushes the deadline to at least this many blocks past its block.
    pub extension: u32,
    /// Most keys one entry may buy.
    pub max_keys: u16,
    pub pot_bps: u64,
    pub dividend_bps: u64,
    pub streak_bps: u64,
    /// Share of the pot paid to the final block's buyers when a round ends.
    pub jackpot_bps: u64,
    /// Least BMM bid (M8 fee paid to the L1 miner) that qualifies for the streak bonus.
    pub min_bid: u64,
    /// A bonus is at most this many times the bid of its block.
    pub bid_multiple: u64,
    /// The bonus cap, as a share of the streak pool.
    pub bonus_pool_bps: u64,
    /// Blocks a streak takes to reach the cap: its first block earns cap / 2^bonus_ramp, doubling each block. With
    /// 3, a streak earns 1/8, 1/4, 1/2 of the cap, then all of it; with 0, every winning block earns the full cap.
    pub bonus_ramp: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            depth: 2,
            start_price: 1_000_000,
            price_step_ppm: 5_000,
            start_time: 144,
            max_time: 144,
            extension: 6,
            max_keys: 1000,
            pot_bps: 5_000,
            dividend_bps: 2_500,
            streak_bps: 2_500,
            jackpot_bps: 5_000,
            min_bid: 10_000,
            bid_multiple: 2,
            bonus_pool_bps: 200,
            bonus_ramp: 3,
        }
    }
}

impl Params {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.pot_bps + self.dividend_bps + self.streak_bps != 10_000 {
            return Err("pot, dividend and streak shares must sum to 10,000 bps");
        }
        if self.jackpot_bps > 10_000 || self.bonus_pool_bps > 10_000 {
            return Err("a share exceeds 10,000 bps");
        }
        if self.bonus_ramp > 32 {
            return Err("bonus_ramp must be at most 32");
        }
        if self.start_price == 0 || self.max_keys == 0 {
            return Err("start_price and max_keys must be positive");
        }
        if self.extension > self.max_time || self.start_time > self.max_time {
            return Err("extension and start_time must not exceed max_time");
        }
        Ok(())
    }
}
