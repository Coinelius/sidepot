//! The pending fork: the price step by blocks left, its activation height, and proof that it leaves every state
//! before that height exactly as it was.

mod common;

use common::{chained, hex, keypair, DOMAIN, END, START};
use sidepot_core::{game::step_price, Entry, Fork, Game, Params, PriceSteps, Reject};

/// Produced by the crate at rev 31937a5 (before any fork code) running tests/common/mod.rs: see that file.
const PINNED_CHAINED: [(u32, &str); 7] = [
    (199, "985fc17ff699fa4959fce73a4c280f5957e69a93c76dcdba5b23d3f221ee3b4b"),
    (399, "641d020c2c318341a1d3d48085334922ebf60326311ef16bf8aa97f4b93bff12"),
    (599, "c6020ca43f3b40ef3321801cc0cc8f56ce3a58ff3c5bbfa8696d8ff0f0b1f0cf"),
    (699, "dd82f0b95ff4a9d17ba031a814bb035f5cf00b7ad2934891ff01cf03cc6a9b46"),
    (700, "f368e970289e3489ad0174883e30c57dff589444a9b66ce700ecf6352c9e14b3"),
    (899, "3ad0848573478b0fe071c6b14aa281f9edc826e4f1de056c69dc52db9816d3cd"),
    (1099, "e66317a0903af8f4f16b1a370844f626fb77c203867479c9775ee8466f7d74e9"),
];
const PINNED_FINAL_LEN: usize = 701;
const PINNED_FINAL_HASH: &str = "2f24d5a9aa2415c29675906649ec68b5f9118fd2850d410549e2448229275510";
const PINNED_FINAL_PRICE: u64 = 1_694_435;
/// (blocks, blocks with a buy, rounds ended) of the script, so the pins are known to cover something.
const PINNED_SHAPE: (usize, usize, usize) = (1000, 209, 5);

/// An activation height inside the script: L1 block 700 holds a buy, in a round that began at block 656 and had
/// its first buy at 667.
const ACTIVATION: u32 = 700;

fn script(fork: Fork) -> Vec<common::Step> {
    common::run(|g, height, entries| g.process_batch(height, entries, &fork))
}

/// The rule as the owner stated it, written out again here rather than taken from the crate.
fn expected_ppm(left: u32) -> u64 {
    if left <= 6 {
        100_000
    } else {
        10_000 + 20_000 * (144 - u64::from(left.min(144))) / (144 - 6)
    }
}

fn rich_game() -> (Game, [u8; 32]) {
    let mut g = Game::new(Params::default(), DOMAIN, 1);
    let player = keypair(0).x_only_public_key().0.serialize();
    g.deposit(player, 1 << 50);
    (g, player)
}

fn buy(g: &Game, nonce: u64) -> Entry {
    let round = if g.clock + 1 > g.deadline { g.round + 1 } else { g.round };
    Entry::buy(&keypair(0), &DOMAIN, round, 1, u64::MAX, nonce, u32::MAX)
}

/// Blocks with a transaction but no entries, until the state shows `left_shown` blocks left.
fn run_down(g: &mut Game, fork: &Fork, left_shown: u32) {
    while g.deadline - g.clock > left_shown {
        g.process_batch(g.height + 1, &[], fork);
    }
}

#[test]
fn the_three_numbers() {
    assert_eq!(PriceSteps::PENDING, PriceSteps { ramp_start_ppm: 10_000, ramp_end_ppm: 30_000, final_ppm: 100_000 });
    assert_eq!(Fork::OFF.price_height, None);
    assert_eq!(Fork::at(None), Fork::OFF);
    assert_eq!(Fork::at(Some(5)).steps, PriceSteps::PENDING);
}

