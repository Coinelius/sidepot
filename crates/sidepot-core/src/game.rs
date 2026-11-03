//! The round state machine (spec sections 5 and 6).
//!
//! Every rule reads a batch as a set: entries are grouped by player and each player's are taken in nonce order,
//! so the order a miner puts transactions in an L1 block cannot change the outcome.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Entry, Params};

pub type PlayerKey = [u8; 32];

/// Fixed-point bits of the per-key dividend accumulator.
const SCALE: u32 = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// sECX in sats, excluding dividends not yet settled (see [`Game::balance`]).
    pub balance: u64,
    /// Keys held in the current round.
    pub keys: u64,
    /// Highest accepted entry nonce.
    pub nonce: u64,
    /// Accumulator value already accounted for, scaled by 2^SCALE.
    debt: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    WrongRound,
    Keys,
    Price,
    Signature,
    Nonce,
    Balance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundEnd {
    pub round: u32,
    /// The last L1 block of the round with a valid BUY, whose buyers shared the jackpot.
    pub final_height: Option<u32>,
    pub winners: Vec<(PlayerKey, u64)>,
    /// Pot carried into the next round.
    pub carried: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReport {
    pub height: u32,
    /// Set when the previous round ended before this batch was read.
    pub round_end: Option<RoundEnd>,
    /// Per-key price every buy in this batch paid.
    pub price: u64,
    pub accepted: Vec<Entry>,
    pub rejected: Vec<(Entry, Reject)>,
}

/// The Sidepot block an L1 block BMM-accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bmm {
    pub producer: PlayerKey,
    /// The M8 transaction's fee, paid to the L1 miner.
    pub bid: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsufficientBalance;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub params: Params,
    pub round: u32,
    /// Last L1 height at which a buy still counts for this round.
    pub deadline: u32,
    pub price: u64,
    pub pot: u64,
    pub streak_pool: u64,
    /// Highest L1 height whose batch has been processed.
    pub height: u32,
    pub accounts: BTreeMap<PlayerKey, Account>,
    total_keys: u64,
    acc: u128,
    /// Dividends distributed and settled this round; the difference is owed to key holders.
    div_in: u64,
    div_out: u64,
    /// The latest block of this round with a valid BUY, and the keys each buyer bought in it.
    last_buy: Option<(u32, BTreeMap<PlayerKey, u64>)>,
    streak: Option<(PlayerKey, u32)>,
}

impl Game {
    /// A game whose first batch is the L1 block at `start_height`.
    pub fn new(params: Params, start_height: u32) -> Self {
        params.validate().expect("invalid params");
        Self {
            round: 1,
            deadline: start_height + params.start_time,
            price: params.start_price,
            pot: 0,
            streak_pool: 0,
            height: start_height - 1,
            accounts: BTreeMap::new(),
            total_keys: 0,
            acc: 0,
            div_in: 0,
            div_out: 0,
            last_buy: None,
            streak: None,
            params,
        }
    }

    /// Spendable balance including unsettled dividends.
    pub fn balance(&self, player: &PlayerKey) -> u64 {
        self.accounts.get(player).map_or(0, |a| a.balance + pending(a, self.acc))
    }

    pub fn deposit(&mut self, player: PlayerKey, amount: u64) {
        self.accounts.entry(player).or_default().balance += amount;
    }

    pub fn withdraw(&mut self, player: &PlayerKey, amount: u64) -> Result<(), InsufficientBalance> {
        let a = self.accounts.get_mut(player).ok_or(InsufficientBalance)?;
        settle(a, self.acc, &mut self.div_out);
        a.balance = a.balance.checked_sub(amount).ok_or(InsufficientBalance)?;
        Ok(())
    }

    /// Every sat the game holds. Changes only through [`Game::deposit`] and [`Game::withdraw`].
    pub fn total(&self) -> u128 {
        let balances: u128 = self.accounts.values().map(|a| u128::from(a.balance)).sum();
        balances + u128::from(self.pot) + u128::from(self.streak_pool) + u128::from(self.div_in - self.div_out)
    }

