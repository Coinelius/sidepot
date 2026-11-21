use secp256k1::{Keypair, SECP256K1};
use sidepot_core::{
    deposit_player, entry, Action, BatchReport, Bmm, DecodeError, Domain, Entry, Fork, Game, Params, PlayerKey, Reject,
};

/// The game starts at L1 height 1, so that as long as a test has no empty blocks the game clock equals the height.
const START: u32 = 1;
const DOMAIN: Domain = Domain { anchor: [7; 32], slot: 88 };
/// An expiry far past every test's last block.
const NEVER: u32 = u32::MAX;

/// These tests are of the rules without the pending fork (its own are in tests/fork.rs).
trait PreFork {
    fn batch(&mut self, height: u32, entries: &[Entry]) -> BatchReport;
}
impl PreFork for Game {
    fn batch(&mut self, height: u32, entries: &[Entry]) -> BatchReport {
        self.process_batch(height, entries, &Fork::OFF)
    }
}

fn keypair(i: u8) -> Keypair {
    Keypair::from_seckey_slice(SECP256K1, &[i + 1; 32]).unwrap()
}

fn key(i: u8) -> PlayerKey {
    keypair(i).x_only_public_key().0.serialize()
}

fn buy(i: u8, round: u32, keys: u16, nonce: u64) -> Entry {
    Entry::buy(&keypair(i), &DOMAIN, round, keys, u64::MAX, nonce, NEVER)
}

/// Processes empty batches up to and including `height`.
fn advance(g: &mut Game, height: u32) {
    while g.height < height {
        g.batch(g.height + 1, &[]);
    }
}

/// xorshift64, so the randomised tests are reproducible without extra dependencies.
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

#[test]
fn entry_roundtrip_and_signature() {
    let e = Entry::buy(&keypair(0), &DOMAIN, 7, 3, 1_500_000, 42, NEVER);
    assert!(e.verify(&DOMAIN));
    let bytes = e.encode();
    assert_eq!(bytes.len(), entry::BUY_LEN);
    assert_eq!(bytes.len(), 128);
    assert_eq!(&bytes[0..6], b"SPOT\x00\x01");
    assert_eq!(Entry::decode(&bytes), Ok(e));
    assert_eq!(Entry::from_script(&e.script()), Some(Ok(e)));

    let mut tampered = e;
    tampered.action = Action::Buy { round: 7, keys: 4, max_price: 1_500_000 };
    assert!(!tampered.verify(&DOMAIN), "the signature covers the key count");
    let mut later = e;
    later.expires -= 1;
    assert!(!later.verify(&DOMAIN), "the signature covers the expiry");
    // The same bytes are no entry for another slot or another chain.
    assert!(!e.verify(&Domain { slot: 89, ..DOMAIN }));
    assert!(!e.verify(&Domain { anchor: [8; 32], ..DOMAIN }));
    let mut wrong_player = e;
    wrong_player.player = key(1);
    assert!(!wrong_player.verify(&DOMAIN));

    let mut v1 = bytes.clone();
    v1[4] = 1;
    assert_eq!(Entry::decode(&v1), Err(DecodeError::Version(1)));
    let mut unknown = bytes.clone();
    unknown[5] = 3;
    assert_eq!(Entry::decode(&unknown), Err(DecodeError::Kind(3)));
    let mut as_cashout = bytes.clone();
    as_cashout[5] = 2;
    assert_eq!(Entry::decode(&as_cashout), Err(DecodeError::Length(128)), "a cashout is 142 bytes");
    assert_eq!(Entry::decode(&bytes[..127]), Err(DecodeError::Length(127)));
    // Other data outputs are not entries at all.
    assert_eq!(Entry::from_script(&[0x6a, 0x04, b'S', b'P', b'O', b'T']), None);
    let mut other = e.script();
    other[3] = b'X';
    assert_eq!(Entry::from_script(&other), None);
}

