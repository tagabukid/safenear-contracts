# SafeNear contracts

Two Rust contracts for the SafeNear launchpad on NEAR.

| Contract | What it does |
|---|---|
| `factory` | Launches tokens. Each token gets its own account `<ticker>.<factory>` with **no access keys**, keeps the token list the frontend reads. |
| `token` | NEP-141 token with a NEAR bonding curve built in. Buy and sell against the curve; when it fills, liquidity moves to Ref Finance and is locked. |

## Safety rules (enforced in code)

- **Fixed supply.** 1,000,000,000 tokens, minted once in `new`. No mint function exists.
- **No admin, no upgrades.** Token accounts are created without keys, so nobody can change the code or pull funds.
- **Tax cap.** Creator tax is 0 to 10%, set at launch, cannot change.
- **Locked liquidity.** At graduation all raised NEAR plus 200M tokens go into a Ref Finance pool. The LP shares sit in the token contract's own Ref account and there is no withdraw method.
- **Fair curve.** 800M tokens sell on a constant-product curve. The last buy is capped at the graduation threshold and any extra NEAR is refunded.

## Token supply

| Share | Amount | Where it goes |
|---|---|---|
| 80% | 800M | Sold on the bonding curve |
| 20% | 200M | Paired with raised NEAR in the Ref pool at graduation |

Creators get nothing for free; they buy on the curve like everyone else.

## Build

```bash
# one-time
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install cargo-near near-cli-rs

./scripts/build.sh          # builds token first, then the factory (which embeds the token)
cargo test -p safenear_token # curve math tests
```

## Deploy to testnet

1. Make a testnet account and get faucet NEAR (you need about 15 NEAR: 9 for the factory, 5 to launch a token, plus gas).
2. Log in so the CLI has your key: `near account import-account`
3. Deploy:

```bash
MASTER=tagabukid.testnet ./scripts/deploy-testnet.sh
```

This creates `safenear.tagabukid.testnet`. To use a short name like `safenear.testnet` instead, create that account first
(`near account create-account sponsor-by-faucet-service safenear.testnet ...`) and run with `FACTORY=safenear.testnet`.

4. Try it from the CLI: `MASTER=tagabukid.testnet ./scripts/smoke-test.sh`
5. Open the frontend: `safenear-launchpad.html?factory=safenear.tagabukid.testnet`

Testnet defaults: 5 NEAR creation fee, curve graduates at **5 NEAR**, so you can test the whole flow with faucet funds.
Use real numbers for mainnet (for example a 300+ NEAR threshold) via `set_config`.

## Graduation flow

When the curve hits the threshold, trading stops and anyone calls `migrate()` (300 Tgas) about 7 times:

| Step | Action |
|---|---|
| 0 | Storage on Ref and wNEAR for the token contract |
| 1 | Register wNEAR + token in its Ref account |
| 2 | Wrap the raised NEAR into wNEAR |
| 3 | Deposit wNEAR into Ref |
| 4 | Deposit the 200M LP tokens into Ref |
| 5 | Create the Ref simple pool (0.3% fee) |
| 6 | Add liquidity. LP is now locked. |

Each step only advances if it succeeded, so a failed step can just be called again. The frontend shows a
"Finish graduation" button for this.

## Testnet points (airdrop campaign)

The factory keeps a points table. Token contracts report activity to it; only tokens the factory launched can report.

| Action | Points |
|---|---|
| Launch a token | 50 (first 3 launches per day) |
| Buy | 10 per NEAR (1 per 0.1 NEAR) |
| Sell | 2 per NEAR |
| Your token graduates | 500 to the creator |
| Push a graduation step (`migrate`) | 10 |

Buy and sell points share a **300 per day cap** per account, so wash trading can't run away.

Users link their mainnet wallet with `link_mainnet_account`. At snapshot time:

```bash
near contract call-function as-transaction safenear.tagabukid.testnet set_points_open \
  json-args '{"open":false}' prepaid-gas '30 Tgas' attached-deposit '0 NEAR' \
  sign-as tagabukid.testnet network-config testnet sign-with-keychain send
node scripts/export-points.mjs safenear.tagabukid.testnet > points.csv
```

Testnet accounts and NEAR are free, so one person can farm with many accounts. Review the CSV before paying out
(for example: same mainnet wallet linked by many accounts, accounts created the same minute, trades only between
each other) and say clearly on the site that farmed points can be removed.

## Before mainnet

- These contracts have **not** been compiled or tested on-chain yet. Build, run `cargo test`, and run the full flow on testnet, including graduation to Ref.
- Confirm the Ref Finance method names and storage amounts against the current Ref contract (`ref-finance-101.testnet` / `v2.ref-finance.near`).
- Measure `get_config().token_code_bytes`; storage costs 1 NEAR per 100 KB, so `token_account_balance` must cover the token wasm plus about 0.3 NEAR for graduation deposits.
- Get an independent security review. These contracts will hold real user funds.
