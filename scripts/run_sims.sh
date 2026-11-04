#!/usr/bin/env bash
# Runs the ramp-3 and flat-cap (ramp 0) simulations back to back on one regtest chain and writes report.txt.
# Detach it (nohup setsid) on the regtest host; both.done appears when it finishes.
set -u
DIR=${SIDEPOT_REGTEST:-/opt/sidepot-regtest}
BLOCKS=${1:-330}
SEED=${2:-11}
cd "$DIR"
rm -f both.done
SIM_OUT=$DIR/ramp3 python3 regtest_sim.py "$BLOCKS" "$SEED" > ramp3.log 2>&1
SIDEPOT_BONUS_RAMP=0 SIM_OUT=$DIR/ramp0 python3 regtest_sim.py "$BLOCKS" "$SEED" > ramp0.log 2>&1
{
    echo "=== RAMP 3 (as now) + 30% miner"; python3 sim_report.py ramp3
    echo; echo "=== RAMP 0 (flat cap) + 30% miner"; python3 sim_report.py ramp0
    echo; echo "failed bids: $(grep -c failed ramp3.log) / $(grep -c failed ramp0.log)"
} > report.txt 2>&1
touch both.done
