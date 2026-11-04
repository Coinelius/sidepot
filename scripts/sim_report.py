#!/usr/bin/env python3
"""Summarise a regtest_sim.py run from the scan CSV and the bid log.

    sim_report.py <sim-bmm.csv> <sim-bids.jsonl> [names as id8=name ...]
"""
import csv
import json
import sys
from collections import defaultdict

ECX = 1e8
rows = list(csv.DictReader(open(sys.argv[1])))
bids = {r["height"]: r["bids"] for r in map(json.loads, open(sys.argv[2]))}
names = dict(a.split("=") for a in sys.argv[3:])

rounds = defaultdict(lambda: {"blocks": 0, "volume": 0, "bids": [], "bonus": 0, "won": 0})
producers = defaultdict(lambda: {"won": 0, "bid": 0, "bonus": 0})
prev_pool, longest, run, last = 0, 0, 0, None
checked = top = 0
for r in rows:
    h, pool, bid, bonus = int(r["height"]), int(r["streak_pool"]), int(r["bid"]), int(r["bonus"])
    rd = rounds[r["round"]]
    rd["blocks"] += 1
    # Buys add STREAK_SHARE (25%) of their cost to the pool; bonuses take from it.
    rd["volume"] += (pool - prev_pool + bonus) * 4
    prev_pool = pool
    if r["producer"]:
        rd["bids"].append(bid)
        rd["bonus"] += bonus
        rd["won"] += 1
        p = producers[r["producer"]]
        p["won"] += 1
        p["bid"] += bid
        p["bonus"] += bonus
        run = run + 1 if r["producer"] == last else 1
        longest = max(longest, run)
        posted = bids.get(h, {})
        if len(posted) > 1:
            checked += 1
            top += bid == max(posted.values())
    else:
        run = 0
    last = r["producer"] or None

total_volume = sum(rd["volume"] for rd in rounds.values())
total_bids = sum(sum(rd["bids"]) for rd in rounds.values())
print(f"{len(rows)} L1 blocks, {sum(rd['won'] for rd in rounds.values())} with a Sidepot BMM accept\n")
print("round  blocks  buy volume   bids to miners  bids/volume  mean bid    max bid    bonuses")
for k, rd in sorted(rounds.items(), key=lambda kv: int(kv[0])):
    b = rd["bids"]
    mean = sum(b) / len(b) if b else 0
    ratio = sum(b) / rd["volume"] if rd["volume"] else 0
    print(f"{k:>5}  {rd['blocks']:>6}  {rd['volume'] / ECX:>8.3f} ECX  {sum(b) / ECX:>10.4f} ECX  {ratio:>10.1%}"
          f"  {mean / ECX:>8.4f}  {max(b, default=0) / ECX:>8.4f}  {rd['bonus'] / ECX:>8.4f}")
print(f"\nall    {len(rows):>6}  {total_volume / ECX:>8.3f} ECX  {total_bids / ECX:>10.4f} ECX"
      f"  {total_bids / total_volume if total_volume else 0:>10.1%}   (model: STREAK_SHARE = 25% once the pool has passed on)")
print(f"streak pool left: {prev_pool / ECX:.4f} ECX; longest streak {longest} blocks")
print(f"highest posted bid won {top} of {checked} contested auctions")
print("\nproducer      won   bids paid   bonuses    profit")
for pid, p in sorted(producers.items(), key=lambda kv: -kv[1]["won"]):
    print(f"{names.get(pid, pid):<12} {p['won']:>4}  {p['bid'] / ECX:>9.4f}  {p['bonus'] / ECX:>9.4f}"
          f"  {(p['bonus'] - p['bid']) / ECX:>+8.4f} ECX")