#[test]
fn dividends_go_only_to_earlier_holders() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    let (a, b) = (key(0), key(1));
    g.deposit(a, 100_000_000);
    g.deposit(b, 100_000_000);

    // A buys first: nobody holds keys yet, so the dividend share stays in the pot.
    let r = g.batch(START, &[buy(0, 1, 10, 1)]);
    assert_eq!(r.accepted.len(), 1);
    let a_cost = 10 * 1_000_000;
    assert_eq!(g.balance(&a), 100_000_000 - a_cost);
    assert_eq!(g.pot, a_cost * 75 / 100);
    assert_eq!(g.streak_pool, a_cost * 25 / 100);

    // B buys next block at the stepped price; A earns 25% of it, B earns nothing from its own buy.
    let r = g.batch(START + 1, &[buy(1, 1, 5, 1)]);
    let b_cost = 5 * r.price;
    assert!(r.price > 1_000_000);
    assert_eq!(g.balance(&b), 100_000_000 - b_cost);
    let a_div = g.balance(&a) - (100_000_000 - a_cost);
    assert!(b_cost / 4 - a_div <= 1, "A gets the whole dividend, less accumulator rounding");
}

#[test]
fn batch_order_does_not_matter() {
    let mut rng = Rng(7);
    for trial in 0..20 {
        let mut entries: Vec<Entry> = (0..12)
            .map(|_| {
                let p = rng.below(4) as u8;
                buy(p, 1, 1 + rng.below(50) as u16, 1 + rng.below(3))
            })
            .collect();
        let mut g1 = Game::new(Params::default(), DOMAIN, START);
        for p in 0..4 {
            g1.deposit(key(p), 20_000_000 + rng.below(40_000_000));
        }
        let mut g2 = g1.clone();
        let r1 = g1.batch(START, &entries);
        for i in (1..entries.len()).rev() {
            entries.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let r2 = g2.batch(START, &entries);
        assert_eq!(g1, g2, "trial {trial}: state depends on order");
        assert_eq!(r1.accepted.len(), r2.accepted.len());
    }
}

#[test]
fn same_player_entries_are_taken_in_nonce_order() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.deposit(key(0), 15_000_000);
    // Each costs 10M at the batch price; only the lower nonce fits, whichever comes first in the block.
    let r = g.batch(START, &[buy(0, 1, 10, 9), buy(0, 1, 10, 3)]);
    assert_eq!(r.accepted.iter().map(|e| e.nonce).collect::<Vec<_>>(), vec![3]);
    assert_eq!(r.rejected[0].0.nonce, 9);
    assert_eq!(r.rejected[0].1, Reject::Balance);
    // The rejected entry did not use up its nonce.
    g.deposit(key(0), 50_000_000);
    let r = g.batch(START + 1, &[buy(0, 1, 10, 9)]);
    assert_eq!(r.accepted.len(), 1);
}

#[test]
fn replays_and_bad_entries_are_ignored() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.deposit(key(0), 1_000_000_000);
    let e = buy(0, 1, 1, 5);
    assert_eq!(g.batch(START, &[e, e]).accepted.len(), 1, "a duplicate in one block counts once");
    assert_eq!(g.batch(START + 1, &[e]).rejected[0].1, Reject::Nonce);

    let price = g.price;
    let cheap = Entry::buy(&keypair(0), &DOMAIN, 1, 1, price - 1, 6, NEVER);
    assert_eq!(g.batch(START + 2, &[cheap]).rejected[0].1, Reject::Price);
    assert_eq!(g.batch(START + 3, &[buy(0, 2, 1, 7)]).rejected[0].1, Reject::WrongRound);
    assert_eq!(g.batch(START + 4, &[buy(0, 1, 1001, 8)]).rejected[0].1, Reject::Keys);
    let mut forged = buy(0, 1, 1, 9);
    forged.sig[0] ^= 1;
    assert_eq!(g.batch(START + 5, &[forged]).rejected[0].1, Reject::Signature);
    let r = g.batch(START + 6, &[buy(9, 1, 1, 1)]);
    assert_eq!(r.rejected[0].1, Reject::Balance, "no account, nothing to spend");
    assert!(!g.accounts.contains_key(&key(9)), "rejected entries create no account");
}

