# Known Issues — daemon audit



This file tracks confirmed defects and sharp edges found in the daemon, skill
runtime, bundled skills, payment layer, and identity/key-storage layer. Items
are ordered roughly by impact.

Validation run during this pass:

- `cargo check` — passes, with warnings.
- `cargo test` — fails: daemon lib 23 passed / 0 failed / 1 ignored; daemon
  binary 30 passed / 1 failed / 1 ignored. The failing test is
  `agent::react_loop::tests::ambiguous_reply_does_not_approve_and_regenerated_payment_is_confirmed_again`
  (assertion on payment confirmation string format, `react_loop.rs:1407`).
- `cargo test --workspace` — passes; several skill crates have 0 tests.
- Individual WASM skill builds:
  - `cargo build -p read_fs --target wasm32-wasip1 --release` — passes.
  - `cargo build -p search_web -p scrape_web -p find_fs -p pay_x402 -p query_payments --target wasm32-wasip1 --release` — passes.
  - `cargo build -p list_fs --target wasm32-wasip1 --release` — passes despite stub source.
  - `cargo build -p write_fs --target wasm32-wasip1 --release` — passes despite stub source.
- `cargo build --workspace --target wasm32-wasip1 --release` — fails because it tries to compile the daemon crate and `tokio` rejects the enabled WASM feature set.

---

## Critical / High

### 1. TCP request framing is a single read

`src/main.rs:990-1002` reads at most one 16 KiB chunk and immediately parses
that chunk as the entire request. Any request larger than the buffer,
fragmented by TCP, or sent slowly can fail as `Invalid JSON` even if the
client sent valid JSON. TCP does not preserve application message boundaries.

**Fix**: use newline-delimited JSON, length-prefixed frames, or read until EOF
with a bounded maximum request size before parsing.

### 2. Per-step audit signatures can mismatch stored audit rows

`src/main.rs:1162-1181` computes and signs an audit `chain_hash` using
`db.get_next_step_info(...)`. `src/db.rs:495-526` then recomputes the input
hash, result hash, timestamp, step index, previous hash, and chain hash inside
`log_task_step`. If the timestamp crosses a one-second boundary or the step
index / previous hash changes, the stored `chain_hash` no longer matches the
signature.

**Fix**: move all audit-row hash construction and signing into one path. Either
have DB return a fully formed unsigned row to sign and then insert unchanged,
or have the caller pass the already-signed `step`, `prev_hash`, `timestamp`,
and `chain_hash` into `log_task_step`.

### 3. `list.fs` and `write.fs` are advertised but not implemented

Both skill manifests advertise usable filesystem tools, but their sources are
one-line stubs:

- `skills/fs/list.fs/src/lib.rs:1`
- `skills/fs/write.fs/src/lib.rs:1`

They still compile to WASM, so build/test does not catch this. At runtime,
`src/skills/wasm_runtime.rs` requires exported `memory`, `alloc`, and `run`;
these stub modules cannot satisfy the advertised behavior.

**Fix**: implement both skills or remove their manifests/workspace members until
ready. Add a smoke test that loads every manifest-backed WASM and checks required
exports before release.

### 4. Native tool names use dots and can be rejected by OpenAI-compatible APIs

`src/agent/prompt.rs:226-256` emits manifest names directly as native function
names, e.g. `search.web`, `read.fs`, `pay.x402`. Many OpenAI-compatible
chat-completions tool schemas allow only alphanumeric, underscore, and hyphen
characters for function names. This can make native tool mode fail before model
execution.

**Fix**: introduce a reversible mapping for native tool names, such as
`search_web` <-> `search.web`, and map back before dispatching.

---

## Medium

### 5. Filesystem write resolution creates directories before sandbox boundary check

`src/skills/fs_sandbox.rs:100-115` calls `create_dir_all(parent)` for write
targets before checking `canonical.starts_with(&self.root)`. A denied absolute
path outside `fs_root` can still create directories before access is rejected.

**Fix**: canonicalize and boundary-check the nearest existing ancestor before
creating directories, then create only under the verified sandbox root.

### 6. WASM memory helpers trust signed pointer/length inputs too much

`src/skills/wasm_runtime.rs:699-709` accepts `i32` pointer and length values,
casts them to `usize`, and computes `(ptr + len)` before the bounds check.
Negative values or overflow from a malicious or corrupted guest can produce
surprising ranges or panic in debug builds.

**Fix**: reject negative pointers/lengths, use checked addition, and then
perform the `memory.data()` bounds check.

### 7. `search.web` does not handle `host_http_get` failure

`skills/web/search.web/src/lib.rs:174-191` unpacks and reads the packed result
without checking `packed == 0`. Other skills such as `scrape.web` correctly
treat `0` as host-call failure. This can turn an HTTP failure into unsafe guest
memory access behavior instead of a clean JSON error.

**Fix**: mirror the `packed == 0` guard used by `scrape.web`, `find.fs`,
`read.fs`, and payment skills.

### 8. `cargo build --workspace --target wasm32-wasip1` is not a valid release build

The full workspace WASM build fails because it includes `aria-daemon`, whose
`tokio`/`wasmtime` dependencies are not valid for WASM. The README currently
shows individual package build examples, but there is no checked script that
builds exactly the runnable skills and skips the daemon.