/// (1) With no activation height the serialized state after every block of the script is what the crate
/// produced before the fork code existed.
#[test]
fn without_a_fork_height_every_state_is_as_before() {
    let steps = script(Fork::OFF);
    let shape =
        (steps.len(), steps.iter().filter(|s| s.bought).count(), steps.iter().filter(|s| s.round_ended).count());
    assert_eq!(shape, PINNED_SHAPE);
    for (height, pinned) in PINNED_CHAINED {
        assert_eq!(chained(&steps, height), pinned, "states up to L1 block {height}");
    }
    let last = steps.last().unwrap();
    assert_eq!(last.height, END);
    assert_eq!(last.bytes.len(), PINNED_FINAL_LEN);
    assert_eq!(hex(&common::game_hash(&bincode::deserialize(&last.bytes).unwrap())), PINNED_FINAL_HASH);
    assert_eq!(hex(blake3::hash(&last.bytes).as_bytes()), PINNED_FINAL_HASH);
    assert_eq!(last.price_after, PINNED_FINAL_PRICE);
    // A height the script never reaches is no fork either.
    let never = script(Fork::at(Some(END + 1)));
    assert!(steps.iter().zip(&never).all(|(a, b)| a.bytes == b.bytes));
}

/// (2) With an activation height, every state below it is byte-identical, and so is every state up to the
/// first block at or above it whose buy steps the price differently. (A buy in the block that opens a round has
/// a full round left, and steps 1% under both rules.) That block's state differs.
#[test]
fn below_the_activation_height_every_state_is_as_before() {
    let old = script(Fork::OFF);
    // 700: a block with a buy, mid-round. 458: in a busy final stretch. 485: a block that ends a round and
    // buys. 600: no buy until 649. START: the whole script.
    for activation in [ACTIVATION, 458, 485, 600, START] {
        let new = script(Fork::at(Some(activation)));
        let first = old.iter().find(|s| s.height >= activation && s.bought && !s.round_ended).unwrap().height;
        assert_eq!(
            first,
            [(ACTIVATION, 700), (458, 458), (485, 487), (600, 649), (START, 110)]
                .iter()
                .find(|(a, _)| *a == activation)
                .unwrap()
                .1
        );
        for (a, b) in old.iter().zip(&new) {
            assert_eq!(a.height, b.height);
            if a.height < first {
                assert!(a.bytes == b.bytes, "activation {activation}: L1 block {} changed", a.height);
            }
            if a.height == first {
                assert!(a.bytes != b.bytes, "activation {activation}: block {first} should step differently");
                assert_eq!(a.price_before, b.price_before);
                assert_ne!(a.price_after, b.price_after);
            }
        }
        for (height, pinned) in PINNED_CHAINED {
            if height < first {
                assert_eq!(chained(&new, height), pinned, "activation {activation}: states up to {height}");
            }
        }
    }
    // The pins on either side of the activation height used below.
    let new = script(Fork::at(Some(ACTIVATION)));
    assert_eq!(chained(&new, ACTIVATION - 1), PINNED_CHAINED[3].1);
    assert_ne!(chained(&new, ACTIVATION), PINNED_CHAINED[4].1);
    assert_eq!(chained(&old, ACTIVATION), PINNED_CHAINED[4].1);
}

/// (3) From the activation height on, every block with a buy steps the price by the blocks left; below it, by 1%.
/// The round in progress at the activation height simply continues under the new rule.
/// Checks every block of the script run with `activation`; returns how many blocks with a buy came below it,
/// on the ramp, in the final stretch, and as the first block of a new round.
fn check_steps(activation: u32) -> (Vec<common::Step>, [u32; 4]) {
    let steps = script(Fork::at(Some(activation)));
    let mut seen = [0; 4];
    for s in &steps {
        // The price the block's buys paid, and the blocks left as the block was counted: one less than the
        // state before it showed, or a full round if the block ended one.
        let (paid, left) = if s.round_ended { (1_000_000, 144) } else { (s.price_before, s.left_shown - 1) };
        if !s.bought {
            assert_eq!(s.price_after, paid, "L1 block {}: no buy, no step", s.height);
            continue;
        }
        let ppm = if s.height < activation { 10_000 } else { expected_ppm(left) };
        let expected = (u128::from(paid) * (1_000_000 + u128::from(ppm)) / 1_000_000) as u64;
        assert_eq!(s.price_after, expected, "L1 block {}: {left} left, {ppm} ppm", s.height);
        let case = match left {
            _ if s.height < activation => 0,
            _ if s.round_ended => 3,
            0..=6 => 2,
            _ => 1,
        };
        seen[case] += 1;
    }
    (steps, seen)
}

