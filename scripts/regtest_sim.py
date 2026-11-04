#!/usr/bin/env python3
"""Regtest simulation of Sidepot rounds with a real BMM auction and miners renting hashrate in bursts.

    regtest_sim.py <blocks> [seed]

Six players deposit into game accounts and buy keys with simple strategies. Three outside producers post real BMM
requests (M8) for slot 88 each block, bidding from the bonus they would win. Three renters watch the block bonus cap
and, once it reaches their trigger, rent 30-60% of the hashrate for 3-8 blocks, then cool down for 10-30. In blocks a
renter finds, it accepts its own M8 (the bid paid back to itself) whenever the cap beats the best outside bid; the
other blocks go to ordinary miners, whose enforcer template accepts the highest M8. A renter's own block is
modelled by its posting the highest M8 just before the block is mined. M8s are built from the node wallet, in the
format the enforcer's wallet uses. Until Sidepot blocks exist, a producer's 32-byte id doubles as the committed hash
h*. SIDEPOT_BONUS_RAMP=<n> runs the optional streak ramp instead of the flat cap.
"""
import json
import os
import random
import subprocess
import sys

DIR = os.environ.get("SIDEPOT_REGTEST", "/opt/sidepot-regtest")
sys.path.insert(0, DIR)
from regtest_miner import mine_one, node  # noqa: E402

SP = os.environ.get("SIDEPOT_CLI", "/root/sidepot-build/target/release/sidepot-cli")
GRPC = "127.0.0.1:50153"
SLOT = 88
COOKIE = f"{DIR}/node/regtest/.cookie"
BLOCKS = int(sys.argv[1]) if len(sys.argv) > 1 else 330
rng = random.Random(int(sys.argv[2]) if len(sys.argv) > 2 else 7)
# Block bonus cap, in sats, at which each renter starts renting.
RENTER_TRIGGERS = [int(t) for t in os.environ.get("RENTER_TRIGGERS", "5000000,15000000,30000000").split(",")]
# A node wallet holding only confirmed coins: buys and bids spend confirmed coins only, since losing bids never
# confirm and anything chained on them would never be mined.
WALLET = os.environ.get("SIM_WALLET", "sim")
RAMP = int(os.environ.get("SIDEPOT_BONUS_RAMP", "0"))
OUT = os.environ.get("SIM_OUT", f"{DIR}/sim")

USED = set()
# Bids posted for the next block: (txid, coin, coin value, fee).
PENDING = []


def recycle_losing_bids(height):
    """Replaces each bid the block at `height` did not include with a plain transfer to the wallet at a slightly
    higher fee. A losing M8 never confirms, so without this its coin would stay locked in the mempool."""
    mined = set(node("getblock", node("getblockhash", height))["tx"])
    for txid, coin, value, fee in PENDING:
        if txid in mined:
            continue
        raw = wallet("createrawtransaction", [{"txid": coin["txid"], "vout": coin["vout"]}],
                     [{wallet("getrawchangeaddress"): (value - fee - 2_000) / 1e8}])
        try:
            node("sendrawtransaction", wallet("signrawtransactionwithwallet", raw)["hex"], 0)
        except (RuntimeError, OSError) as e:
            print(f"  could not recycle losing bid {txid[:12]}: {e}", flush=True)
    PENDING.clear()
    USED.clear()


def recycle_stale_bids():
    """Replaces every unconfirmed M8 the wallet still has in the mempool, e.g. left by an interrupted run."""
    stale = {t["txid"] for t in wallet("listtransactions", "*", 100_000) if t["confirmations"] == 0}
    freed = 0
    for txid in stale:
        try:
            tx = node("getrawtransaction", txid, 1)
        except RuntimeError:
            continue  # already replaced
        if not any(o["scriptPubKey"]["hex"].startswith("6a4400bf00") for o in tx["vout"]):
            continue
        inputs = [{"txid": i["txid"], "vout": i["vout"]} for i in tx["vin"]]
        value = sum(round(node("getrawtransaction", i["txid"], 1)["vout"][i["vout"]]["value"] * 1e8) for i in tx["vin"])
        fee = value - sum(round(o["value"] * 1e8) for o in tx["vout"])
        raw = wallet("createrawtransaction", inputs, [{wallet("getrawchangeaddress"): (value - fee - 2_000) / 1e8}])
        try:
            node("sendrawtransaction", wallet("signrawtransactionwithwallet", raw)["hex"], 0)
            freed += 1
        except (RuntimeError, OSError) as e:
            print(f"  could not recycle stale bid {txid[:12]}: {e}", flush=True)
    if freed:
        mine()
        print(f"recycled {freed} stale bids from an earlier run", flush=True)

