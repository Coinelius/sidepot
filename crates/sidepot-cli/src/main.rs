//! Sidepot test tool.
//!
//!     sidepot-cli pubkey <seckey-hex>
//!     sidepot-cli buy <seckey-hex> <round> <keys> <max-price> <nonce>
//!     sidepot-cli cashout <seckey-hex> <sats, 0 = all> <dest-hex-20-bytes> <nonce>
//!     sidepot-cli scan <rpc-url> <cookie-path> <slot> <start-height> [<player-hex>:<sats> ...]
//!     sidepot-cli follow <rpc-url> <cookie-path> <slot> <start-height>
//!
//! `buy` and `cashout` print the entry as hex, ready for a `{"data": ...}` output of `createrawtransaction`.
//! `scan` replays the game from `start-height` to `depth` blocks below the node's tip and prints every block that
//! touched it. BIP300 deposits to `slot` whose address is `spot:<player-hex>` credit that game account; listed
//! players get an extra test balance first. Each L1 block's BMM (the M7 accept in its coinbase, the bid being the
//! matching M8 transaction's fee) drives the streak bonus. Until Sidepot blocks exist, the committed hash h*
//! stands in for the producer's key. SIDEPOT_QUIET=1 prints only the summary; SIDEPOT_BMM_CSV=<path> writes one
//! row per L1 block; SIDEPOT_BONUS_RAMP=<n> overrides the bonus ramp (0 = the full cap from a streak's first
//! block). The last line is `STATE <json>`, the game after the last processed block. `follow` does the same
//! incrementally: each line on stdin advances to the current tip and prints a `STATE` line.

use std::collections::BTreeMap;
use std::io::Write;

use std::process::exit;

use base64::Engine;
use secp256k1::{Keypair, SECP256K1};
use serde_json::{json, Value};
use sidepot_core::{deposit_player, Bmm, Entry, Game, Params, PlayerKey};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["pubkey", sk] => keypair(sk).map(|kp| println!("{}", hex(&kp.x_only_public_key().0.serialize()))),
        ["buy", sk, round, keys, max_price, nonce] => (|| {
            let e = Entry::buy(&keypair(sk)?, num(round)?, num(keys)?, num(max_price)?, num(nonce)?);
            println!("{}", hex(&e.encode()));
            Ok(())
        })(),
        ["cashout", sk, amount, dest, nonce] => (|| {
            let dest = unhex(dest)?.try_into().map_err(|_| "a sidechain address is 20 bytes")?;
            println!("{}", hex(&Entry::cashout(&keypair(sk)?, num(amount)?, dest, num(nonce)?).encode()));
            Ok(())
        })(),
        ["scan", url, cookie, slot, start, funds @ ..] => (|| scan(url, cookie, num(slot)?, num(start)?, funds))(),
        ["follow", url, cookie, slot, start] => (|| follow(url, cookie, num(slot)?, num(start)?))(),
        _ => Err("usage: see the doc comment in crates/sidepot-cli/src/main.rs".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        exit(1);
    }
}

fn scan(url: &str, cookie: &str, slot: u8, start: u32, funds: &[&str]) -> Result<(), String> {
    let mut scanner = Scanner::new(url, cookie, slot, start, funds)?;
    scanner.advance()?;
    scanner.summary();
    println!("STATE {}", scanner.state());
    Ok(())
}

/// Like `scan`, but keeps the game in memory: each line on stdin processes the blocks added since the last one and
/// prints `STATE <json>`. The summary follows at end of input.
fn follow(url: &str, cookie: &str, slot: u8, start: u32) -> Result<(), String> {
    let mut scanner = Scanner::new(url, cookie, slot, start, &[])?;
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
    slot: u8,
    start: u32,
    /// The slot's treasury output: OP_DRIVECHAIN <slot> OP_TRUE. Each deposit replaces it with a larger one.
    treasury_script: [u8; 4],
    quiet: bool,
    csv: Option<std::fs::File>,
    /// Per producer: blocks won, bids paid, bonuses received.
    stats: BTreeMap<PlayerKey, (u32, u64, u64)>,
    tip: u32,
}

impl Scanner {
    fn new(url: &str, cookie: &str, slot: u8, start: u32, funds: &[&str]) -> Result<Self, String> {
        let rpc = Rpc { url: url.to_string(), auth: std::fs::read_to_string(cookie).map_err(|e| e.to_string())? };
        let mut params = Params::default();
        if let Ok(ramp) = std::env::var("SIDEPOT_BONUS_RAMP") {
            params.bonus_ramp = num(&ramp)?;
        }
        let mut game = Game::new(params, start);
        for f in funds {
            let (player, sats) = f.split_once(':').ok_or("fund as <player-hex>:<sats>")?;
            game.deposit(player_key(player)?, num(sats)?);
        }
        let csv = match std::env::var("SIDEPOT_BMM_CSV") {
            Ok(path) => {
                let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
                writeln!(f, "height,producer,bid,bonus,streak,streak_pool,price,round,pot,entries")
                    .map_err(|e| e.to_string())?;
                Some(f)
            }
            Err(_) => None,
        };
        let quiet = std::env::var_os("SIDEPOT_QUIET").is_some();
        let treasury_script = [0xb7, 0x01, slot, 0x51];
        Ok(Self { rpc, game, slot, start, treasury_script, quiet, csv, stats: BTreeMap::new(), tip: 0 })
    }

    /// Processes every block from the last processed one up to `depth` below the node's tip.
    fn advance(&mut self) -> Result<(), String> {
        self.tip = self.rpc.call("getblockcount", json!([]))?.as_u64().ok_or("bad getblockcount")? as u32;
        while self.game.height < self.tip.saturating_sub(self.game.params.depth) {
            self.block(self.game.height + 1)?;
        }
        Ok(())
    }

    fn block(&mut self, height: u32) -> Result<(), String> {
        let game = &mut self.game;
        let hash = self.rpc.call("getblockhash", json!([height]))?;
        let block = self.rpc.call("getblock", json!([hash, 3]))?;
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
        let report = game.process_batch(height, &entries);
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
                "{height},{producer},{bid},{bonus},{n},{},{},{},{},{}",
                game.streak_pool,
                game.price,
                game.round,
                game.pot,
                report.accepted.len()
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
                "{height}: price {} accepted {:?} rejected {:?} -> pot {} streak_pool {} deadline {}",
                report.price,
                report.accepted.iter().map(|e| (short(&e.player), e.action)).collect::<Vec<_>>(),
                report.rejected.iter().map(|(e, r)| (short(&e.player), *r)).collect::<Vec<_>>(),
                game.pot,
                game.streak_pool,
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

/// Every well-formed entry in a block, in block order. Malformed Sidepot outputs are skipped.
fn block_entries(block: &Value) -> Vec<Entry> {
    let txs = block["tx"].as_array().into_iter().flatten();
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