#[test]
fn from_the_activation_height_the_price_follows_the_blocks_left() {
    // The script exercises each case.
    let (_, [before, ramp, stretch, turned]) = check_steps(400);
    assert!(before > 40 && ramp > 40 && stretch > 10 && turned >= 1, "{before} {ramp} {stretch} {turned}");
    let (_, [before, ramp, ..]) = check_steps(START);
    assert!(before == 0 && ramp > 40);
    let (steps, [before, ramp, ..]) = check_steps(ACTIVATION);
    assert!(before > 40 && ramp > 40);
    // Round 5 began at L1 block 656 and its first buy, at 667, was below the activation height: 1%.
    let s667 = steps.iter().find(|s| s.height == 667).unwrap();
    assert_eq!((s667.price_before, s667.price_after), (1_000_000, 1_010_000));
    // Its next buy, at 700, is at the activation height: the state before showed 105 left, so 104 as the block
    // is counted, 10,000 + 20,000 * 40 / 138 = 15,797 ppm.
    let s700 = steps.iter().find(|s| s.height == 700).unwrap();
    assert!(!s700.round_ended);
    assert_eq!((s700.left_shown, s700.price_before, s700.price_after), (105, 1_010_000, 1_025_954));
}

#[test]
fn step_by_blocks_left() {
    let p = Params::default();
    let fork = Fork::at(Some(1000));
    for (left, ppm) in [
        (144, 10_000),
        (143, 10_144),
        (142, 10_289),
        (76, 19_855),
        (75, 20_000),
        (74, 20_144),
        (8, 29_710),
        (7, 29_855),
        (6, 100_000),
        (5, 100_000),
        (1, 100_000),
        (0, 100_000),
        // More than a round left cannot happen with these parameters; it counts as a full round.
        (145, 10_000),
        (u32::MAX, 10_000),
    ] {
        assert_eq!(fork.step_ppm(&p, 1000, left), ppm, "{left} left");
        assert_eq!(fork.step_ppm(&p, u32::MAX, left), ppm);
        assert_eq!(ppm, expected_ppm(left));
        // Below the activation height, and with no fork, the flat step whatever is left.
        assert_eq!(fork.step_ppm(&p, 999, left), 10_000);
        assert_eq!(Fork::OFF.step_ppm(&p, 1000, left), 10_000);
    }
    // The ramp never falls as the clock runs down and never reaches its end value.
    for left in 7..144 {
        let (here, earlier) = (fork.step_ppm(&p, 1000, left), fork.step_ppm(&p, 1000, left + 1));
        assert!(earlier <= here && here < 30_000);
    }
    // The flat step is the parameter, not the ramp's start.
    let other = Params { price_step_ppm: 12_345, ..Params::default() };
    assert_eq!(fork.step_ppm(&other, 999, 3), 12_345);
    assert_eq!(fork.step_ppm(&other, 1000, 144), 10_000);
    // Other numbers, including a ramp that falls.
    let steps = PriceSteps { ramp_start_ppm: 50_000, ramp_end_ppm: 4_000, final_ppm: 1 };
    let odd = Fork { price_height: Some(0), steps };
    assert_eq!(odd.step_ppm(&p, 0, 144), 50_000);
    assert_eq!(odd.step_ppm(&p, 0, 75), 27_000);
    assert_eq!(odd.step_ppm(&p, 0, 7), 50_000 - 46_000 * 137 / 138);
    assert_eq!(odd.step_ppm(&p, 0, 6), 1);
}

