#!/usr/bin/env python3
"""Regtest simulation of a Sidepot round with a real BMM auction.

    regtest_sim.py <blocks> [seed]

Six players deposit into game accounts and buy keys with simple strategies; three producers post real BMM
requests (M8) for slot 88 through the enforcer wallet each block, bidding from the bonus they would win. Blocks
are mined from the enforcer's template, so the enforcer's choice of M8 decides each auction. Until Sidepot blocks
exist, a producer's 32-byte id doubles as the committed hash h*. Game state comes from replaying the chain with
`sidepot-cli scan`; the run ends with a summary of buy volume, bids paid to miners and bonuses.
"""
import json
import os
import random
import subprocess
import sys

DIR = os.environ.get("SIDEPOT_REGTEST", "/opt/sidepot-regtest")
SP = os.environ.get("SIDEPOT_CLI", "/root/sidepot-build/target/release/sidepot-cli")
CLI = ["/usr/local/bin/trixie-exec", "/opt/ecash-1.2.0/bin/bitcoin-cli", f"-datadir={DIR}/node"]
GRPC = "127.0.0.1:50153"
SLOT = 88
COOKIE = f"{DIR}/node/regtest/.cookie"
BLOCKS = int(sys.argv[1]) if len(sys.argv) > 1 else 330
rng = random.Random(int(sys.argv[2]) if len(sys.argv) > 2 else 7)

# Bonus rules, mirrored from sidepot-core's defaults to size the producers' bids.
BONUS_POOL_BPS, BID_MULTIPLE, MIN_BID = 200, 2, 10_000


def cli(*args, wallet=None):
    cmd = CLI + ([f"-rpcwallet={wallet}"] if wallet else []) + [str(a) for a in args]
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.strip()


def grpc(method, body):
    return subprocess.run(["grpcurl", "-plaintext", "-d", json.dumps(body), GRPC, method],
                          capture_output=True, text=True)


def mine(n=1):
    subprocess.run(["python3", f"{DIR}/regtest_miner.py", str(n)], check=True, capture_output=True)


def post(data_hex):
    raw = cli("createrawtransaction", "[]", json.dumps([{"data": data_hex}]), wallet="test")
    funded = json.loads(cli("fundrawtransaction", raw, wallet="test"))["hex"]
    signed = json.loads(cli("signrawtransactionwithwallet", funded, wallet="test"))["hex"]
    cli("sendrawtransaction", signed)


def sp(*args, env=None):
    return subprocess.run([SP] + [str(a) for a in args], check=True, capture_output=True, text=True,
                          env={**os.environ, **(env or {})}).stdout


def state(start):
    out = sp("scan", "http://127.0.0.1:18943", COOKIE, SLOT, start, env={"SIDEPOT_QUIET": "1"})
    return json.loads(out.rsplit("STATE ", 1)[1])


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
        cap = s["streak_pool"] * BONUS_POOL_BPS // 10_000
        streak = s["streak"]
        holding = bool(streak) and streak["producer"] == self.id
        n = streak["n"] + 1 if holding else 1
        bonus = cap >> (4 - min(n, 4))
        f = {"steady": 0.8, "defender": 1.1 if holding else 0.6, "opportunist": rng.uniform(0.2, 1.0)}[self.style]
        if self.style == "opportunist" and rng.random() < 0.4:
            return 0
        b = int(bonus * f)
        return b if b >= MIN_BID else 0


def main():
    players = [Player(i) for i in range(6)]
    producers = [Producer("steady", 0, "steady"), Producer("defender", 1, "defender"),
                 Producer("opportunist", 2, "opportunist")]
    start = int(cli("getblockcount")) + 1
    print(f"game starts at L1 {start}; players {[p.key[:8] for p in players]}", flush=True)
    for p in players:
        r = grpc("cusf.mainchain.v1.WalletService/CreateDepositTransaction",
                 {"sidechainId": SLOT, "address": f"spot:{p.key}", "valueSats": "3000000000", "feeSats": "10000"})
        if r.returncode:
            sys.exit(f"deposit failed: {r.stdout}{r.stderr}")
        mine()

    posted_bids = 0
    bid_log = open(f"{DIR}/sim-bids.jsonl", "w")
    for i in range(BLOCKS):
        s = state(start)
        tip = int(cli("getblockcount"))
        blocks_left = s["deadline"] - (tip + 1)
        keys = sum(p.act(s, blocks_left) for p in players)
        tip_hash = cli("getblockhash", tip)
        bids = {}
        for pr in producers:
            b = pr.bid(s)
            if not b:
                continue
            r = grpc("cusf.mainchain.v1.WalletService/CreateBmmCriticalDataTransaction",
                     {"sidechainId": SLOT, "valueSats": b, "height": tip, "criticalHash": {"hex": pr.id},
                      "prevBytes": {"hex": tip_hash}})
            posted_bids += r.returncode == 0
            if r.returncode == 0:
                bids[pr.id[:8]] = b
            if r.returncode:
                print(f"  {pr.name} bid {b} failed: {(r.stdout + r.stderr).strip()[:160]}", flush=True)
        mine()
        bid_log.write(json.dumps({"height": tip + 1, "bids": bids, "keys": keys}) + "\n")
        bid_log.flush()
        if i % 25 == 0:
            print(f"block {tip + 1}: round {s['round']} price {s['price']} pool {s['streak_pool']} "
                  f"pot {s['pot']} left {blocks_left} keys {keys}", flush=True)

    mine(2)
    csv = f"{DIR}/sim-bmm.csv"
    print(sp("scan", "http://127.0.0.1:18943", COOKIE, SLOT, start, env={"SIDEPOT_QUIET": "1", "SIDEPOT_BMM_CSV": csv}))
    print(f"posted {posted_bids} M8 bids; per-block rows in {csv}")
    print("producer ids:", {pr.name: pr.id[:8] for pr in producers})


if __name__ == "__main__":
    main()