#[test]
fn deadline_extends_and_is_capped() {
    let p = Params::default();
    let mut g = Game::new(p.clone(), DOMAIN, START);
    g.deposit(key(0), u64::MAX / 4);
    assert_eq!(g.deadline, START + 144);

    // An early buy leaves the deadline where it is.
    g.batch(START, &[buy(0, 1, 1, 1)]);
    assert_eq!(g.deadline, START + 144);

    // A buy close to the deadline pushes it to EXTENSION blocks past its block.
    advance(&mut g, START + 141);
    g.batch(START + 142, &[buy(0, 1, 1, 2)]);
    assert_eq!(g.deadline, START + 148);

    // A buy exactly at the deadline still counts.
    advance(&mut g, START + 147);
    let r = g.batch(START + 148, &[buy(0, 1, 1, 3)]);
    assert_eq!(r.accepted.len(), 1);
    assert_eq!(g.deadline, START + 154);

    // The first batch past the deadline ends the round before it is read.
    advance(&mut g, START + 154);
    let r = g.batch(START + 155, &[buy(0, 1, 1, 4)]);
    let end = r.round_end.expect("round ended");
    assert_eq!(end.final_height, Some(START + 148));
    assert_eq!(r.rejected[0].1, Reject::WrongRound);
    assert_eq!(g.round, 2);
    assert_eq!(g.deadline, START + 155 + 144);
    assert_eq!(g.price, p.start_price);
}

#[test]
fn final_block_buyers_split_the_jackpot_pro_rata() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    for p in 0..3 {
        g.deposit(key(p), 1_000_000_000);
    }
    g.batch(START, &[buy(0, 1, 50, 1)]);
    advance(&mut g, START + 9);
    g.batch(START + 10, &[buy(1, 1, 10, 1), buy(2, 1, 20, 1)]);
    advance(&mut g, START + 144);
    let before: Vec<u64> = (0..3).map(|p| g.balance(&key(p))).collect();
    let pot = g.pot;

    let end = g.batch(START + 145, &[]).round_end.expect("round ended");
    assert_eq!(end.final_height, Some(START + 10));
    let paid: Vec<u64> = (0..3).map(|p| g.balance(&key(p)) - before[p as usize]).collect();
    assert_eq!(paid[0], 0, "earlier buyers do not share the jackpot");
    // Three quarters of the pot (plus a few sats of returned dividend rounding), split 10:20 by keys bought in the
    // final block.
    assert!((paid[1] + paid[2]).abs_diff(pot * 3 / 4) <= 3);
    assert!(paid[2].abs_diff(2 * paid[1]) <= 1);
    assert_eq!(end.carried, g.pot);
    assert!(g.accounts.values().all(|a| a.keys == 0));
}

#[test]
fn a_round_without_buys_moves_its_pot_to_the_bonus_pool() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.deposit(key(0), 1_000_000_000);
    g.batch(START, &[buy(0, 1, 10, 1)]);
    advance(&mut g, START + 144);
    // Round 1 had a buy: its final block wins 75%, and 25% carries over.
    let end = g.batch(START + 145, &[]).round_end.expect("round 1 ended");
    assert_eq!(end.final_height, Some(START));
    assert_eq!(end.to_bonus_pool, 0);
    let carried = end.carried;
    assert!(carried > 0);
    assert_eq!(g.pot, carried);

    // Round 2 has no buys at all: no winner, and its pot goes to the bonus pool instead of rolling on.
    let pool = g.streak_pool;
    advance(&mut g, START + 145 + 144);
    let end = g.batch(START + 290, &[]).round_end.expect("round 2 ended");
    assert_eq!(end.final_height, None);
    assert!(end.winners.is_empty());
    assert_eq!(end.to_bonus_pool, carried);
    assert_eq!((g.pot, g.streak_pool), (0, pool + carried));
    assert_eq!(g.round, 3);
}

#[test]
fn price_steps_once_per_block_with_a_buy() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.deposit(key(0), u64::MAX / 4);
    g.deposit(key(1), u64::MAX / 4);
    // Ten keys from two players in one block: one 1% step.
    g.batch(START, &[buy(0, 1, 7, 1), buy(1, 1, 3, 1)]);
    assert_eq!(g.price, 1_010_000);
    // Blocks without a buy, or with only a cashout or a rejected buy, leave the price alone.
    g.batch(START + 1, &[]);
    let stale = Entry::buy(&keypair(0), &DOMAIN, 1, 1, 1, 2, NEVER);
    let r = g.batch(START + 2, &[stale, Entry::cashout(&keypair(1), &DOMAIN, 1, [0; 20], 2, NEVER)]);
    assert_eq!(r.rejected, vec![(stale, Reject::Price)]);
    assert_eq!(g.price, 1_010_000);
    g.batch(START + 3, &[buy(0, 1, 1, 3)]);
    assert_eq!(g.price, 1_020_100);
}