**Fix**: add a `build-skills` script/xtask that builds only skill packages for
`wasm32-wasip1`, and document that as the supported release command.

### 9. Direct payment HashScan links are always testnet

`src/payments/direct.rs:71` always formats
`https://hashscan.io/testnet/transaction/...` even when `HEDERA_NETWORK=mainnet`
or `previewnet`. The x402 settlement-confirmation path
(`src/skills/wasm_runtime.rs:1202-1205`) has the same hardcoded segment.

**Fix**: derive the HashScan network segment from the selected Hedera network.

### 10. Direct payment amount handling uses `f64`

`src/payments/direct.rs:45` accepts `amount_hbar: f64` and converts via
`(amount_hbar * 100_000_000.0) as i64`. This can silently truncate fractional
tinybars and does not reject non-positive, NaN, or infinite values before
building a transfer.

**Fix**: parse/accept decimal strings or integer tinybars, validate finite
positive values, and reject values that are not exactly representable in
tinybars.

### 11. Facilitator client has no timeout and logs raw responses to stderr

`src/payments/facilitator_client.rs` uses `reqwest::Client::new()` without a
timeout and has unconditional `eprintln!` debug logs for `/verify` and
`/settle` responses. A hung facilitator can stall payment flow, and raw payment
responses should not be printed in normal operation.

**Fix**: build the client with a timeout and replace unconditional prints with
`tracing::debug!` or structured, redacted logs.

---

## Lower Priority / Cleanup

### 12. OpenRouter/provider selection is compile-time hardcoded

`src/config.rs:17-23` hardcodes `Provider::Ollama`, the Ollama URL, and both
model names. `Provider::OpenRouter` is never constructed in normal code. This
makes deployment-specific provider selection require a source change.

**Fix**: load provider, base URL, and model from DB/env with sane defaults.

### 13. OpenRouter API-key prompt is interactive and unsuitable for services

If provider selection is changed to OpenRouter, `src/main.rs:865-887` prompts
on stdin when no key is in the DB. That is reasonable for a CLI setup flow but
bad for `aria daemon` under systemd.

**Fix**: fail fast with a clear config error in daemon mode, and keep prompting
only in an explicit interactive setup command.

### 14. `RuntimeConfig` is loaded once at startup

`src/main.rs:919` loads injected config once and clones it into every request.
Changes to DB-backed config such as `searxng_url`, `fs_root`, `fs_allow`, or
`node_url` do not affect a running daemon until restart.

**Fix**: reload injected config per request or add a config-watch / refresh
mechanism.

### 15. `Db` uses `Mutex<Connection>` and unwraps poisoned locks

Most DB methods call `self.conn.lock().unwrap()`. A panic while holding the DB
mutex would poison the lock and cause later daemon operations to panic instead
of returning an error.

**Fix**: map poisoned locks to `anyhow` errors consistently.

### 16. Identity export is not passphrase-gated

`FileVault` / `crypto::load_signing_key` decrypt using a key derived from
`device_secret` plus the DID string as salt. There is no user-supplied
passphrase gate for private key export. This is encryption-at-rest against a
narrow local threat model, but anything that can read the daemon user's files
can also read the device secret and decrypt the identity key.

**Fix**: add a separate passphrase-gated export path using its own random salt
and Argon2id derivation. Do not mutate the stored `id.key` blob for day-to-day
signing.

---

## Recently Fixed / Stale Notes Removed

The previous issue file contained several x402 notes that are now stale:

- `X402PaymentVault::pay()` now takes `skill_called` and `task_id` explicitly
  instead of smuggling them through `PaymentRequirements.extra`.
- The hardcoded Hedera node ID has been softened via `HEDERA_NODE_ACCOUNT_ID`
  with `0.0.3` as fallback.
- ECDSA transaction coverage exists in
  `payments::x402_types::tests::test_build_payment_transaction_ecdsa`.
- The payment DB field in `X402PaymentVault` is already `Arc<Db>`.

Additional items verified fixed and removed from the active lists:

- Daemon no longer crashes at startup without Hedera credentials — both
  `PaymentVault` and `X402PaymentVault` are now optional and built from a
  single shared operator client (`src/main.rs:920-931`).
- Global task-chain signatures now sign the stored task-chain link — the task
  ID, timestamp, and link hash are computed once and passed into
  `create_task` (`src/main.rs:1107-1131`, `src/db.rs:305-327`).
- Failed tasks are no longer always sealed as `done` — the TCP handlers track
  the loop outcome and seal as `TaskStatus::Failed` on error/panic
  (`src/main.rs:1192-1227`).
- `verify_task_chain` now recomputes `chain_hash` from each row's underlying
  fields and compares it to the stored value before verifying the signature
  (`src/db.rs:548-561`).
- `pay.x402` policy-block/failure reasons now reach the guest and client:
  `wire_x402_pay` writes `{"error": "<reason>"}` JSON via `write_wasm_error`
  instead of returning a bare `0`, so allowlist/rate-limit/cap blocks surface
  the specific reason.
- The proposal-time confirmation gate no longer blocks `pay.x402`:
  `skill_requires_confirmation` only triggers for `hedera_pay` capabilities;
  x402 governance is enforced autonomously inside `wire_x402_pay`.
