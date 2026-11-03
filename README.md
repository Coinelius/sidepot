# Sidepot

A last-block-wins game on an eCash drivechain (BIP300/301). Moves are posted on L1 and every sidechain block must
process them, so sidechain block producers cannot censor them. Spec: <https://szats.world/sidepot> (draft v0.2);
the slot 88 proposal was mined on eCash mainnet in block 982,723.

## Layout

- `crates/sidepot-core`: the game rules, pure and deterministic. `Entry` is the 124-byte L1 entry (codec and
  BIP340 signature); `Game` is the consensus state machine: `process_batch` for the entries of one L1 block,
  `on_bmm` for the streak bonus, `deposit`/`withdraw` for the game-account ledger.

## Choices the spec leaves open

These are what `sidepot-core` does; the spec should state them.

- **Several entries from one player in one batch** are taken in nonce order (ties broken by the entry bytes), each
  checked against what is left of the balance. The outcome never depends on transaction order in the L1 block.
- **Dividends of a round's first buys**, when nobody holds keys yet, go to the pot.
- **Rounding**: each batch splits its total spend (not each buy) into pot, dividend and streak shares; the pot takes
  the remainder. Sub-sat accumulator dust returns to the pot when the round ends.
- **Entries from a player with no game account** are rejected without creating an account, so signed spam leaves
  no state behind.
- **Game balances** are an account ledger keyed by the player's x-only key. How native Thunder transactions move
  sECX into and out of it is decided at the node integration.

## Test

```
cargo test --release
```

The tests cover the codec and signatures, dividends going only to earlier holders, order independence under
shuffled batches, nonce and balance rules, deadline extension and cap, the pro-rata jackpot, the streak-bonus caps,
and exact conservation of sats over a randomised multi-round run.