# Bonus rules, mirrored from sidepot-core's defaults to size the producers' bids.
BONUS_POOL_BPS, BID_MULTIPLE, MIN_BID = 500, 2, 10_000


def wallet(method, *params):
    return node_wallet(method, list(params))


def node_wallet(method, params):
    import regtest_miner as m
    return m.rpc(m.NODE_RPC + "/wallet/" + WALLET, method, params, open(COOKIE).read().strip())


def grpc(method, body):
    return subprocess.run(["grpcurl", "-plaintext", "-d", json.dumps(body), GRPC, method],
                          capture_output=True, text=True)


def mine(n=1):
    for _ in range(n):
        mine_one()


def post(data_hex):
    raw = wallet("createrawtransaction", [], [{"data": data_hex}])
    funded = wallet("fundrawtransaction", raw, {"minconf": 1})["hex"]
    node("sendrawtransaction", wallet("signrawtransactionwithwallet", funded)["hex"])


def sp(*args):
    return subprocess.run([SP] + [str(a) for a in args], check=True, capture_output=True, text=True).stdout


class Follow:
    """sidepot-cli follow: the game state, advanced incrementally."""

    def __init__(self, start, csv):
        env = {**os.environ, "SIDEPOT_QUIET": "1", "SIDEPOT_BMM_CSV": csv, "SIDEPOT_BONUS_RAMP": str(RAMP)}
        self.proc = subprocess.Popen([SP, "follow", "http://127.0.0.1:18943", COOKIE, str(SLOT), str(start)],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)

    def state(self):
        self.proc.stdin.write("\n")
        self.proc.stdin.flush()
        while not (line := self.proc.stdout.readline()).startswith("STATE "):
            if not line:
                sys.exit("follow exited")
        return json.loads(line[6:])

    def finish(self):
        self.proc.stdin.close()
        return self.proc.stdout.read()


def bonus_for(s, producer_id):
    """The bonus `producer_id` would win in the next block, and whether it holds the streak."""
    cap = s["streak_pool"] * BONUS_POOL_BPS // 10_000
    streak = s["streak"]
    holding = bool(streak) and streak["producer"] == producer_id
    n = streak["n"] + 1 if holding else 1
    return cap >> max(RAMP - (n - 1), 0), cap, holding


def submit_bid(producer_id, bid, tip, tip_hash):
    """Posts an M8 BMM request from the node wallet: OP_RETURN 00bf00 <slot> <h*> <prev block hash>, locktime at the
    tip, fee exactly `bid`. The same transaction the enforcer wallet builds, without its ~1.5 s per request. A bid is
    all fee, so it is sent with maxfeerate 0 (no cap)."""
    # A confirmed coin not used this block: losing bids stay in the mempool, so bids must not chain on them.
    coin = next(u for u in sorted(wallet("listunspent", 1), key=lambda u: -u["amount"])
                if (u["txid"], u["vout"]) not in USED and u["amount"] * 1e8 > bid + 100_000)
    USED.add((coin["txid"], coin["vout"]))
    value = round(coin["amount"] * 1e8)
    change = wallet("getrawchangeaddress")
    data = "00bf00" + f"{SLOT:02x}" + producer_id + bytes.fromhex(tip_hash)[::-1].hex()
    raw = wallet("createrawtransaction", [{"txid": coin["txid"], "vout": coin["vout"]}],
                 [{"data": data}, {change: (value - bid) / 1e8}], tip)
    try:
        txid = node("sendrawtransaction", wallet("signrawtransactionwithwallet", raw)["hex"], 0)
        PENDING.append((txid, coin, value, bid))
        return True
    except (RuntimeError, OSError) as e:
        print(f"  bid {bid} for {producer_id[:8]} failed: {e}", flush=True)
        return False


class Player:
    def __init__(self, i):
        self.sk = f"{i + 1:02x}" * 32
        self.key = sp("pubkey", self.sk).strip()
        self.nonce = 0
        self.limit = rng.choice([5_000_000, 20_000_000, 50_000_000, 100_000_000])  # most a key is worth to them

    def act(self, s, blocks_left):
        balance = s["balances"].get(self.key, 0)
        price = s["price"]
        if price > self.limit or blocks_left < 0:
            return 0
        sniping = blocks_left <= 6
        if rng.random() > (0.7 if sniping else 0.2):
            return 0
        keys = rng.randint(1, 3 if sniping else 5)
        if keys * price * 1.5 > balance:
            return 0
        self.nonce += 1
        # The state lags two blocks, so allow for the price having moved on.
        post(sp("buy", self.sk, s["round"], keys, price * 2, self.nonce).strip())
        return keys


