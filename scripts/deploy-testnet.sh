#!/usr/bin/env bash
# Deploys the SafeNear factory to testnet.
# Needs: near-cli-rs  ->  cargo install near-cli-rs   (or: npm i -g near-cli-rs)
#
# Usage:  MASTER=tagabukid.testnet ./scripts/deploy-testnet.sh
# Creates safenear.<MASTER> (e.g. safenear.tagabukid.testnet) and deploys the factory there.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MASTER:?Set MASTER to your testnet account, e.g. MASTER=tagabukid.testnet}"
FACTORY="${FACTORY:-safenear.$MASTER}"
FUND="${FUND:-9 NEAR}"          # factory storage is roughly 2x the token wasm size

# Testnet defaults: small numbers so you can fill a curve with faucet NEAR.
CREATION_FEE="5000000000000000000000000"          # 5 NEAR total paid by the creator
TOKEN_ACCOUNT_BALANCE="4500000000000000000000000" # 4.5 NEAR goes to the new token account
GRADUATION="5000000000000000000000000"            # curve graduates at 5 NEAR raised
VIRTUAL_NEAR="2000000000000000000000000"          # 2 NEAR virtual reserve (sets start price)
REF="ref-finance-101.testnet"
WRAP="wrap.testnet"

[ -f res/safenear_factory.wasm ] || ./scripts/build.sh

echo "==> Creating $FACTORY funded with $FUND from $MASTER"
near account create-account fund-myself "$FACTORY" "$FUND" \
  autogenerate-new-keypair save-to-keychain \
  sign-as "$MASTER" network-config testnet sign-with-keychain send

echo "==> Deploying factory"
near contract deploy "$FACTORY" use-file res/safenear_factory.wasm \
  with-init-call new json-args "{
    \"owner\": \"$MASTER\",
    \"creation_fee\": \"$CREATION_FEE\",
    \"token_account_balance\": \"$TOKEN_ACCOUNT_BALANCE\",
    \"graduation_threshold\": \"$GRADUATION\",
    \"virtual_near\": \"$VIRTUAL_NEAR\",
    \"ref_contract\": \"$REF\",
    \"wrap_contract\": \"$WRAP\"
  }" \
  prepaid-gas '100 Tgas' attached-deposit '0 NEAR' \
  network-config testnet sign-with-keychain send

echo
echo "Done. Open the frontend with:"
echo "  safenear-launchpad.html?factory=$FACTORY"