/// Under per-key pricing, a few thousand keys in a round's first block priced every later key above the pot (or
/// at u64::MAX), so the bulk buyer could not be followed and took the jackpot. The price must not depend on volume.
#[test]
fn a_bulk_buy_cannot_price_out_the_next_block() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.pot = 4_000_000_000; // 40 ECX carried from an earlier round
    g.deposit(key(0), 100_000_000_000);
    g.deposit(key(1), 2_000_000);
    let bulk: Vec<Entry> = (1..=7).map(|n| buy(0, 1, 1000, n)).collect();
    let r = g.batch(START, &bulk);
    assert_eq!(r.accepted.len(), 7);
    assert_eq!(g.price, 1_010_000);
    // A rival with two cents follows in the next block and is the only buyer of the final block.
    let r = g.batch(START + 1, &[buy(1, 1, 1, 1)]);
    assert_eq!(r.accepted.len(), 1);
    let (deadline, before) = (g.deadline, g.balance(&key(0)));
    advance(&mut g, deadline);
    let end = g.batch(deadline + 1, &[]).round_end.unwrap();
    assert_eq!(end.final_height, Some(START + 1));
    assert_eq!(end.winners.iter().map(|w| w.0).collect::<Vec<_>>(), vec![key(1)]);
    assert_eq!(g.balance(&key(0)), before);
}

#[test]
fn an_optional_ramp_doubles_the_bonus_along_a_streak() {
    let mut g = Game::new(Params { bonus_ramp: 3, bonus_pool_bps: 200, ..Params::default() }, DOMAIN, START);
    let x = key(5);
    g.streak_pool = 1_000_000_000;
    // The cap is 2% of the pool at the time; a streak earns 1/8, 1/4, 1/2 of it, then all of it.
    let cap = |g: &Game| g.streak_pool / 50;

    let big = Some(Bmm { producer: x, bid: 100_000_000 });
    for shift in [3, 2, 1, 0, 0] {
        let expected = cap(&g) >> shift;
        assert_eq!(g.on_bmm(big), expected);
    }

    // A small qualifying bid caps the bonus at BID_MULTIPLE times the bid.
    assert_eq!(g.on_bmm(Some(Bmm { producer: x, bid: 20_000 })), 40_000);
    // A bid under MIN_BID earns nothing and resets the streak.
    assert_eq!(g.on_bmm(Some(Bmm { producer: x, bid: 9_999 })), 0);
    let expected = cap(&g) / 8;
    assert_eq!(g.on_bmm(big), expected);
    // So does an L1 block without a Sidepot block, or another producer.
    g.on_bmm(None);
    let expected = cap(&g) / 8;
    assert_eq!(g.on_bmm(big), expected);
    let expected = cap(&g) / 8;
    assert_eq!(g.on_bmm(Some(Bmm { producer: key(6), bid: 100_000_000 })), expected);

    // The bonus scales with the pool: a pool ten times bigger pays a bonus ten times bigger.
    let mut small = Game::new(Params::default(), DOMAIN, START);
    let mut large = small.clone();
    small.streak_pool = 100_000_000;
    large.streak_pool = 1_000_000_000;
    let (s, l) = (small.on_bmm(big), large.on_bmm(big));
    assert_eq!(l, 10 * s);
}