class Producer:
    def __init__(self, name, i, style):
        self.name, self.style = name, style
        self.id = f"{0xa0 + i:02x}" * 32

    def bid(self, s):
        bonus, _, holding = bonus_for(s, self.id)
        f = {"steady": 0.8, "defender": 1.1 if holding else 0.6, "opportunist": rng.uniform(0.2, 1.0)}[self.style]
        if self.style == "opportunist" and rng.random() < 0.4:
            return 0
        b = int(bonus * f)
        return b if b >= MIN_BID else 0


class Renter:
    """A miner that rents hashrate in bursts when the block bonus is worth it, and accepts its own M8 in the blocks
    it finds whenever the cap beats the outside bid it gives up."""

    def __init__(self, name, i, trigger):
        self.name, self.id, self.trigger = name, f"{0xa0 + i:02x}" * 32, trigger
        self.left = self.cooldown = 0
        self.share = 0.0
        self.bursts = 0

    def tick(self, cap):
        if self.left:
            self.left -= 1
            if not self.left:
                self.cooldown = rng.randint(10, 30)
        elif self.cooldown:
            self.cooldown -= 1
        elif cap >= self.trigger and rng.random() < 0.5:
            self.left, self.share = rng.randint(3, 8), rng.uniform(0.3, 0.6)
            self.bursts += 1
        return self.share if self.left else 0.0


def block_miner(renters, shares):
    """Who finds the next block: a renting miner with its share of the hashrate, else the ordinary miners."""
    total = sum(shares)
    scale = 0.9 / total if total > 0.9 else 1.0
    r = rng.random()
    for renter, share in zip(renters, shares):
        r -= share * scale
        if r < 0:
            return renter
    return None


def main():
    players = [Player(i) for i in range(6)]
    outside = [Producer("steady", 0, "steady"), Producer("defender", 1, "defender"),
               Producer("opportunist", 2, "opportunist")]
    renters = [Renter(f"renter{j + 1}", 4 + j, t) for j, t in enumerate(RENTER_TRIGGERS)]
    recycle_stale_bids()
    start = node("getblockcount") + 1
    print(f"game starts at L1 {start}; ramp {RAMP}; cap {BONUS_POOL_BPS / 100:g}% of the pool; "
          f"renter triggers {RENTER_TRIGGERS}", flush=True)
    for p in players:
        r = grpc("cusf.mainchain.v1.WalletService/CreateDepositTransaction",
                 {"sidechainId": SLOT, "address": f"spot:{p.key}", "valueSats": "3000000000", "feeSats": "10000"})
        if r.returncode:
            sys.exit(f"deposit failed: {r.stdout}{r.stderr}")
        mine()

    follow = Follow(start, f"{OUT}-bmm.csv")
    log = open(f"{OUT}-bids.jsonl", "w")
    for i in range(BLOCKS):
        s = follow.state()
        tip = node("getblockcount")
        tip_hash = node("getblockhash", tip)
        keys = sum(p.act(s, s["deadline"] - (tip + 1)) for p in players)

        bids = {}
        for pr in outside:
            b = pr.bid(s)
            if b and submit_bid(pr.id, b, tip, tip_hash):
                bids[pr.name] = b
        best = max(bids.values(), default=0)
        cap = s["streak_pool"] * BONUS_POOL_BPS // 10_000
        shares = [r.tick(cap) for r in renters]
        miner = block_miner(renters, shares)
        self_accept = False
        if miner and cap > best and cap >= MIN_BID:
            # Its own block: worth it whenever the bonus beats the outside bid it would otherwise collect.
            own = max(best + 1, (cap + 1) // BID_MULTIPLE, MIN_BID)
            self_accept = submit_bid(miner.id, own, tip, tip_hash)
            if self_accept:
                bids[miner.name] = own
        mine()
        recycle_losing_bids(tip + 1)
        log.write(json.dumps({"height": tip + 1, "bids": bids, "miner": miner and miner.name,
                              "renting": {r.name: round(sh, 2) for r, sh in zip(renters, shares) if sh},
                              "best_outside": best, "self_accept": self_accept, "keys": keys}) + "\n")
        log.flush()
        if i % 50 == 0:
            print(f"block {tip + 1}: round {s['round']} price {s['price']} pool {s['streak_pool']} pot {s['pot']}",
                  flush=True)

    mine(2)
    follow.state()
    print(follow.finish())
    print("bursts:", {r.name: r.bursts for r in renters})
    print("producer ids:", {pr.name: pr.id[:8] for pr in outside + renters})


if __name__ == "__main__":
    main()
