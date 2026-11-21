# Sidepot

A last-block-wins game on an eCash drivechain (BIP300/301). Moves are posted on L1 and every sidechain block must
process them, so sidechain block producers cannot censor them. Spec: <https://szats.world/sidepot> (draft v0.8);
the slot 88 proposal was mined on eCash mainnet in block 982,723.

## Layout

- `crates/sidepot-core`: the game rules, pure and deterministic. `Entry` is an L1 entry (BUY 128 bytes, CASHOUT
  142; codec and BIP340 signature); `Game` is the consensus state machine: `process_batch` for the entries of one
  L1 block, `on_bmm` for the streak bonus, `deposit`/`withdraw` for the game-account ledger. `Fork` (`fork.rs`)
  holds the rules that are written but not active: `process_batch` is told on every call from which L1 height
  they apply. It is not part of the state, so states below that height serialize, and hash, exactly as before.
- `crates/sidepot-cli`: test tool. `buy` and `cashout` sign entries; `scan` replays the game from L1 blocks over
  a node's RPC, crediting BIP300 deposits addressed `spot:<player key>` to game accounts.
- `scripts/regtest_miner.py`: mines regtest blocks from the enforcer's `getblocktemplate`, so they carry its
  M1/M2/BMM messages.
- `scripts/regtest_e2e.sh`: deposits into game accounts, posts signed buys and a cashout in L1 transactions, mines
  them and replays the game.

## The pending fork

Not active on any network, and the spec does not have it yet. From an L1 height the node is started with, a block
with a buy steps the price by the blocks left on the round's clock instead of the flat 1%: 10% with 6 or fewer
left (the final stretch), and before that a ramp from 1% with all 144 left towards 3%,
`10,000 + 20,000 × (144 − left) / 138` ppm rounded down (29,855 ppm with 7 left). `left` is `deadline − clock`
with the clock counting the block and the deadline not yet moved by the block's own buys: one less than the
state before the block shows. The three numbers are `PriceSteps::PENDING` in `fork.rs`. `Game::next_buy` gives
the step the next block would apply; `sidepot-cli price-table` shows what the steps do to the price.

## Choices the spec leaves open

These are what `sidepot-core` does; the spec should state them.

- **Several entries from one player in one batch** are taken in nonce order (ties broken by the entry bytes), each
  checked against what is left of the balance. The outcome never depends on transaction order in the L1 block.
- **Dividends of a round's first buys**, when nobody holds keys yet, go to the pot.
- **Rounding**: each batch splits its total spend (not each buy) into pot, dividend and streak shares; the pot takes
  the remainder. Sub-sat accumulator dust returns to the pot when the round ends.
- **Entries from a player with no game account** are rejected without creating an account, so signed spam leaves
  no state behind.
- **Game balances** are an account ledger keyed by the player's x-only key, filled by BIP300 deposits addressed
  `spot:<key>` (credited before the same block's batch) and emptied by CASHOUT entries (amount 0 = everything),
  which the sidechain block pays out as ordinary coins. Both work from L1 alone, so no producer can trap funds.

## Regtest stack

An eCash 1.2.0 node in regtest (config in `ecash.conf`, not `bitcoin.conf`) and enforcer v1.0.1 with the block
template server, both on localhost. On regtest the enforcer's activation threshold is short: the slot 88 Sidepot
proposal mined at block 105 was active at block 111. The enforcer wallet syncs from Fulcrum 2.1.4 on
127.0.0.1:60401, behind a relay that presents the node as `/Satoshi:` so Fulcrum treats it as segwit BTC.

## Test

```
cargo test --release
```

The tests cover the codec and signatures, dividends going only to earlier holders, order independence under
shuffled batches, nonce and balance rules, deadline extension and cap, the pro-rata jackpot, the streak-bonus caps,
and exact conservation of sats over a randomised multi-round run.
