#!/usr/bin/env bash
# End-to-end check on the regtest stack: BIP300 deposits into game accounts, signed entries in real L1
# transactions mined from the enforcer's template, then the whole game replayed from the chain with
# sidepot-cli scan. Run on the host that runs the regtest node, enforcer (with wallet) and Fulcrum.
set -euo pipefail
DIR=${SIDEPOT_REGTEST:-/opt/sidepot-regtest}
SLOT=88
CLI="/usr/local/bin/trixie-exec /opt/ecash-1.2.0/bin/bitcoin-cli -datadir=$DIR/node -rpcwallet=test"
SP=${SIDEPOT_CLI:-/root/sidepot-build/target/release/sidepot-cli}
MINE="python3 $DIR/regtest_miner.py"
GRPC=127.0.0.1:50153
# The L1 block that mined the slot proposal: entries are signed for its hash and the slot.
ANCHOR_HEIGHT=${SIDEPOT_ANCHOR_HEIGHT:-105}
ANCHOR=$($CLI getblockhash "$ANCHOR_HEIGHT")
RPC=http://127.0.0.1:18943
COOKIE=$DIR/node/regtest/.cookie
# Signs an entry good for the next three ticks of the game clock (L1 blocks with transactions):
# sign buy|cashout <seckey> <kind arguments...> <nonce>
sign() { local kind=$1 sk=$2; shift 2; $SP "$kind" "$sk" "$ANCHOR" $SLOT "$@" $(( $($SP tick $RPC "$COOKIE" "$START") + 3 )); }

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

# A BIP300 deposit of 1 ECX to slot 88, addressed to a game account, from the enforcer wallet.
deposit() {
    grpcurl -plaintext -d "{\"sidechainId\":$SLOT,\"address\":\"spot:$1\",\"valueSats\":\"100000000\",\"feeSats\":\"10000\"}" \
        $GRPC cusf.mainchain.v1.WalletService/CreateDepositTransaction >/dev/null
}

START=$(( $($CLI getblockcount) + 1 ))
echo "players A=${A:0:8} B=${B:0:8}; game starts at L1 $START"

# Block START: both deposit 1 ECX into their game accounts; A buys 10 keys in the same block.
deposit "$A"
$MINE 1 >/dev/null
deposit "$B"
post "$(sign buy "$A_SK" 1 10 1000000 1)" >/dev/null
$MINE 1 >/dev/null

# Next block: B buys 5 keys; A replays nonce 1; B also tries to buy at the start price.
post "$(sign buy "$B_SK" 1 5 2000000 1)" >/dev/null
post "$(sign buy "$A_SK" 1 10 2000000 1)" >/dev/null
post "$(sign buy "$B_SK" 1 1 1000000 2)" >/dev/null
$MINE 1 >/dev/null

# Run past the round's deadline: empty blocks do not tick the game clock, so every block carries a transaction.
# Then A cashes out everything to a sidechain address.
for _ in $(seq 146); do
    $CLI sendtoaddress "$($CLI getnewaddress)" 0.001 >/dev/null
    $MINE 1 >/dev/null
done
post "$(sign cashout "$A_SK" 0 "$(printf 'aa%.0s' {1..20})" 2)" >/dev/null
# The cashout is mined in the block after next, and the replay stops two blocks below the tip.
$MINE 4 >/dev/null
$SP scan $RPC "$COOKIE" $SLOT "$ANCHOR_HEIGHT" "$START"
