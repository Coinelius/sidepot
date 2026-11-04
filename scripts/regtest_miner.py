#!/usr/bin/env python3
"""Mine regtest blocks from the enforcer's block template, so they carry its M1-M4 and BMM messages.

    regtest_miner.py [blocks]

Fetches getblocktemplate (coinbasetxn) from the enforcer, grinds the nonce against the trivial regtest target and
submits the block to the node. Settings come from the environment: ENFORCER_RPC (default http://127.0.0.1:18124),
NODE_RPC (default http://127.0.0.1:18943) and NODE_COOKIE (default /opt/sidepot-regtest/node/regtest/.cookie).
"""
import base64
import hashlib
import json
import os
import struct
import sys
import time
import urllib.request

ENFORCER_RPC = os.environ.get("ENFORCER_RPC", "http://127.0.0.1:18124")
NODE_RPC = os.environ.get("NODE_RPC", "http://127.0.0.1:18943")
NODE_COOKIE = os.environ.get("NODE_COOKIE", "/opt/sidepot-regtest/node/regtest/.cookie")


def rpc(url, method, params, auth=None):
    req = urllib.request.Request(url, json.dumps({"jsonrpc": "2.0", "id": 0, "method": method, "params": params}).encode(),
                                 {"Content-Type": "application/json"})
    if auth:
        req.add_header("Authorization", "Basic " + base64.b64encode(auth.encode()).decode())
    with urllib.request.urlopen(req, timeout=120) as resp:
        reply = json.load(resp)
    if reply.get("error"):
        raise RuntimeError(f"{method}: {reply['error']}")
    return reply["result"]


def node(method, *params):
    return rpc(NODE_RPC, method, list(params), open(NODE_COOKIE).read().strip())


def dsha(b):
    return hashlib.sha256(hashlib.sha256(b).digest()).digest()


def varint(n):
    if n < 0xfd:
        return bytes([n])
    return b"\xfd" + struct.pack("<H", n) if n <= 0xffff else b"\xfe" + struct.pack("<I", n)


def strip_witness(tx):
    """The legacy serialisation of a transaction, for its txid."""
    if tx[4:6] != b"\x00\x01":
        return tx
    i = 6

    def vi(i):
        n = tx[i]
        if n < 0xfd:
            return n, i + 1
        k = {0xfd: 2, 0xfe: 4, 0xff: 8}[n]
        return int.from_bytes(tx[i + 1:i + 1 + k], "little"), i + 1 + k

    nin, i = vi(i)
    for _ in range(nin):
        i += 36
        n, i = vi(i)
        i += n + 4
    nout, i = vi(i)
    for _ in range(nout):
        i += 8
        n, i = vi(i)
        i += n
    return tx[:4] + tx[6:i] + tx[-4:]


def merkle_root(hashes):
    while len(hashes) > 1:
        if len(hashes) % 2:
            hashes.append(hashes[-1])
        hashes = [dsha(hashes[i] + hashes[i + 1]) for i in range(0, len(hashes), 2)]
    return hashes[0]


def mine_one():
    # The enforcer sees a new tip a moment after the node does; wait for a template that builds on it.
    tip = node("getblockcount")
    while True:
        t = rpc(ENFORCER_RPC, "getblocktemplate", [{"rules": ["segwit"], "capabilities": ["coinbasetxn"]}])
        if t["height"] > tip:
            break
        time.sleep(0.05)
    coinbase = bytes.fromhex(t["coinbasetxn"]["data"])
    txs = [coinbase] + [bytes.fromhex(x["data"]) for x in t["transactions"]]
    root = merkle_root([dsha(strip_witness(tx)) for tx in txs])
    target = int(t["target"], 16)
    head = struct.pack("<I", t["version"]) + bytes.fromhex(t["previousblockhash"])[::-1] + root \
        + struct.pack("<I", max(t["curtime"], t.get("mintime", 0))) + bytes.fromhex(t["bits"])[::-1]
    for nonce in range(2 ** 32):
        header = head + struct.pack("<I", nonce)
        if int.from_bytes(dsha(header), "little") <= target:
            break
    block = header + varint(len(txs)) + b"".join(txs)
    result = node("submitblock", block.hex())
    if result is not None:
        raise RuntimeError(f"submitblock rejected block {t['height']}: {result}")
    return t["height"], dsha(header)[::-1].hex()


if __name__ == "__main__":
    for _ in range(int(sys.argv[1]) if len(sys.argv) > 1 else 1):
        height, block_hash = mine_one()
        print(height, block_hash)