#[test]
fn sats_are_conserved_across_rounds() {
    let mut rng = Rng(42);
    let params = Params { start_time: 20, max_time: 20, ..Params::default() };
    let mut g = Game::new(params, DOMAIN, START);
    let mut nonces = [0u64; 6];
    let mut net: u128 = 0;
    let mut rounds_with_winner = 0;
    for h in START..START + 400 {
        if rng.below(5) == 0 {
            let p = rng.below(6) as u8;
            let amount = 1_000_000 + rng.below(200_000_000);
            g.deposit(key(p), amount);
            net += u128::from(amount);
        }
        if rng.below(7) == 0 {
            let p = key(rng.below(6) as u8);
            let amount = g.balance(&p) / 2;
            if g.withdraw(&p, amount).is_ok() {
                net -= u128::from(amount);
            }
        }
        // Quiet stretches longer than the timer, so rounds end: the price alone no longer stops the buying.
        let busy = (h - START) % 60 < 35;
        let entries: Vec<Entry> = (0..if busy { rng.below(4) } else { 0 })
            .map(|_| {
                let p = rng.below(6) as usize;
                nonces[p] += 1 + rng.below(2);
                match rng.below(8) {
                    0 => Entry::cashout(
                        &keypair(p as u8),
                        &DOMAIN,
                        rng.below(3) * 10_000_000,
                        [p as u8; 20],
                        nonces[p],
                        NEVER,
                    ),
                    _ => buy(p as u8, g.round, 1 + rng.below(30) as u16, nonces[p]),
                }
            })
            .collect();
        let r = g.batch(h, &entries);
        net -= r.payouts.iter().map(|&(_, sats)| u128::from(sats)).sum::<u128>();
        if r.round_end.is_some_and(|e| !e.winners.is_empty()) {
            rounds_with_winner += 1;
        }
        let producer = key(rng.below(3) as u8);
        g.on_bmm((rng.below(3) > 0).then_some(Bmm { producer, bid: 5_000 + rng.below(100_000) }));
        assert_eq!(g.total(), net, "sats created or lost at height {h}");
    }
    assert!(rounds_with_winner >= 3, "the simulation should cover several finished rounds");
}

#[test]
fn cashout_entries_pay_out_game_balance() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    let a = key(0);
    let dest = [7u8; 20];
    g.deposit(a, 50_000_000);

    let e = Entry::cashout(&keypair(0), &DOMAIN, 20_000_000, dest, 1, NEVER);
    let bytes = e.encode();
    assert_eq!(bytes.len(), entry::CASHOUT_LEN);
    assert_eq!(bytes.len(), 142);
    assert_eq!(Entry::from_script(&e.script()), Some(Ok(e)));
    assert!(e.verify(&DOMAIN));

    // A cashout is not tied to a round, and is taken in nonce order with the player's buys.
    let r = g.batch(START, &[buy(0, 1, 10, 2), e, Entry::cashout(&keypair(0), &DOMAIN, 30_000_000, dest, 3, NEVER)]);
    assert_eq!(r.payouts, vec![(dest, 20_000_000)]);
    assert_eq!(r.accepted.len(), 2, "cashout 20M, then buy 10M; the 30M cashout no longer fits");
    assert_eq!(r.rejected[0].1, Reject::Balance);
    assert_eq!(g.balance(&a), 20_000_000);
    assert_eq!(g.total(), 50_000_000 - 20_000_000);

    // Amount 0 cashes out everything, dividends included; with nothing left it is rejected.
    g.deposit(key(1), 100_000_000);
    g.batch(START + 1, &[buy(1, 1, 5, 1)]);
    let all = g.balance(&a);
    assert!(all > 20_000_000, "A earned dividends from B's buy");
    let r = g.batch(START + 2, &[Entry::cashout(&keypair(0), &DOMAIN, 0, dest, 4, NEVER)]);
    assert_eq!(r.payouts, vec![(dest, all)]);
    assert_eq!(g.balance(&a), 0);
    let r = g.batch(START + 3, &[Entry::cashout(&keypair(0), &DOMAIN, 0, dest, 5, NEVER)]);
    assert_eq!(r.rejected[0].1, Reject::Balance);
    assert!(r.payouts.is_empty());
}

#[test]
fn deposit_addresses_name_game_accounts() {
    let hex: String = key(3).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(deposit_player(&format!("spot:{hex}")), Some(key(3)));
    assert_eq!(deposit_player(&format!("spot:{}", hex.to_uppercase())), Some(key(3)));
    assert_eq!(deposit_player(&hex), None, "a plain address is an ordinary sidechain address");
    assert_eq!(deposit_player(&format!("spot:{}", &hex[2..])), None);
    // 0x00..00 is not a valid x-only key.
    assert_eq!(deposit_player(&format!("spot:{}", "0".repeat(64))), None);
}

