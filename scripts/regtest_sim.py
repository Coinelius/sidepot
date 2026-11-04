#!/usr/bin/env python3
"""Regtest simulation of Sidepot rounds with a real BMM auction and a miner that bids for itself.

    regtest_sim.py <blocks> [seed]

Six players deposit into game accounts and buy keys with simple strategies. Three outside producers post real BMM
requests (M8) for slot 88 through the enforcer wallet each block, bidding from the bonus they would win. A fourth,
the miner, mines MINER_SHARE (default 30%) of blocks: in those it accepts its own M8, with the bid paid back to
itself, whenever it holds the streak or the best outside bid is below the full cap; in the others it bids like an
outside producer. Blocks are mined from the enforcer's template, which accepts the highest M8; the miner's own block
is modelled by its posting the highest M8 just before it mines. M8s are built from the node wallet, in the format
the enforcer's wallet uses. Until Sidepot blocks exist, a producer's 32-byte id
doubles as the committed hash h*. SIDEPOT_BONUS_RAMP=0 runs the flat-cap variant.
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
MINER_SHARE = float(os.environ.get("MINER_SHARE", "0.3"))
# A node wallet holding only confirmed coins: buys and bids spend confirmed coins only, since losing bids never
# confirm and anything chained on them would never be mined.
WALLET = os.environ.get("SIM_WALLET", "sim")
RAMP = int(os.environ.get("SIDEPOT_BONUS_RAMP", "3"))
OUT = os.environ.get("SIM_OUT", f"{DIR}/sim")

USED = set()

# Bonus rules, mirrored from sidepot-core's defaults to size the producers' bids.
BONUS_POOL_BPS, BID_MULTIPLE, MIN_BID = 200, 2, 10_000


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
    tip, fee exactly `bid`. The same transaction the enforcer wallet builds, without its ~1.5 s per request."""
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
        node("sendrawtransaction", wallet("signrawtransactionwithwallet", raw)["hex"])
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
        f = {"steady": 0.8, "defender": 1.1 if holding else 0.6, "opportunist": rng.uniform(0.2, 1.0),
             "miner": 0.8}[self.style]
        if self.style == "opportunist" and rng.random() < 0.4:
            return 0
        b = int(bonus * f)
        return b if b >= MIN_BID else 0


def main():
    players = [Player(i) for i in range(6)]
    outside = [Producer("steady", 0, "steady"), Producer("defender", 1, "defender"),
               Producer("opportunist", 2, "opportunist")]
    miner = Producer("miner", 3, "miner")
    start = node("getblockcount") + 1
    print(f"game starts at L1 {start}; ramp {RAMP}; miner share {MINER_SHARE:.0%}", flush=True)
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
        miner_block = rng.random() < MINER_SHARE
        self_accept = False
        if miner_block:
            bonus, cap, holding = bonus_for(s, miner.id)
            best = max(bids.values(), default=0)
            # Its own block: keep its streak, or break another's when the bid it gives up is below the full cap.
            if bonus >= MIN_BID and (holding or best < cap):
                own = max(best + 1, (bonus + 1) // BID_MULTIPLE, MIN_BID)
                self_accept = submit_bid(miner.id, own, tip, tip_hash)
                if self_accept:
                    bids["miner"] = own
        else:
            b = miner.bid(s)
            if b and submit_bid(miner.id, b, tip, tip_hash):
                bids["miner"] = b
        mine()
        log.write(json.dumps({"height": tip + 1, "bids": bids, "miner_block": miner_block,
                              "self_accept": self_accept, "keys": keys}) + "\n")
        log.flush()
        if i % 50 == 0:
            print(f"block {tip + 1}: round {s['round']} price {s['price']} pool {s['streak_pool']} "
                  f"pot {s['pot']} streak {s['streak'] and (s['streak']['producer'][:8], s['streak']['n'])}",
                  flush=True)

    mine(2)
    follow.state()
    print(follow.finish())
    print("producer ids:", {pr.name: pr.id[:8] for pr in outside + [miner]})


if __name__ == "__main__":
    main()
