#!/usr/bin/env bash
# Builds the token first (the factory embeds it), then the factory.
# Needs: Rust (rustup) and cargo-near  ->  cargo install cargo-near
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p res

echo "==> Building token"
(cd token && cargo near build non-reproducible-wasm --no-abi)
cp "$(find target -path '*near*' -name 'safenear_token.wasm' | head -n1)" res/safenear_token.wasm

echo "==> Building factory (embeds res/safenear_token.wasm)"
(cd factory && cargo near build non-reproducible-wasm --no-abi)
cp "$(find target -path '*near*' -name 'safenear_factory.wasm' | head -n1)" res/safenear_factory.wasm

ls -lh res/*.wasm
