use secp256k1::{Keypair, SECP256K1};
use sidepot_core::{entry, Bmm, DecodeError, Entry, Game, Params, PlayerKey, Reject};

const START: u32 = 1000;

fn keypair(i: u8) -> Keypair {
    Keypair::from_seckey_slice(SECP256K1, &[i + 1; 32]).unwrap()
}

fn key(i: u8) -> PlayerKey {
    keypair(i).x_only_public_key().0.serialize()
}

fn buy(i: u8, round: u32, keys: u16, nonce: u64) -> Entry {
    Entry::sign(&keypair(i), round, keys, u64::MAX, nonce)
}

/// Processes empty batches up to and including `height`.
fn advance(g: &mut Game, height: u32) {
    while g.height < height {
        g.process_batch(g.height + 1, &[]);
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
    let e = Entry::sign(&keypair(0), 7, 3, 1_500_000, 42);
    assert!(e.verify());
    let bytes = e.encode();
    assert_eq!(bytes.len(), entry::ENTRY_LEN);
    assert_eq!(&bytes[0..6], b"SPOT\x00\x01");
    assert_eq!(Entry::decode(&bytes), Ok(e));
    assert_eq!(Entry::from_script(&e.script()), Some(Ok(e)));

    let mut tampered = e;
    tampered.keys = 4;
    assert!(!tampered.verify(), "the signature covers the key count");
    let mut wrong_player = e;
    wrong_player.player = key(1);
    assert!(!wrong_player.verify());

    let mut v1 = bytes;
    v1[4] = 1;
    assert_eq!(Entry::decode(&v1), Err(DecodeError::Version(1)));
    let mut sell = bytes;
    sell[5] = 2;
    assert_eq!(Entry::decode(&sell), Err(DecodeError::Kind(2)));
    assert_eq!(Entry::decode(&bytes[..123]), Err(DecodeError::Length(123)));
    // Other data outputs are not entries at all.
    assert_eq!(Entry::from_script(&[0x6a, 0x04, b'S', b'P', b'O', b'T']), None);
    let mut other = e.script();
    other[3] = b'X';
    assert_eq!(Entry::from_script(&other), None);
}

#[test]
fn dividends_go_only_to_earlier_holders() {
    let mut g = Game::new(Params::default(), START);
    let (a, b) = (key(0), key(1));
    g.deposit(a, 100_000_000);
    g.deposit(b, 100_000_000);

    // A buys first: nobody holds keys yet, so the dividend share stays in the pot.
    let r = g.process_batch(START, &[buy(0, 1, 10, 1)]);
    assert_eq!(r.accepted.len(), 1);
    let a_cost = 10 * 1_000_000;
    assert_eq!(g.balance(&a), 100_000_000 - a_cost);
    assert_eq!(g.pot, a_cost * 85 / 100);
    assert_eq!(g.streak_pool, a_cost * 15 / 100);

    // B buys next block at the stepped price; A earns 25% of it, B earns nothing from its own buy.
    let r = g.process_batch(START + 1, &[buy(1, 1, 5, 1)]);
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
        let mut g1 = Game::new(Params::default(), START);
        for p in 0..4 {
            g1.deposit(key(p), 20_000_000 + rng.below(40_000_000));
        }
        let mut g2 = g1.clone();
        let r1 = g1.process_batch(START, &entries);
        for i in (1..entries.len()).rev() {
            entries.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let r2 = g2.process_batch(START, &entries);
        assert_eq!(g1, g2, "trial {trial}: state depends on order");
        assert_eq!(r1.accepted.len(), r2.accepted.len());
    }
}

#[test]
fn same_player_entries_are_taken_in_nonce_order() {
    let mut g = Game::new(Params::default(), START);
    g.deposit(key(0), 15_000_000);
    // Each costs 10M at the batch price; only the lower nonce fits, whichever comes first in the block.
    let r = g.process_batch(START, &[buy(0, 1, 10, 9), buy(0, 1, 10, 3)]);
    assert_eq!(r.accepted.iter().map(|e| e.nonce).collect::<Vec<_>>(), vec![3]);
    assert_eq!(r.rejected[0].0.nonce, 9);
    assert_eq!(r.rejected[0].1, Reject::Balance);
    // The rejected entry did not use up its nonce.
    g.deposit(key(0), 50_000_000);
    let r = g.process_batch(START + 1, &[buy(0, 1, 10, 9)]);
    assert_eq!(r.accepted.len(), 1);
}

#[test]
fn replays_and_bad_entries_are_ignored() {
    let mut g = Game::new(Params::default(), START);
    g.deposit(key(0), 1_000_000_000);
    let e = buy(0, 1, 1, 5);
    assert_eq!(g.process_batch(START, &[e, e]).accepted.len(), 1, "a duplicate in one block counts once");
    assert_eq!(g.process_batch(START + 1, &[e]).rejected[0].1, Reject::Nonce);

    let price = g.price;
    let cheap = Entry::sign(&keypair(0), 1, 1, price - 1, 6);
    assert_eq!(g.process_batch(START + 2, &[cheap]).rejected[0].1, Reject::Price);
    assert_eq!(g.process_batch(START + 3, &[buy(0, 2, 1, 7)]).rejected[0].1, Reject::WrongRound);
    assert_eq!(g.process_batch(START + 4, &[buy(0, 1, 1001, 8)]).rejected[0].1, Reject::Keys);
    let mut forged = buy(0, 1, 1, 9);
    forged.sig[0] ^= 1;
    assert_eq!(g.process_batch(START + 5, &[forged]).rejected[0].1, Reject::Signature);
    let r = g.process_batch(START + 6, &[buy(9, 1, 1, 1)]);
    assert_eq!(r.rejected[0].1, Reject::Balance, "no account, nothing to spend");
    assert!(!g.accounts.contains_key(&key(9)), "rejected entries create no account");
}

#[test]
fn deadline_extends_and_is_capped() {
    let p = Params::default();
    let mut g = Game::new(p.clone(), START);
    g.deposit(key(0), u64::MAX / 4);
    assert_eq!(g.deadline, START + 144);

    // An early buy leaves the deadline where it is.
    g.process_batch(START, &[buy(0, 1, 1, 1)]);
    assert_eq!(g.deadline, START + 144);

    // A buy close to the deadline pushes it to EXTENSION blocks past its block.
    advance(&mut g, START + 141);
    g.process_batch(START + 142, &[buy(0, 1, 1, 2)]);
    assert_eq!(g.deadline, START + 148);

    // A buy exactly at the deadline still counts.
    advance(&mut g, START + 147);
    let r = g.process_batch(START + 148, &[buy(0, 1, 1, 3)]);
    assert_eq!(r.accepted.len(), 1);
    assert_eq!(g.deadline, START + 154);

    // The first batch past the deadline ends the round before it is read.
    advance(&mut g, START + 154);
    let r = g.process_batch(START + 155, &[buy(0, 1, 1, 4)]);
    let end = r.round_end.expect("round ended");
    assert_eq!(end.final_height, Some(START + 148));
    assert_eq!(r.rejected[0].1, Reject::WrongRound);
    assert_eq!(g.round, 2);
    assert_eq!(g.deadline, START + 155 + 144);
    assert_eq!(g.price, p.start_price);
}

#[test]
fn final_block_buyers_split_the_jackpot_pro_rata() {
    let mut g = Game::new(Params::default(), START);
    for p in 0..3 {
        g.deposit(key(p), 1_000_000_000);
    }
    g.process_batch(START, &[buy(0, 1, 50, 1)]);
    advance(&mut g, START + 9);
    g.process_batch(START + 10, &[buy(1, 1, 10, 1), buy(2, 1, 20, 1)]);
    advance(&mut g, START + 144);
    let before: Vec<u64> = (0..3).map(|p| g.balance(&key(p))).collect();
    let pot = g.pot;

    let end = g.process_batch(START + 145, &[]).round_end.expect("round ended");
    assert_eq!(end.final_height, Some(START + 10));
    let paid: Vec<u64> = (0..3).map(|p| g.balance(&key(p)) - before[p as usize]).collect();
    assert_eq!(paid[0], 0, "earlier buyers do not share the jackpot");
    // Half the pot (plus a few sats of returned dividend rounding), split 10:20 by keys bought in the final block.
    assert!((paid[1] + paid[2]).abs_diff(pot / 2) <= 2);
    assert!(paid[2].abs_diff(2 * paid[1]) <= 1);
    assert_eq!(end.carried, g.pot);
    assert!(g.accounts.values().all(|a| a.keys == 0));
}

#[test]
fn a_round_without_buys_carries_the_pot() {
    let mut g = Game::new(Params::default(), START);
    advance(&mut g, START + 144);
    let end = g.process_batch(START + 145, &[]).round_end.expect("round ended");
    assert_eq!(end.final_height, None);
    assert!(end.winners.is_empty());
    assert_eq!(g.round, 2);
}

#[test]
fn price_steps_per_key_bought() {
    let mut g = Game::new(Params::default(), START);
    g.deposit(key(0), u64::MAX / 4);
    g.process_batch(START, &[buy(0, 1, 10, 1)]);
    let mut expected = 1_000_000u64;
    for _ in 0..10 {
        expected = expected * 1_005_000 / 1_000_000;
    }
    assert_eq!(g.price, expected);
}

#[test]
fn streak_bonus_is_capped_by_bid_cap_and_pool() {
    let mut g = Game::new(Params::default(), START);
    let x = key(5);
    g.streak_pool = 1_000_000_000;

    // A big bid lets the bonus double up to BONUS_CAP.
    let big = Some(Bmm { producer: x, bid: 1_000_000 });
    let paid: Vec<u64> = (0..5).map(|_| g.on_bmm(big)).collect();
    assert_eq!(paid, vec![100_000, 200_000, 400_000, 800_000, 800_000]);

    // A small qualifying bid caps the bonus at BID_MULTIPLE times the bid.
    assert_eq!(g.on_bmm(Some(Bmm { producer: x, bid: 20_000 })), 40_000);
    // A bid under MIN_BID earns nothing and resets the streak.
    assert_eq!(g.on_bmm(Some(Bmm { producer: x, bid: 9_999 })), 0);
    assert_eq!(g.on_bmm(big), 100_000);
    // So does an L1 block without a Sidepot block, or another producer.
    g.on_bmm(None);
    assert_eq!(g.on_bmm(big), 100_000);
    assert_eq!(g.on_bmm(Some(Bmm { producer: key(6), bid: 1_000_000 })), 100_000);

    // A bonus is never more than 1% of the pool.
    g.streak_pool = 5_000_000;
    assert_eq!(g.on_bmm(Some(Bmm { producer: key(7), bid: 1_000_000 })), 50_000);
}

#[test]
fn sats_are_conserved_across_rounds() {
    let mut rng = Rng(42);
    let params = Params { start_time: 20, max_time: 20, ..Params::default() };
    let mut g = Game::new(params, START);
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
        let entries: Vec<Entry> = (0..rng.below(4))
            .map(|_| {
                let p = rng.below(6) as usize;
                nonces[p] += 1 + rng.below(2);
                buy(p as u8, g.round, 1 + rng.below(30) as u16, nonces[p])
            })
            .collect();
        let r = g.process_batch(h, &entries);
        if r.round_end.is_some_and(|e| !e.winners.is_empty()) {
            rounds_with_winner += 1;
        }
        let producer = key(rng.below(3) as u8);
        g.on_bmm((rng.below(3) > 0).then_some(Bmm { producer, bid: 5_000 + rng.below(100_000) }));
        assert_eq!(g.total(), net, "sats created or lost at height {h}");
    }
    assert!(rounds_with_winner >= 3, "the simulation should cover several finished rounds");
}
