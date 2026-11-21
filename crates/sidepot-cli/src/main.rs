//! Sidepot test tool.
//!
//!     sidepot-cli pubkey <seckey-hex>
//!     sidepot-cli buy <seckey-hex> <anchor-hash> <slot> <round> <keys> <max-price> <nonce> <expires>
//!     sidepot-cli cashout <seckey-hex> <anchor-hash> <slot> <sats, 0 = all> <dest-hex-20-bytes> <nonce> <expires>
//!     sidepot-cli scan <rpc-url> <cookie-path> <slot> <anchor-height> <start-height> [<player-hex>:<sats> ...]
//!     sidepot-cli follow <rpc-url> <cookie-path> <slot> <anchor-height> <start-height>
//!     sidepot-cli tick <rpc-url> <cookie-path> <start-height>
//!     sidepot-cli price-table [<buy-blocks> ...]
//!
//! Entries are signed for one Sidepot: the slot plus an anchor, the L1 block that mined its slot proposal. `buy` and
//! `cashout` take that block's hash as `getblockhash` prints it; `scan` and `follow` take its height and look the
//! hash up. `expires` is the last tick of the game clock at which the entry counts. The clock counts the L1 blocks
//! since `start-height` that hold at least one non-coinbase transaction; `tick` prints its value at the node's tip
//! (the `tip_clock` of a `STATE` line), which is what a wallet adds its expiry window to.
//! `buy` and `cashout` print the entry as hex, ready for a `{"data": ...}` output of `createrawtransaction`.
//! `scan` replays the game from `start-height` to `depth` blocks below the node's tip and prints every block that
//! touched it. BIP300 deposits to `slot` whose address is `spot:<player-hex>` credit that game account; listed
//! players get an extra test balance first. Each L1 block's BMM (the M7 accept in its coinbase, the bid being the
//! matching M8 transaction's fee) drives the streak bonus. Until Sidepot blocks exist, the committed hash h*
//! stands in for the producer's key. SIDEPOT_QUIET=1 prints only the summary; SIDEPOT_BMM_CSV=<path> writes one
//! row per L1 block; SIDEPOT_BONUS_RAMP=<n> overrides the bonus ramp (0 = the full cap from a streak's first
//! block). The last line is `STATE <json>`, the game after the last processed block. `follow` does the same
//! incrementally: each line on stdin advances to the current tip and prints a `STATE` line.
//! SIDEPOT_FORK_HEIGHT=<L1 height> replays with the pending fork's price rule from that height (unset: no fork).
//! `price-table` runs the game itself to show what the pending fork's price steps do to the key price: after N
//! L1 blocks with a buy (default 10 20 30 50 100), under today's flat step, under the fork with the buys spread
//! evenly over a round before its final stretch, and under the fork with every buy in the final stretch.

use std::collections::BTreeMap;
use std::io::Write;

use std::process::exit;

use base64::Engine;
use secp256k1::{Keypair, SECP256K1};
use serde_json::{json, Value};
use sidepot_core::{deposit_player, Bmm, Domain, Entry, Fork, Game, Params, PlayerKey};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["pubkey", sk] => keypair(sk).map(|kp| println!("{}", hex(&kp.x_only_public_key().0.serialize()))),
        ["buy", sk, anchor, slot, round, keys, max_price, nonce, expires] => (|| {
            let (kp, d) = (keypair(sk)?, domain(anchor, slot)?);
            let e = Entry::buy(&kp, &d, num(round)?, num(keys)?, num(max_price)?, num(nonce)?, num(expires)?);
            println!("{}", hex(&e.encode()));
            Ok(())
        })(),
        ["cashout", sk, anchor, slot, amount, dest, nonce, expires] => (|| {
            let dest = unhex(dest)?.try_into().map_err(|_| "a sidechain address is 20 bytes")?;
            let (kp, d) = (keypair(sk)?, domain(anchor, slot)?);
            let e = Entry::cashout(&kp, &d, num(amount)?, dest, num(nonce)?, num(expires)?);
            println!("{}", hex(&e.encode()));
            Ok(())
        })(),
        ["scan", url, cookie, slot, anchor, start, funds @ ..] => {
            (|| scan(url, cookie, num(slot)?, num(anchor)?, num(start)?, funds))()
        }
        ["follow", url, cookie, slot, anchor, start] => {
            (|| follow(url, cookie, num(slot)?, num(anchor)?, num(start)?))()
        }
        ["tick", url, cookie, start] => (|| {
            let rpc = Rpc::new(url, cookie)?;
            let tip = rpc.call("getblockcount", json!([]))?.as_u64().ok_or("bad getblockcount")? as u32;
            println!("{}", ticks(&rpc, num(start)?, tip)?);
            Ok(())
        })(),
        ["price-table", counts @ ..] => (|| {
            let counts: Vec<u32> = counts.iter().map(|n| num(n)).collect::<Result<_, _>>()?;
            price_table(if counts.is_empty() { &[10, 20, 30, 50, 100] } else { &counts })
        })(),
        _ => Err("usage: see the doc comment in crates/sidepot-cli/src/main.rs".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        exit(1);
    }
}

