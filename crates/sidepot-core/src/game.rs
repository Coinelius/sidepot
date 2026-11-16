//! The round state machine (spec sections 5 and 6).
//!
//! Every rule reads a batch as a set: entries are grouped by player and each player's are taken in nonce order,
//! so the order a miner puts transactions in an L1 block cannot change the outcome.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{entry::Action, Domain, Entry, Params};

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
    /// The batch's block is past the entry's `expires` tick.
    Expired,
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
    /// Pot moved to the bonus pool because the round had no buys at all.
    pub to_bonus_pool: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReport {
    pub height: u32,
    /// The game clock at this batch.
    pub clock: u32,
    /// Set when the previous round ended before this batch was read.
    pub round_end: Option<RoundEnd>,
    /// Per-key price every buy in this batch paid.
    pub price: u64,
    pub accepted: Vec<Entry>,
    pub rejected: Vec<(Entry, Reject)>,
    /// Coins the sidechain block must create for this batch's cashouts: (address, sats).
    pub payouts: Vec<([u8; 20], u64)>,
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
    /// The Sidepot entries must be signed for.
    pub domain: Domain,
    pub round: u32,
    /// Last tick of the game clock at which a buy still counts for this round.
    pub deadline: u32,
    pub price: u64,
    pub pot: u64,
    pub streak_pool: u64,
    /// Highest L1 height that has been processed.
    pub height: u32,
    /// The game clock: the number of L1 blocks processed that held at least one non-coinbase transaction. Deadlines
    /// and entry expiries count these ticks, so empty L1 blocks neither run the countdown down nor age an entry.
    pub clock: u32,
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
    /// A game whose first block is the L1 block at `start_height`. The clock starts at 0, so the first block with
    /// transactions is tick 1.
    pub fn new(params: Params, domain: Domain, start_height: u32) -> Self {
        params.validate().expect("invalid params");
        Self {
            round: 1,
            deadline: 1 + params.start_time,
            price: params.start_price,
            pot: 0,
            streak_pool: 0,
            height: start_height - 1,
            clock: 0,
            accounts: BTreeMap::new(),
            total_keys: 0,
            acc: 0,
            div_in: 0,
            div_out: 0,
            last_buy: None,
            streak: None,
            params,
            domain,
        }
    }

    /// The current streak: its producer and length in L1 blocks.
    pub fn streak(&self) -> Option<(PlayerKey, u32)> {
        self.streak
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

    /// Every sat the game holds. Changes only through [`Game::deposit`], [`Game::withdraw`] and cashouts.
    pub fn total(&self) -> u128 {
        let balances: u128 = self.accounts.values().map(|a| u128::from(a.balance)).sum();
        balances + u128::from(self.pot) + u128::from(self.streak_pool) + u128::from(self.div_in - self.div_out)
    }

    /// Passes over the L1 block at `height`, which holds nothing but its coinbase. The clock does not tick: an
    /// empty block cannot carry a buy, so it must not bring the deadline closer either.
    pub fn process_empty(&mut self, height: u32) {
        assert_eq!(height, self.height + 1, "blocks must be processed in L1 height order");
        self.height = height;
    }

    /// Processes the L1 block at `height`, which must follow the last processed one and hold at least one
    /// non-coinbase transaction. `entries` are the entries found in its non-coinbase transactions.
    pub fn process_batch(&mut self, height: u32, entries: &[Entry]) -> BatchReport {
        assert_eq!(height, self.height + 1, "blocks must be processed in L1 height order");
        self.height = height;
        self.clock += 1;
        let clock = self.clock;
        let round_end = (clock > self.deadline).then(|| self.end_round());
        let price = self.price;

        let mut rejected = Vec::new();
        let mut by_player: BTreeMap<PlayerKey, Vec<Entry>> = BTreeMap::new();
        for e in entries {
            let reason = match e.action {
                _ if e.expires < clock => Some(Reject::Expired),
                Action::Buy { round, .. } if round != self.round => Some(Reject::WrongRound),
                Action::Buy { keys, .. } if keys == 0 || keys > self.params.max_keys => Some(Reject::Keys),
                Action::Buy { max_price, .. } if max_price < price => Some(Reject::Price),
                _ if !e.verify(&self.domain) => Some(Reject::Signature),
                _ => None,
            };
            match reason {
                Some(r) => rejected.push((*e, r)),
                None => by_player.entry(e.player).or_default().push(*e),
            }
        }

        let mut accepted = Vec::new();
        let mut payouts = Vec::new();
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
                // Sats leaving the balance: a buy's cost, or a cashout's amount (0 meaning all of it).
                let debit = match e.action {
                    Action::Buy { keys, .. } => u64::from(keys).checked_mul(price),
                    Action::Cashout { amount: 0, .. } => Some(a.balance).filter(|&b| b > 0),
                    Action::Cashout { amount, .. } => Some(amount),
                };
                if e.nonce <= a.nonce {
                    rejected.push((e, Reject::Nonce));
                } else if let Some(debit) = debit.filter(|&d| d <= a.balance) {
                    a.balance -= debit;
                    a.nonce = e.nonce;
                    match e.action {
                        Action::Buy { keys: k, .. } => {
                            keys += u64::from(k);
                            spent += debit;
                        }
                        Action::Cashout { dest, .. } => payouts.push((dest, debit)),
                    }
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
            self.deadline = self.deadline.max(clock + p.extension).min(clock + p.max_time);
            self.price = step_price(price, p.price_step_ppm);
            self.last_buy = Some((height, bought));
        }

        BatchReport { height, clock, round_end, price, accepted, rejected, payouts }
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
        // The cap halved once per remaining ramp block: 1/8, 1/4, 1/2, then the full cap with a ramp of 3.
        let cap = bps(self.streak_pool, p.bonus_pool_bps);
        let ramp = cap >> p.bonus_ramp.saturating_sub(streak - 1);
        let bonus = ramp.min(b.bid.saturating_mul(p.bid_multiple));
        if bonus > 0 {
            self.streak_pool -= bonus;
            self.deposit(b.producer, bonus);
        }
        bonus
    }

    /// Ends the round before the current block's batch is read: pays the final block's buyers and starts the next.
    fn end_round(&mut self) -> RoundEnd {
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
        // A round nobody played: rather than roll a dead game's pot forward forever, pay it out through the
        // block bonus, to producers and, through their bids, to miners.
        let to_bonus_pool = if final_height.is_none() { std::mem::take(&mut self.pot) } else { 0 };
        self.streak_pool += to_bonus_pool;

        let end = RoundEnd { round: self.round, final_height, winners, carried: self.pot, to_bonus_pool };
        self.round += 1;
        self.deadline = self.clock + self.params.start_time;
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

/// price × (1 + step), rounded down and saturating at u64::MAX.
fn step_price(price: u64, step_ppm: u64) -> u64 {
    let p = u128::from(price) * (1_000_000 + u128::from(step_ppm)) / 1_000_000;
    u64::try_from(p).unwrap_or(u64::MAX)
}
