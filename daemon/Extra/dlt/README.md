# dlt — Hedera Payments Plugin (Optional)

Everything ARIA needs to touch the Hedera network lives here and **only**
here. The daemon core discovers this plugin at runtime (directory scan in
`src/skills/paths.rs`, host code wired via `#[path]`) and never requires
it: delete this directory and the daemon still builds and runs fully
offline.

## Contents

- `skills/pay/` — WASM skill crates, same layout as `daemon/skills/`:
  - `x402.pay` — pays paywalled URLs via the Hedera x402 flow
    (`capabilities.x402_pay`), then returns the unlocked content.
  - `transfer.pay` — direct HBAR transfer to a known Hedera account
    (`capabilities.hedera_pay`).
  - `query.pay` — reads the agent's own recent Hedera payments from the
    local DB (`capabilities.db_query`). DLT-adjacent (payment history),
    so it moves with the rest.
- `host/` — host-side Rust modules, wired into the daemon as
  `crate::payments` via `#[path = "../Extra/dlt/host/mod.rs"]` in
  `src/main.rs` / `src/lib.rs`. Callers use `crate::payments::*`
  unchanged:
  - `direct.rs` (`PaymentVault`), `x402_vault.rs` (`X402PaymentVault`) —
    operator clients for transfers and x402.
  - `audit.rs` / `task_anchor.rs` — HCS audit writes + per-task seal
    anchor. Failures never fail the task they annotate.
  - `mirror.rs` — Hedera Mirror Node read for chain-verifying payment
    history (degrades to locally-cached status when unreachable).
  - `governance.rs`, `facilitator_client.rs`, `x402_types.rs` — spend
    keys, facilitator HTTP client, x402 protocol types.

## Required keys (env)

- `HEDERA_ACCOUNT_ID` — operator account, e.g. `0.0.12345`.
- `HEDERA_PRIVATE_KEY` — operator **ECDSA (secp256k1)** private key.
  Pays gas for identity bootstrap, transfers, and HCS writes.
- `HEDERA_NETWORK` — `testnet` (default), `mainnet`, or `previewnet`.
- `X402_FACILITATOR_URL` — x402 facilitator (default
  `https://x402.org/facilitator`).

Without `HEDERA_ACCOUNT_ID` / `HEDERA_PRIVATE_KEY` no vault is built:
wallet-balance queries error, payment skills pause with a clear message,
and identity bootstrap prompts on a TTY (headless: hard error).

## Costs (all paid in HBAR from the operator account)

- Direct transfer (`transfer.pay`): the sent amount + network fee
  (~0.0001–0.001 HBAR per transaction, network-priced).
- x402 pay (`x402.pay`): resource price (set by the seller) + network fee.
- Every HCS write (audit log entry, per-task seal anchor, audit-topic
  creation): a ConsensusSubmitMessage / TopicCreate fee
  (~0.001–0.05 HBAR, network-priced). Anchors are fire-and-forget and
  never block task completion.
- Mirror Node reads (`query.pay` chain verification): free (HTTPS GET).

## Air-gap behavior (`dlt_enabled=false`)

Toggle live via `aria dlt off`, TCP `mutate_dlt`, or env
`ARIA_DLT_ENABLED=0` (env wins over the DB flag; default ON):

- `transfer.pay` / `x402.pay` disappear from the system prompt and the
  native tool list, so the model never sees them.
- Enforcement points refuse DLT paths; the per-task HCS anchor becomes a
  no-op; no packets leave the host for Hedera, the mirror node, or the
  x402 facilitator.
- `query.pay` (local-DB payment history) keeps working offline; only its
  live chain-verification column degrades to cached values.