/// One row per N: the price after N blocks with a buy and what one key in each of those blocks cost in total,
/// in three games played out with the rules crate from the start of a round.
fn price_table(counts: &[u32]) -> Result<(), String> {
    let params = Params::default();
    let (start_time, extension) = (params.start_time, params.extension);
    // Blocks of a round that are not in the final stretch: those with more than `extension` blocks left.
    let before_stretch = start_time - extension;
    let domain = Domain { anchor: [0; 32], slot: 0 };
    let kp = Keypair::from_seckey_slice(SECP256K1, &[1; 32]).map_err(|e| e.to_string())?;
    // Plays a round from its first block: `buy_at(i)` is the clock tick of the i-th buy block. Returns the price
    // after the last buy, the total paid for one key per buy block, and the blocks left at the first and last buy.
    let play = |fork: Fork, n: u32, buy_at: &dyn Fn(u32) -> u32| -> Result<(u64, u64, u32, u32), String> {
        let mut game = Game::new(params.clone(), domain, 1);
        game.deposit(kp.x_only_public_key().0.serialize(), u64::MAX);
        let (mut paid, mut lefts) = (0u64, Vec::new());
        for i in 0..n {
            while game.clock + 1 < buy_at(i) {
                game.process_batch(game.height + 1, &[], &fork);
            }
            let entry = Entry::buy(&kp, &domain, 1, 1, u64::MAX, u64::from(i) + 1, u32::MAX);
            let report = game.process_batch(game.height + 1, &[entry], &fork);
            if report.accepted.len() != 1 || report.round_end.is_some() || report.clock != buy_at(i) {
                return Err(format!("buy {i} of {n} did not go as planned: {report:?}"));
            }
            paid += report.price;
            lefts.push(report.left);
        }
        Ok((game.price, paid, lefts[0], lefts[n as usize - 1]))
    };
    let fork = Fork::at(Some(1));
    println!(
        "start price {} sats; flat step {} ppm; fork steps {:?}",
        params.start_price, params.price_step_ppm, fork.steps
    );
    println!("round {start_time} blocks, final stretch {extension} blocks or fewer left");
    println!("N | flat: price, total | spread before the stretch: price, total (blocks left first..last) | all in the stretch: price, total");
    for &n in counts {
        if n == 0 || n > before_stretch {
            return Err(format!("{n} buy blocks do not fit the {before_stretch} blocks before the final stretch"));
        }
        // Today's rule: where the buys fall does not matter. They are spread the same way as in the next column.
        let spread = |i: u32| 1 + i * before_stretch / n;
        let (flat_price, flat_paid, ..) = play(Fork::OFF, n, &spread)?;
        let (ramp_price, ramp_paid, first, last) = play(fork, n, &spread)?;
        // The round runs down untouched until `extension` blocks are left, then every block holds a buy.
        let (end_price, end_paid, end_first, end_last) = play(fork, n, &|i| before_stretch + 1 + i)?;
        if end_first > extension || end_last > extension {
            return Err("a final-stretch buy was not in the final stretch".into());
        }
        println!(
            "{n} | {flat_price}, {flat_paid} | {ramp_price}, {ramp_paid} ({first}..{last}) | {end_price}, {end_paid}"
        );
    }
    Ok(())
}

fn scan(url: &str, cookie: &str, slot: u8, anchor: u32, start: u32, funds: &[&str]) -> Result<(), String> {
    let mut scanner = Scanner::new(url, cookie, slot, anchor, start, funds)?;
    scanner.advance()?;
    scanner.summary();
    println!("STATE {}", scanner.state());
    Ok(())
}

/// Like `scan`, but keeps the game in memory: each line on stdin processes the blocks added since the last one and
/// prints `STATE <json>`. The summary follows at end of input.
fn follow(url: &str, cookie: &str, slot: u8, anchor: u32, start: u32) -> Result<(), String> {
    let mut scanner = Scanner::new(url, cookie, slot, anchor, start, &[])?;
    let stdout = std::io::stdout();
    for line in std::io::stdin().lines() {
        line.map_err(|e| e.to_string())?;
        scanner.advance()?;
        let mut out = stdout.lock();
        writeln!(out, "STATE {}", scanner.state()).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
    }
    scanner.summary();
    Ok(())
}