/// Parameters with no room for a ramp, or none at all, divide nothing by zero.
#[test]
fn degenerate_parameters() {
    let fork = Fork::at(Some(0));
    for start_time in 0..12u32 {
        for extension in 0..12u32 {
            let p = Params { start_time, extension, max_time: 20, ..Params::default() };
            assert!(p.validate().is_ok());
            for left in 0..=21 {
                let ppm = fork.step_ppm(&p, 0, left);
                if left <= extension {
                    assert_eq!(ppm, 100_000, "{start_time} {extension} {left}");
                } else if start_time <= extension || left >= start_time {
                    // No ramp, or more than a full round left (max_time > start_time allows both).
                    assert_eq!(ppm, 10_000, "{start_time} {extension} {left}");
                } else {
                    assert!((10_000..30_000).contains(&ppm), "{start_time} {extension} {left}");
                }
            }
        }
    }
    // start_time == extension: a round opens in its final stretch, and a game runs.
    let p = Params { start_time: 6, extension: 6, max_time: 6, ..Params::default() };
    let mut g = Game::new(p, DOMAIN, 1);
    g.deposit(keypair(0).x_only_public_key().0.serialize(), 1 << 40);
    let r = g.process_batch(1, &[buy(&g, 1)], &fork);
    assert_eq!((r.left, r.accepted.len(), g.price), (6, 1, 1_100_000));
    // Extreme numbers saturate rather than wrap.
    let steps = PriceSteps { ramp_start_ppm: 0, ramp_end_ppm: u64::MAX, final_ppm: u64::MAX };
    let huge = Fork { price_height: Some(0), steps };
    let p = Params { start_time: u32::MAX, extension: 0, max_time: u32::MAX, ..Params::default() };
    assert_eq!(huge.step_ppm(&p, 0, 1), u64::MAX / u64::from(u32::MAX) * u64::from(u32::MAX - 1));
    assert_eq!(step_price(1_000_000, u64::MAX), u64::MAX);
}

/// `left` in a running game: what the state before a block shows, less one for the block's own tick, with the
/// deadline taken before the block's buys extend it.
#[test]
fn left_is_counted_before_the_block_extends_the_deadline() {
    let fork = Fork::at(Some(1));
    let (mut g, _) = rich_game();
    // The first block of round 1: a full round left, 1%.
    assert_eq!((g.deadline, g.clock), (145, 0));
    let next = g.next_buy(&fork);
    assert_eq!((next.left, next.step_ppm, next.price, next.price_after), (144, 10_000, 1_000_000, 1_010_000));
    let r = g.process_batch(1, &[buy(&g, 1)], &fork);
    assert_eq!((r.left, r.price, g.price), (144, 1_000_000, 1_010_000));
    // Far from the deadline a buy does not move it.
    assert_eq!((g.deadline, g.clock), (145, 1));

    // The state shows 8 left; the next block counts 7: the last and largest step of the ramp.
    run_down(&mut g, &fork, 8);
    assert_eq!(g.next_buy(&fork).step_ppm, 29_855);
    let r = g.process_batch(g.height + 1, &[buy(&g, 2)], &fork);
    assert_eq!((r.left, g.price), (7, step_price(1_010_000, 29_855)));
    assert_eq!(g.price, 1_040_153);
    // Still not extended: 7 left was more than the extension.
    assert_eq!(g.deadline - g.clock, 7);

    // The state shows 7 left; the next block counts 6: the final stretch, 10%, and the buy now extends the
    // deadline to 6 past its block. The step used the 6 left before that, and would have been the same after.
    let next = g.next_buy(&fork);
    assert_eq!((next.left, next.step_ppm), (6, 100_000));
    let deadline = g.deadline;
    let r = g.process_batch(g.height + 1, &[buy(&g, 3)], &fork);
    assert_eq!((r.left, g.price), (6, 1_144_168));
    assert_eq!(g.deadline, deadline);
    assert_eq!(g.deadline - g.clock, 6);

    // The state shows 6 left, the next block counts 5 and extends the deadline by one block.
    let r = g.process_batch(g.height + 1, &[buy(&g, 4)], &fork);
    assert_eq!((r.left, g.price), (5, 1_258_584));
    assert_eq!((g.deadline, g.deadline - g.clock), (deadline + 1, 6));

    // The last block of the round: the state shows 1 left, the block counts 0, and its buy still counts.
    run_down(&mut g, &fork, 1);
    let next = g.next_buy(&fork);
    assert_eq!((next.round, next.left, next.step_ppm), (1, 0, 100_000));
    let r = g.process_batch(g.height + 1, &[buy(&g, 5)], &fork);
    assert_eq!((r.left, r.accepted.len(), r.round_end.is_none()), (0, 1, true));
    assert_eq!(g.price, 1_384_442);
    assert_eq!(g.deadline - g.clock, 6);

    // The state shows 0 left: the next counted block ends the round before reading its entries, so a buy in it
    // is the first of the next round, at the start price with a full round left. Nothing is ever bought with a
    // negative number of blocks left.
    run_down(&mut g, &fork, 0);
    let next = g.next_buy(&fork);
    assert_eq!((next.round, next.left, next.step_ppm, next.price), (2, 144, 10_000, 1_000_000));
    let stale = Entry::buy(&keypair(0), &DOMAIN, 1, 1, u64::MAX, 6, u32::MAX);
    let r = g.process_batch(g.height + 1, &[stale, buy(&g, 7)], &fork);
    assert_eq!(r.rejected, vec![(stale, Reject::WrongRound)]);
    assert_eq!((r.round_end.unwrap().round, r.left, r.price, g.price), (1, 144, 1_000_000, 1_010_000));
    assert_eq!((g.round, g.deadline - g.clock), (2, 144));
}

