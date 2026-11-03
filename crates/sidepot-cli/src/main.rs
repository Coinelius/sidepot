//! Sidepot test tool.
//!
//!     sidepot-cli pubkey <seckey-hex>
//!     sidepot-cli buy <seckey-hex> <round> <keys> <max-price> <nonce>
//!     sidepot-cli cashout <seckey-hex> <sats, 0 = all> <dest-hex-20-bytes> <nonce>
//!     sidepot-cli scan <rpc-url> <cookie-path> <slot> <start-height> [<player-hex>:<sats> ...]
//!
//! `buy` and `cashout` print the entry as hex, ready for a `{"data": ...}` output of `createrawtransaction`.
//! `scan` replays the game from `start-height` to `depth` blocks below the node's tip and prints every block that
//! touched it. BIP300 deposits to `slot` whose address is `spot:<player-hex>` credit that game account; listed
//! players get an extra test balance first.

use std::process::exit;

use base64::Engine;
use secp256k1::{Keypair, SECP256K1};
use serde_json::{json, Value};
use sidepot_core::{deposit_player, Entry, Game, Params, PlayerKey};

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
        _ => Err("usage: see the doc comment in crates/sidepot-cli/src/main.rs".into()),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        exit(1);
    }
}

fn scan(url: &str, cookie: &str, slot: u8, start: u32, funds: &[&str]) -> Result<(), String> {
    let rpc = Rpc { url: url.to_string(), auth: std::fs::read_to_string(cookie).map_err(|e| e.to_string())? };
    let mut game = Game::new(Params::default(), start);
    for f in funds {
        let (player, sats) = f.split_once(':').ok_or("fund as <player-hex>:<sats>")?;
        game.deposit(player_key(player)?, num(sats)?);
    }
    let tip: u32 = rpc.call("getblockcount", json!([]))?.as_u64().ok_or("bad getblockcount")? as u32;
    let last = tip.saturating_sub(game.params.depth);
    // The slot's treasury output: OP_DRIVECHAIN <slot> OP_TRUE. Each deposit replaces it with a larger one.
    let treasury_script = [0xb7, 0x01, slot, 0x51];
    for height in start..=last {
        let hash = rpc.call("getblockhash", json!([height]))?;
        let block = rpc.call("getblock", json!([hash, 3]))?;
        // Deposits in a block are credited before its batch, so a player can deposit and buy in one block.
        for (address, sats) in deposits(&block, &treasury_script) {
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
        if let Some(end) = &report.round_end {
            println!(
                "{height}: round {} ended, final block {:?}, winners {:?}, carried {}",
                end.round,
                end.final_height,
                end.winners.iter().map(|(p, s)| (short(p), s)).collect::<Vec<_>>(),
                end.carried
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
    }
    println!("processed {start}..={last} (tip {tip}, depth {}); round {}", game.params.depth, game.round);
    for player in game.accounts.keys() {
        println!("  {} balance {} keys {}", short(player), game.balance(player), game.accounts[player].keys);
    }
    Ok(())
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