struct Scanner {
    rpc: Rpc,
    game: Game,
    fork: Fork,
    slot: u8,
    start: u32,
    /// The slot's treasury output: OP_DRIVECHAIN <slot> OP_TRUE. Each deposit replaces it with a larger one.
    treasury_script: [u8; 4],
    quiet: bool,
    csv: Option<std::fs::File>,
    /// Per producer: blocks won, bids paid, bonuses received.
    stats: BTreeMap<PlayerKey, (u32, u64, u64)>,
    tip: u32,
    /// The game clock once every block up to the tip is processed.
    tip_clock: u32,
}

impl Scanner {
    fn new(url: &str, cookie: &str, slot: u8, anchor: u32, start: u32, funds: &[&str]) -> Result<Self, String> {
        let rpc = Rpc::new(url, cookie)?;
        let mut params = Params::default();
        if let Ok(ramp) = std::env::var("SIDEPOT_BONUS_RAMP") {
            params.bonus_ramp = num(&ramp)?;
        }
        let anchor_hash = rpc.call("getblockhash", json!([anchor]))?;
        let domain = domain(anchor_hash.as_str().ok_or("bad getblockhash")?, &slot.to_string())?;
        let mut game = Game::new(params, domain, start);
        for f in funds {
            let (player, sats) = f.split_once(':').ok_or("fund as <player-hex>:<sats>")?;
            game.deposit(player_key(player)?, num(sats)?);
        }
        let csv = match std::env::var("SIDEPOT_BMM_CSV") {
            Ok(path) => {
                let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
                writeln!(f, "height,producer,bid,bonus,streak,streak_pool,price,round,pot,entries,clock")
                    .map_err(|e| e.to_string())?;
                Some(f)
            }
            Err(_) => None,
        };
        let fork = match std::env::var("SIDEPOT_FORK_HEIGHT") {
            Ok(height) => Fork::at(Some(num(&height)?)),
            Err(_) => Fork::OFF,
        };
        let quiet = std::env::var_os("SIDEPOT_QUIET").is_some();
        let treasury_script = [0xb7, 0x01, slot, 0x51];
        Ok(Self {
            rpc,
            game,
            fork,
            slot,
            start,
            treasury_script,
            quiet,
            csv,
            stats: BTreeMap::new(),
            tip: 0,
            tip_clock: 0,
        })
    }

    /// Processes every block from the last processed one up to `depth` below the node's tip.
    fn advance(&mut self) -> Result<(), String> {
        self.tip = self.rpc.call("getblockcount", json!([]))?.as_u64().ok_or("bad getblockcount")? as u32;
        while self.game.height < self.tip.saturating_sub(self.game.params.depth) {
            self.block(self.game.height + 1)?;
        }
        self.tip_clock = self.game.clock + ticks(&self.rpc, self.game.height + 1, self.tip)?;
        Ok(())
    }