#[test]
fn every_block_winner_earns_the_flat_cap() {
    // The default: 5% of the pool to each L1 block's BMM winner, whoever won the block before.
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.streak_pool = 1_000_000_000;
    for producer in [key(5), key(6), key(5), key(5)] {
        let cap = g.streak_pool / 20;
        assert_eq!(g.on_bmm(Some(Bmm { producer, bid: 100_000_000 })), cap, "a newcomer earns as much as an incumbent");
    }
}

#[test]
fn entries_count_only_up_to_their_expiry() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.deposit(key(0), 100_000_000);
    let kp = keypair(0);
    // Valid in its last block, ignored in the next one.
    let on_time = Entry::buy(&kp, &DOMAIN, 1, 1, u64::MAX, 1, START);
    assert_eq!(g.batch(START, &[on_time]).accepted, vec![on_time]);
    let late = Entry::buy(&kp, &DOMAIN, 1, 1, u64::MAX, 2, START);
    assert_eq!(g.batch(START + 1, &[late]).rejected, vec![(late, Reject::Expired)]);
    assert_eq!(g.accounts[&key(0)].nonce, 1, "an expired entry leaves the nonce alone");
}

/// Without an expiry, a cashout rejected for lack of balance could be re-posted by anyone after a later deposit and
/// would empty the account just before its owner's buy.
#[test]
fn a_rejected_entry_cannot_be_replayed_after_it_expires() {
    let mut g = Game::new(Params::default(), DOMAIN, START);
    g.deposit(key(0), 100_000_000);
    let too_much = Entry::cashout(&keypair(0), &DOMAIN, 500_000_000, [9; 20], 1, START + 3);
    assert_eq!(g.batch(START, &[too_much]).rejected, vec![(too_much, Reject::Balance)]);
    advance(&mut g, START + 3);
    g.deposit(key(0), 1_000_000_000);
    let r = g.batch(START + 4, &[too_much]);
    assert_eq!(r.rejected, vec![(too_much, Reject::Expired)]);
    assert!(r.payouts.is_empty());
    assert_eq!(g.balance(&key(0)), 1_100_000_000);
}

#[test]
fn empty_blocks_do_not_run_the_clock() {
    let p = Params::default();
    let mut g = Game::new(p.clone(), DOMAIN, 500);
    g.deposit(key(0), 100_000_000);
    g.deposit(key(1), 100_000_000);
    // Five ticks with nothing bought, then a buy good for the next three ticks.
    for h in 500..505 {
        g.batch(h, &[]);
    }
    let first = Entry::buy(&keypair(0), &DOMAIN, 1, 1, u64::MAX, 1, g.clock + 3);
    // Twenty empty L1 blocks pass: the entry does not age and the deadline does not come closer.
    for h in 505..525 {
        g.process_empty(h);
    }
    assert_eq!((g.height, g.clock, g.deadline), (524, 5, 1 + p.start_time));
    let r = g.batch(525, &[first]);
    assert_eq!((r.clock, r.accepted.len()), (6, 1));

    // Run the clock to the deadline with empty blocks mixed in: the round is still open at the deadline tick,
    // whatever the L1 height, and ends at the first block with transactions after it.
    let mut h = 526;
    while g.clock < g.deadline - 1 {
        g.process_empty(h);
        assert!(g.batch(h + 1, &[]).round_end.is_none());
        h += 2;
    }
    let last = Entry::buy(&keypair(1), &DOMAIN, 1, 1, u64::MAX, 1, g.deadline);
    let r = g.batch(h, &[last]);
    assert_eq!(r.accepted.len(), 1, "a buy at the deadline tick counts");
    assert_eq!(g.deadline, g.clock + p.extension);
    for i in 1..=30 {
        g.process_empty(h + i);
    }
    assert_eq!(g.round, 1, "a streak of empty blocks cannot end the round");
    let deadline = g.deadline;
    let mut h = h + 31;
    while g.clock < deadline {
        g.batch(h, &[]);
        h += 1;
    }
    let end = g.batch(h, &[]).round_end.unwrap();
    assert_eq!(end.winners.iter().map(|w| w.0).collect::<Vec<_>>(), vec![key(1)]);
    assert_eq!(g.deadline, g.clock + p.start_time);
}
