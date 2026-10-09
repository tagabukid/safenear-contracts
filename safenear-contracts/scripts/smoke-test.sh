#!/usr/bin/env bash
# Launches a test token and makes one buy, straight from the CLI.
# Usage: MASTER=tagabukid.testnet ./scripts/smoke-test.sh
set -euo pipefail
: "${MASTER:?Set MASTER}"
FACTORY="${FACTORY:-safenear.$MASTER}"
SYM="${SYM:-TEST$RANDOM}"
TOKEN="$(echo "$SYM" | tr 'A-Z' 'a-z').$FACTORY"

near contract call-function as-transaction "$FACTORY" create_token \
  json-args "{\"name\":\"Smoke $SYM\",\"symbol\":\"$SYM\",\"tax_bps\":100}" \
  prepaid-gas '300 Tgas' attached-deposit '5 NEAR' \
  sign-as "$MASTER" network-config testnet sign-with-keychain send

near contract call-function as-transaction "$TOKEN" storage_deposit \
  json-args "{\"account_id\":\"$MASTER\",\"registration_only\":true}" \
  prepaid-gas '30 Tgas' attached-deposit '0.00125 NEAR' \
  sign-as "$MASTER" network-config testnet sign-with-keychain send

near contract call-function as-transaction "$TOKEN" buy \
  json-args '{"min_tokens_out":"0"}' \
  prepaid-gas '100 Tgas' attached-deposit '1 NEAR' \
  sign-as "$MASTER" network-config testnet sign-with-keychain send

near contract call-function as-read-only "$TOKEN" get_curve json-args '{}' \
  network-config testnet now