    fn block(&mut self, height: u32) -> Result<(), String> {
        let game = &mut self.game;
        let hash = self.rpc.call("getblockhash", json!([height]))?;
        let block = self.rpc.call("getblock", json!([hash, 3]))?;
        // A block with nothing but its coinbase does not tick the game clock, and cannot carry a BMM request.
        if block["tx"].as_array().is_none_or(|txs| txs.len() <= 1) {
            game.process_empty(height);
            game.on_bmm(None);
            if let Some(csv) = self.csv.as_mut() {
                let (pool, price, round, pot, clock) = (game.streak_pool, game.price, game.round, game.pot, game.clock);
                writeln!(csv, "{height},,0,0,0,{pool},{price},{round},{pot},0,{clock}").map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        // Deposits in a block are credited before its batch, so a player can deposit and buy in one block.
        for (address, sats) in deposits(&block, &self.treasury_script) {
            match deposit_player(&address) {
                Some(player) => {
                    game.deposit(player, sats);
                    println!("{height}: deposit {sats} to game account {}", short(&player));
                }
                None => println!("{height}: deposit {sats} to sidechain address {address:?} (not a game account)"),
            }
        }
        let entries = block_entries(&block);
        let report = game.process_batch(height, &entries, &self.fork);
        let bmm = block_bmm(&block, self.slot);
        let bonus = game.on_bmm(bmm);
        if let Some(b) = bmm {
            let s = self.stats.entry(b.producer).or_default();
            s.0 += 1;
            s.1 += b.bid;
            s.2 += bonus;
            if !self.quiet {
                let n = game.streak().map_or(0, |(_, n)| n);
                println!(
                    "{height}: bmm {} bid {} -> bonus {bonus} (streak {n}, pool {})",
                    short(&b.producer),
                    b.bid,
                    game.streak_pool
                );
            }
        }
        if let Some(csv) = self.csv.as_mut() {
            let (producer, bid) = bmm.map_or((String::new(), 0), |b| (hex(&b.producer[..4]), b.bid));
            let n = game.streak().map_or(0, |(_, n)| n);
            writeln!(
                csv,
                "{height},{producer},{bid},{bonus},{n},{},{},{},{},{},{}",
                game.streak_pool,
                game.price,
                game.round,
                game.pot,
                report.accepted.len(),
                game.clock
            )
            .map_err(|e| e.to_string())?;
        }
        if self.quiet {
            return Ok(());
        }
        if let Some(end) = &report.round_end {
            println!(
                "{height}: round {} ended, final block {:?}, winners {:?}, carried {}, to bonus pool {}",
                end.round,
                end.final_height,
                end.winners.iter().map(|(p, s)| (short(p), s)).collect::<Vec<_>>(),
                end.carried,
                end.to_bonus_pool
            );
        }
        if !entries.is_empty() {
            println!(
                "{height}: price {} accepted {:?} rejected {:?} -> pot {} streak_pool {} clock {} deadline {}",
                report.price,
                report.accepted.iter().map(|e| (short(&e.player), e.action)).collect::<Vec<_>>(),
                report.rejected.iter().map(|(e, r)| (short(&e.player), *r)).collect::<Vec<_>>(),
                game.pot,
                game.streak_pool,
                game.clock,
                game.deadline
            );
        }
        for (dest, sats) in &report.payouts {
            println!("{height}: cashout {sats} to sidechain address {}", hex(dest));
        }
        Ok(())
    }

    fn summary(&self) {
        let game = &self.game;
        println!(
            "processed {}..={} (tip {}, depth {}); round {}",
            self.start, game.height, self.tip, game.params.depth, game.round
        );
        for player in game.accounts.keys() {
            println!("  {} balance {} keys {}", short(player), game.balance(player), game.accounts[player].keys);
        }
        let (won, bids, bonuses) = self.stats.values().fold((0, 0, 0), |(w, b, n), s| (w + s.0, b + s.1, n + s.2));
        println!("BMM: {won} blocks won, bids {bids}, bonuses paid {bonuses}");
        for (producer, (won, paid, got)) in &self.stats {
            println!("  producer {} won {won} bid {paid} bonus {got}", short(producer));
        }
    }

    /// The game after the last processed block, for drivers.
    fn state(&self) -> Value {
        let game = &self.game;
        json!({
            "height": game.height,
            "clock": game.clock,
            "tip_clock": self.tip_clock,
            "round": game.round,
            "deadline": game.deadline,
            "price": game.price,
            "pot": game.pot,
            "streak_pool": game.streak_pool,
            "streak": game.streak().map(|(p, n)| json!({"producer": hex(&p), "n": n})),
            "balances": game.accounts.keys().map(|p| (hex(p), json!(game.balance(p)))).collect::<serde_json::Map<_, _>>(),
        })
    }
}

/// The BIP300 deposits in a block: (address, sats) for each transaction whose treasury output is larger than the
/// treasury output it spends. Needs `getblock` verbosity 3, which includes each input's previous output.
fn deposits(block: &Value, treasury_script: &[u8]) -> Vec<(String, u64)> {
    let script = |o: &Value| unhex(o["scriptPubKey"]["hex"].as_str()?).ok();
    let mut found = Vec::new();
    for tx in block["tx"].as_array().into_iter().flatten() {
        let outs: Vec<&Value> = tx["vout"].as_array().into_iter().flatten().collect();
        let Some(new) = outs.iter().find(|o| script(o).as_deref() == Some(treasury_script)) else { continue };
        let old: u64 = tx["vin"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| script(&i["prevout"]).as_deref() == Some(treasury_script))
            .filter_map(|i| sats(&i["prevout"]["value"]))
            .sum();
        let grew = sats(&new["value"]).unwrap_or(0).saturating_sub(old);
        // The deposit's address is the data of its OP_RETURN output.
        let address = outs.iter().find_map(|o| match script(o)?.as_slice() {
            [0x6a, n, data @ ..] if usize::from(*n) == data.len() => String::from_utf8(data.to_vec()).ok(),
            _ => None,
        });
        if let (true, Some(address)) = (grew > 0, address) {
            found.push((address, grew));
        }
    }
    found
}

fn sats(value: &Value) -> Option<u64> {
    Some((value.as_f64()? * 1e8).round() as u64)
}

/// The block's BMM for `slot`: the h* its coinbase M7 accepts, and the fee of the M8 transaction committing to it
/// and to the previous block's hash (an M8 is only valid in the block after the one it names).
fn block_bmm(block: &Value, slot: u8) -> Option<Bmm> {
    let script = |o: &Value| unhex(o["scriptPubKey"]["hex"].as_str()?).ok();
    let txs = block["tx"].as_array()?;
    let mut prev = unhex(block["previousblockhash"].as_str()?).ok()?;
    prev.reverse();
    let hstar = txs.first()?["vout"].as_array()?.iter().find_map(|o| match script(o)?.as_slice() {
        [0x6a, 0x25, 0xd1, 0x61, 0x73, 0x68, s, h @ ..] if *s == slot && h.len() == 32 => Some(h.to_vec()),
        _ => None,
    })?;
    let fee = txs.iter().skip(1).find_map(|tx| {
        let commits = tx["vout"].as_array()?.iter().any(|o| match script(o).as_deref() {
            Some([0x6a, 0x44, 0x00, 0xbf, 0x00, s, rest @ ..]) => {
                *s == slot && rest.len() == 64 && rest[..32] == hstar[..] && rest[32..] == prev[..]
            }
            _ => false,
        });
        commits.then(|| sats(&tx["fee"])).flatten()
    })?;
    Some(Bmm { producer: hstar.try_into().ok()?, bid: fee })
}

/// Ticks of the game clock in the L1 blocks `from..=to`: those with at least one non-coinbase transaction.
fn ticks(rpc: &Rpc, from: u32, to: u32) -> Result<u32, String> {
    let mut n = 0;
    for height in from..=to {
        let hash = rpc.call("getblockhash", json!([height]))?;
        let header = rpc.call("getblockheader", json!([hash]))?;
        n += u32::from(header["nTx"].as_u64().ok_or("bad getblockheader")? > 1);
    }
    Ok(n)
}

/// Every well-formed entry in a block's non-coinbase transactions, in block order. Malformed Sidepot outputs are
/// skipped.
fn block_entries(block: &Value) -> Vec<Entry> {
    let txs = block["tx"].as_array().into_iter().flatten().skip(1);
    let scripts = txs.flat_map(|tx| tx["vout"].as_array().into_iter().flatten());
    scripts
        .filter_map(|out| unhex(out["scriptPubKey"]["hex"].as_str()?).ok())
        .filter_map(|script| Entry::from_script(&script)?.ok())
        .collect()
}

struct Rpc {
    url: String,
    auth: String,
}

impl Rpc {
    fn new(url: &str, cookie: &str) -> Result<Self, String> {
        Ok(Self { url: url.to_string(), auth: std::fs::read_to_string(cookie).map_err(|e| e.to_string())? })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let auth = base64::engine::general_purpose::STANDARD.encode(self.auth.trim());
        let reply: Value = ureq::post(&self.url)
            .set("Authorization", &format!("Basic {auth}"))
            .send_json(json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params}))
            .map_err(|e| format!("{method}: {e}"))?
            .into_json()
            .map_err(|e| e.to_string())?;
        match &reply["error"] {
            Value::Null => Ok(reply["result"].clone()),
            err => Err(format!("{method}: {err}")),
        }
    }
}

fn keypair(sk: &str) -> Result<Keypair, String> {
    Keypair::from_seckey_slice(SECP256K1, &unhex(sk)?).map_err(|e| e.to_string())
}

/// The domain from a block hash as RPC prints it (byte-reversed) and a slot number.
fn domain(anchor: &str, slot: &str) -> Result<Domain, String> {
    let mut anchor: [u8; 32] = unhex(anchor)?.try_into().map_err(|_| "a block hash is 32 bytes")?;
    anchor.reverse();
    Ok(Domain { anchor, slot: num(slot)? })
}

fn player_key(s: &str) -> Result<PlayerKey, String> {
    unhex(s)?.try_into().map_err(|_| "a player key is 32 bytes".to_string())
}

fn num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("not a number: {s}"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string())).collect()
}

fn short(p: &PlayerKey) -> String {
    hex(&p[..4])
}
