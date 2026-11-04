#!/usr/bin/env python3
"""Summarise a regtest_sim.py run from the scan CSV and the bid log.

    sim_report.py <prefix>        reads <prefix>-bmm.csv and <prefix>-bids.jsonl
"""
import csv
import json
import sys
from collections import defaultdict

ECX = 1e8
NAMES = {"a0a0a0a0": "steady", "a1a1a1a1": "defender", "a2a2a2a2": "opportunist",
         "a4a4a4a4": "renter1", "a5a5a5a5": "renter2", "a6a6a6a6": "renter3"}
prefix = sys.argv[1]
rows = list(csv.DictReader(open(f"{prefix}-bmm.csv")))
log = {r["height"]: r for r in map(json.loads, open(f"{prefix}-bids.jsonl"))}

rounds = defaultdict(lambda: {"blocks": 0, "volume": 0, "to_miners": 0, "self": 0, "bonus": 0, "bids": []})
producers = defaultdict(lambda: {"won": 0, "paid": 0, "bonus": 0})
renters = defaultdict(lambda: {"bursts": 0, "found": 0, "self": 0, "captured": 0, "forgone": 0, "collected": 0})
earned_by_ordinary = 0
prev_pool, contested, top = 0, 0, 0
was_renting = set()
for r in rows:
    h, pool, bid, bonus = int(r["height"]), int(r["streak_pool"]), int(r["bid"]), int(r["bonus"])
    entry = log.get(h, {})
    rd = rounds[r["round"]]
    rd["blocks"] += 1
    # Buys add STREAK_SHARE (25%) of their cost to the pool; bonuses take from it.
    rd["volume"] += (pool - prev_pool + bonus) * 4
    prev_pool = pool
    renting = set(entry.get("renting", {}))
    for name in renting - was_renting:
        renters[name]["bursts"] += 1
    was_renting = renting
    miner = entry.get("miner")
    winner = NAMES.get(r["producer"], r["producer"]) if r["producer"] else None
    if miner:
        renters[miner]["found"] += 1
    if not winner:
        continue
    rd["bonus"] += bonus
    rd["bids"].append(bid)
    if entry.get("self_accept") and winner == miner:
        rt = renters[miner]
        rt["self"] += 1
        rt["captured"] += bonus
        rt["forgone"] += entry.get("best_outside", 0)
        rd["self"] += bid
        continue
    producers[winner]["won"] += 1
    producers[winner]["paid"] += bid
    producers[winner]["bonus"] += bonus
    rd["to_miners"] += bid
    if miner:
        renters[miner]["collected"] += bid
    else:
        earned_by_ordinary += bid
    posted = entry.get("bids", {})
    if len(posted) > 1 and not miner:
        contested += 1
        top += bid == max(posted.values())

total_volume = sum(rd["volume"] for rd in rounds.values())
to_miners = sum(rd["to_miners"] for rd in rounds.values())
won = sum(len(rd["bids"]) for rd in rounds.values())
print(f"{len(rows)} L1 blocks, {won} with a Sidepot BMM accept\n")
print("round  blocks  buy volume   bids paid to miners  renter self-accepts  max bid   bonuses")
for k, rd in sorted(rounds.items(), key=lambda kv: int(kv[0])):
    print(f"{k:>5}  {rd['blocks']:>6}  {rd['volume'] / ECX:>8.3f} ECX  {rd['to_miners'] / ECX:>11.4f} ECX"
          f"  {rd['self'] / ECX:>13.4f} ECX  {max(rd['bids'], default=0) / ECX:>7.4f}  {rd['bonus'] / ECX:>8.4f}")
print(f"\nall    {len(rows):>6}  {total_volume / ECX:>8.3f} ECX  {to_miners / ECX:>11.4f} ECX")
print(f"bids paid to miners = {to_miners / total_volume:.1%} of buy volume "
      f"(ordinary miners {earned_by_ordinary / ECX:.4f} ECX, renters {(to_miners - earned_by_ordinary) / ECX:.4f} ECX); "
      f"streak pool left {prev_pool / ECX:.4f} ECX")
print(f"highest posted bid won {top} of {contested} contested auctions in ordinary miners' blocks")
print("\nproducer      won   bids paid    bonuses     profit")
for name, p in sorted(producers.items(), key=lambda kv: -kv[1]["won"]):
    print(f"{name:<12} {p['won']:>4}  {p['paid'] / ECX:>9.4f}  {p['bonus'] / ECX:>9.4f}  {(p['bonus'] - p['paid']) / ECX:>+9.4f}")
print("\nrenter    bursts  blocks found  self-accepts  bonus captured  outside bids forgone  edge   outside bids collected")
for name, rt in sorted(renters.items()):
    print(f"{name:<9} {rt['bursts']:>6}  {rt['found']:>12}  {rt['self']:>12}  {rt['captured'] / ECX:>14.4f}"
          f"  {rt['forgone'] / ECX:>20.4f}  {(rt['captured'] - rt['forgone']) / ECX:>+6.4f}  {rt['collected'] / ECX:>12.4f}")
print("\n(edge = bonuses a renter captured in its own blocks minus the outside bids it gave up there)")