/// Empty L1 blocks do not tick the clock, so they do not change the blocks left; only their height counts,
/// for whether the fork is active.
#[test]
fn empty_blocks_do_not_change_the_step() {
    let fork = Fork::at(Some(20));
    let (mut g, _) = rich_game();
    run_down(&mut g, &fork, 135);
    assert_eq!((g.height, g.clock), (10, 10));
    // Below the activation height the next buy steps 1%.
    assert_eq!(g.next_buy(&fork).step_ppm, 10_000);
    for h in 11..=19 {
        g.process_empty(h);
        assert_eq!(g.next_buy(&fork).left, 134);
    }
    // The next block is L1 block 20: active, with 134 left as before the empty blocks.
    let next = g.next_buy(&fork);
    assert_eq!((next.left, next.step_ppm), (134, 10_000 + 20_000 * 10 / 138));
    let r = g.process_batch(20, &[buy(&g, 1)], &fork);
    assert_eq!((r.left, g.price), (134, 1_011_449));
}

/// What `next_buy` says before a block is what the block then does, at every block of the script.
#[test]
fn next_buy_agrees_with_the_next_block() {
    for fork in [Fork::OFF, Fork::at(Some(ACTIVATION)), Fork::at(Some(START))] {
        let mut checked = 0;
        common::run(|g, height, entries| {
            let next = g.next_buy(&fork);
            let report = g.process_batch(height, entries, &fork);
            assert_eq!((next.round, next.price, next.left), (g.round, report.price, report.left));
            assert_eq!(next.step_ppm, fork.step_ppm(&g.params, height, report.left));
            assert_eq!(next.price_after, step_price(next.price, next.step_ppm));
            if report.accepted.iter().any(|e| matches!(e.action, sidepot_core::Action::Buy { .. })) {
                assert_eq!(g.price, next.price_after, "L1 block {height}");
                checked += 1;
            } else {
                assert_eq!(g.price, next.price);
            }
            report
        });
        assert!(checked > 150);
    }
}

/// At very high prices the step saturates at u64::MAX, as it always has, and stays there.
#[test]
fn the_price_saturates() {
    let fork = Fork::at(Some(1));
    let (mut g, player) = rich_game();
    g.deposit(player, u64::MAX - (1 << 50));
    run_down(&mut g, &fork, 7);
    g.price = u64::MAX / 11 * 10 + 10;
    let r = g.process_batch(g.height + 1, &[buy(&g, 1)], &fork);
    assert_eq!((r.left, r.accepted.len()), (6, 1));
    assert_eq!(g.price, u64::MAX);
    // Nobody can pay it: one key costs more than is left, two overflow.
    let two = Entry::buy(&keypair(0), &DOMAIN, 1, 2, u64::MAX, 3, u32::MAX);
    let r = g.process_batch(g.height + 1, &[buy(&g, 2), two], &fork);
    assert_eq!(r.rejected.iter().map(|(_, why)| *why).collect::<Vec<_>>(), vec![Reject::Balance, Reject::Balance]);
    assert_eq!(g.price, u64::MAX);
    assert_eq!(step_price(u64::MAX, 100_000), u64::MAX);
}
