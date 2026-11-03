#!/usr/bin/env bash
# End-to-end check on the regtest stack: real signed entries in real L1 transactions, mined from the enforcer's
# template, then replayed from the chain with sidepot-cli scan. Run on the host that runs the regtest node.
set -euo pipefail
DIR=${SIDEPOT_REGTEST:-/opt/sidepot-regtest}
CLI="/usr/local/bin/trixie-exec /opt/ecash-1.2.0/bin/bitcoin-cli -datadir=$DIR/node -rpcwallet=test"
SP=${SIDEPOT_CLI:-/root/sidepot-build/target/release/sidepot-cli}
MINE="python3 $DIR/regtest_miner.py"

A_SK=$(printf '11%.0s' {1..32}); B_SK=$(printf '22%.0s' {1..32})
A=$($SP pubkey "$A_SK"); B=$($SP pubkey "$B_SK")

# Posts one entry as an OP_RETURN output of a wallet-funded L1 transaction; prints the txid.
post() {
    local raw funded signed
    raw=$($CLI createrawtransaction '[]' "[{\"data\":\"$1\"}]")
    funded=$($CLI fundrawtransaction "$raw" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hex"])')
    signed=$($CLI signrawtransactionwithwallet "$funded" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hex"])')
    $CLI sendrawtransaction "$signed"
}

START=$(( $($CLI getblockcount) + 1 ))
echo "players A=${A:0:8} B=${B:0:8}; game starts at L1 $START"

# Block START: A buys 10 keys at the start price.
post "$($SP entry "$A_SK" 1 10 1000000 1)" >/dev/null
$MINE 1 >/dev/null

# Block START+1: B buys 5 keys; A replays nonce 1 (now with room on price); B also tries to buy at the start price.
post "$($SP entry "$B_SK" 1 5 2000000 1)" >/dev/null
post "$($SP entry "$A_SK" 1 10 2000000 1)" >/dev/null
post "$($SP entry "$B_SK" 1 1 1000000 2)" >/dev/null
$MINE 1 >/dev/null

# Run past the round's deadline (START + 144), plus the processing depth.
$MINE 148 >/dev/null
$SP scan "http://127.0.0.1:18943" "$DIR/node/regtest/.cookie" "$START" "$A:100000000" "$B:100000000"
