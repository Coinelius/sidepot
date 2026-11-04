#!/usr/bin/env python3
"""Summarise a regtest_sim.py run from the scan CSV and the bid log.

    sim_report.py <prefix>        reads <prefix>-bmm.csv and <prefix>-bids.jsonl
"""
import csv
import json
import sys
from collections import defaultdict

ECX = 1e8
NAMES = {"a0a0a0a0": "steady", "a1a1a1a1": "defender", "a2a2a2a2": "opportunist", "a3a3a3a3": "miner"}
prefix = sys.argv[1]
rows = list(csv.DictReader(open(f"{prefix}-bmm.csv")))
log = {r["height"]: r for r in map(json.loads, open(f"{prefix}-bids.jsonl"))}

rounds = defaultdict(lambda: {"blocks": 0, "volume": 0, "outside": 0, "self": 0, "bonus": 0, "bids": []})
producers = defaultdict(lambda: {"won": 0, "paid": 0, "self": 0, "bonus": 0})
reigns = defaultdict(list)
prev_pool, holder, length = 0, None, 0
contested = top = breaks = self_blocks = 0
for r in rows:
    h, pool, bid, bonus = int(r["height"]), int(r["streak_pool"]), int(r["bid"]), int(r["bonus"])
    entry = log.get(h, {})
    rd = rounds[r["round"]]
    rd["blocks"] += 1
    # Buys add STREAK_SHARE (25%) of their cost to the pool; bonuses take from it.
    rd["volume"] += (pool - prev_pool + bonus) * 4
    prev_pool = pool
    winner = NAMES.get(r["producer"], r["producer"]) if r["producer"] else None
    if winner:
        p = producers[winner]
        p["won"] += 1
        p["bonus"] += bonus
        rd["bonus"] += bonus
        rd["bids"].append(bid)
        if entry.get("self_accept") and winner == "miner":
            self_blocks += 1
            p["self"] += bid
            rd["self"] += bid
            breaks += holder not in (None, "miner")
        else:
            p["paid"] += bid
            rd["outside"] += bid
            posted = entry.get("bids", {})
            if len(posted) > 1:
                contested += 1
                top += bid == max(posted.values())
    # Reigns: runs of consecutive blocks won by one producer.
    if winner == holder and winner:
        length += 1
    else:
        if holder:
            reigns[holder].append(length)
        holder, length = winner, 1 if winner else 0
if holder:
    reigns[holder].append(length)

total_volume = sum(rd["volume"] for rd in rounds.values())
paid = sum(rd["outside"] for rd in rounds.values())
selfpaid = sum(rd["self"] for rd in rounds.values())
miner_blocks = sum(1 for e in log.values() if e.get("miner_block"))
print(f"{len(rows)} L1 blocks; the miner mined {miner_blocks} and accepted its own M8 in {self_blocks} "
      f"({breaks} of them broke another producer's streak)\n")
print("round  blocks  buy volume    bids paid to miners  self-paid by miner  max bid     bonuses")
for k, rd in sorted(rounds.items(), key=lambda kv: int(kv[0])):
    print(f"{k:>5}  {rd['blocks']:>6}  {rd['volume'] / ECX:>8.3f} ECX  {rd['outside'] / ECX:>12.4f} ECX"
          f"  {rd['self'] / ECX:>13.4f} ECX  {max(rd['bids'], default=0) / ECX:>7.4f}  {rd['bonus'] / ECX:>9.4f}")
print(f"\nall    {len(rows):>6}  {total_volume / ECX:>8.3f} ECX  {paid / ECX:>12.4f} ECX  {selfpaid / ECX:>13.4f} ECX")
print(f"bids paid to other miners = {paid / total_volume:.1%} of buy volume; streak pool left {prev_pool / ECX:.4f} ECX")
print(f"highest posted bid won {top} of {contested} contested auctions in blocks the miner did not mine")
print("\nproducer      won   bids paid  self-paid   bonuses     profit   reigns  mean  longest")
for name, p in sorted(producers.items(), key=lambda kv: -kv[1]["won"]):
    rs = reigns[name]
    print(f"{name:<12} {p['won']:>4}  {p['paid'] / ECX:>9.4f}  {p['self'] / ECX:>9.4f}  {p['bonus'] / ECX:>9.4f}"
          f"  {(p['bonus'] - p['paid']) / ECX:>+9.4f}  {len(rs):>6}  {sum(rs) / len(rs):>4.1f}  {max(rs):>7}")
print("\n(profit = bonuses minus bids paid to others; a miner's self-paid bids return to it)")