    /// Processes the entries found in the L1 block at `height`, which must follow the last processed one.
    pub fn process_batch(&mut self, height: u32, entries: &[Entry]) -> BatchReport {
        assert_eq!(height, self.height + 1, "batches must be processed in L1 height order");
        self.height = height;
        let round_end = (height > self.deadline).then(|| self.end_round(height));
        let price = self.price;

        let mut rejected = Vec::new();
        let mut by_player: BTreeMap<PlayerKey, Vec<Entry>> = BTreeMap::new();
        for e in entries {
            let reason = if e.round != self.round {
                Some(Reject::WrongRound)
            } else if e.keys == 0 || e.keys > self.params.max_keys {
                Some(Reject::Keys)
            } else if e.max_price < price {
                Some(Reject::Price)
            } else if !e.verify() {
                Some(Reject::Signature)
            } else {
                None
            };
            match reason {
                Some(r) => rejected.push((*e, r)),
                None => by_player.entry(e.player).or_default().push(*e),
            }
        }

        let mut accepted = Vec::new();
        let mut bought = BTreeMap::new();
        let mut spent: u64 = 0;
        for (player, mut list) in by_player {
            list.sort_by_key(|e| (e.nonce, e.encode()));
            // Players without an account have nothing to spend; no account is created for them.
            let Some(a) = self.accounts.get_mut(&player) else {
                rejected.extend(list.into_iter().map(|e| (e, Reject::Balance)));
                continue;
            };
            settle(a, self.acc, &mut self.div_out);
            let mut keys = 0;
            for e in list {
                let cost = u64::from(e.keys).checked_mul(price);
                if e.nonce <= a.nonce {
                    rejected.push((e, Reject::Nonce));
                } else if let Some(cost) = cost.filter(|&c| c <= a.balance) {
                    a.balance -= cost;
                    a.nonce = e.nonce;
                    keys += u64::from(e.keys);
                    spent += cost;
                    accepted.push(e);
                } else {
                    rejected.push((e, Reject::Balance));
                }
            }
            if keys > 0 {
                bought.insert(player, keys);
            }
        }

        let dividends = bps(spent, self.params.dividend_bps);
        let streak = bps(spent, self.params.streak_bps);
        self.streak_pool += streak;
        self.pot += spent - dividends - streak;
        // Dividends go to keys held before this block; with none, they stay in the pot.
        if self.total_keys == 0 {
            self.pot += dividends;
        } else {
            self.acc += (u128::from(dividends) << SCALE) / u128::from(self.total_keys);
            self.div_in += dividends;
        }

        let keys_bought: u64 = bought.values().sum();
        for (player, &k) in &bought {
            let a = self.accounts.get_mut(player).unwrap();
            a.keys += k;
            a.debt += u128::from(k) * self.acc;
        }
        self.total_keys += keys_bought;
        if keys_bought > 0 {
            let p = &self.params;
            self.deadline = self.deadline.max(height + p.extension).min(height + p.max_time);
            self.price = step_price(price, keys_bought, p.price_step_ppm);
            self.last_buy = Some((height, bought));
        }

        BatchReport { height, round_end, price, accepted, rejected }
    }

    /// Applies the streak bonus for one L1 block (spec section 6). `bmm` is the Sidepot block it BMM-accepted,
    /// if any. Returns the bonus paid.
    pub fn on_bmm(&mut self, bmm: Option<Bmm>) -> u64 {
        let p = &self.params;
        let Some(b) = bmm.filter(|b| b.bid >= p.min_bid) else {
            self.streak = None;
            return 0;
        };
        let streak = match self.streak {
            Some((producer, n)) if producer == b.producer => n + 1,
            _ => 1,
        };
        self.streak = Some((b.producer, streak));
        let doubled = 1u64.checked_shl(streak - 1).map_or(u64::MAX, |m| p.bonus_base.saturating_mul(m));
        let bonus = doubled
            .min(p.bonus_cap)
            .min(b.bid.saturating_mul(p.bid_multiple))
            .min(bps(self.streak_pool, p.bonus_pool_bps));
        if bonus > 0 {
            self.streak_pool -= bonus;
            self.deposit(b.producer, bonus);
        }
        bonus
    }

    /// Ends the round before the batch at `height` is read: pays the final block's buyers and starts the next.
    fn end_round(&mut self, height: u32) -> RoundEnd {
        for a in self.accounts.values_mut() {
            settle(a, self.acc, &mut self.div_out);
            a.keys = 0;
            a.debt = 0;
        }
        // Accumulator rounding leaves a few sats undistributed; they return to the pot.
        self.pot += self.div_in - self.div_out;

        let mut winners = Vec::new();
        let final_height = self.last_buy.take().map(|(h, buyers)| {
            let jackpot = bps(self.pot, self.params.jackpot_bps);
            let keys: u64 = buyers.values().sum();
            for (player, k) in buyers {
                let share = (u128::from(jackpot) * u128::from(k) / u128::from(keys)) as u64;
                self.accounts.get_mut(&player).unwrap().balance += share;
                self.pot -= share;
                winners.push((player, share));
            }
            h
        });

        let end = RoundEnd { round: self.round, final_height, winners, carried: self.pot };
        self.round += 1;
        self.deadline = height + self.params.start_time;
        self.price = self.params.start_price;
        self.total_keys = 0;
        self.acc = 0;
        self.div_in = 0;
        self.div_out = 0;
        end
    }
}

fn pending(a: &Account, acc: u128) -> u64 {
    ((u128::from(a.keys) * acc - a.debt) >> SCALE) as u64
}

/// Moves an account's dividends into its balance, keeping the sub-sat remainder owed.
fn settle(a: &mut Account, acc: u128, div_out: &mut u64) {
    let owed = pending(a, acc);
    a.balance += owed;
    a.debt += u128::from(owed) << SCALE;
    *div_out += owed;
}

fn bps(amount: u64, bps: u64) -> u64 {
    (u128::from(amount) * u128::from(bps) / 10_000) as u64
}

/// price × (1 + step)^keys, rounded down after each key and saturating at u64::MAX.
fn step_price(price: u64, keys: u64, step_ppm: u64) -> u64 {
    let mut p = u128::from(price);
    for _ in 0..keys {
        p = p * (1_000_000 + u128::from(step_ppm)) / 1_000_000;
        if p >= u128::from(u64::MAX) {
            return u64::MAX;
        }
    }
    p as u64
}
