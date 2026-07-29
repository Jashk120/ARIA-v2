# Known Issues — daemon audit



This file tracks confirmed defects and sharp edges found in the daemon, skill
runtime, bundled skills, payment layer, and identity/key-storage layer. Items
are ordered roughly by impact.

Validation run during this pass:

- `cargo check` — passes, with warnings.
- `cargo test` — passes: 10 daemon lib tests + 11 daemon binary tests, 1 ignored integration test in each target.
- `cargo test --workspace` — passes; several skill crates have 0 tests.
- Individual WASM skill builds:
  - `cargo build -p read_fs --target wasm32-wasip1 --release` — passes.
  - `cargo build -p search_web -p scrape_web -p find_fs -p pay_x402 -p query_payments --target wasm32-wasip1 --release` — passes.
  - `cargo build -p list_fs --target wasm32-wasip1 --release` — passes despite stub source.
  - `cargo build -p write_fs --target wasm32-wasip1 --release` — passes despite stub source.
- `cargo build --workspace --target wasm32-wasip1 --release` — fails because it tries to compile the daemon crate and `tokio` rejects the enabled WASM feature set.

---

## Critical / High

### 1. Daemon cannot start without Hedera credentials

`src/main.rs:156` constructs `PaymentVault::from_env()?` unconditionally.
`src/payments/direct.rs:20-21` requires `HEDERA_ACCOUNT_ID` and
`HEDERA_PRIVATE_KEY`. That means the daemon exits during startup if payment
credentials are missing, even for normal chat, local file, or web-search tasks.

This contradicts the adjacent x402 startup behavior in `src/main.rs:158-160`,
which intentionally makes `X402PaymentVault` optional when Hedera credentials
are absent.

**Fix**: make the direct `PaymentVault` optional too, or only construct it
when a manifest with `hedera_pay = true` is executed. Host payment calls should
return a clear skill error when the vault is unavailable.

### 2. TCP request framing is a single 4 KiB read

`src/main.rs:186-192` reads at most one 4096-byte chunk and immediately parses
that chunk as the entire request. Any request larger than 4 KiB, fragmented by
TCP, or sent slowly can fail as `Invalid JSON` even if the client sent valid
JSON. TCP does not preserve application message boundaries.

**Fix**: use newline-delimited JSON, length-prefixed frames, or read until EOF
with a bounded maximum request size before parsing.

### 3. Global task-chain signatures do not sign the stored task-chain link

`src/main.rs:210-215` signs `db.get_task_link_info("temp_id", &req.task)`.
`src/db.rs:215-224` then creates the real task with a fresh UUID and fresh
timestamp and recomputes `task_chain_prev`. The stored `task_chain_sig` is
therefore a signature over a different hash than the stored `task_chain_prev`.

This breaks the intended verifiable global task chain.

**Fix**: generate the task ID and timestamp once, compute the link hash once,
sign exactly that hash, and insert all of those values in a single DB operation.
Add a verifier test that checks `task_chain_sig` against the stored
`task_chain_prev`.

### 4. Per-step audit signatures can mismatch stored audit rows

`src/main.rs:250-257` computes and signs an audit `chain_hash` using
`db.get_next_step_info(...)`. `src/db.rs:254-259` then recomputes the input
hash, result hash, timestamp, step index, previous hash, and chain hash inside
`log_task_step`. If the timestamp crosses a one-second boundary or the step
index / previous hash changes, the stored `chain_hash` no longer matches the
signature.

**Fix**: move all audit-row hash construction and signing into one path. Either
have DB return a fully formed unsigned row to sign and then insert unchanged,
or have the caller pass the already-signed `step`, `prev_hash`, `timestamp`,
and `chain_hash` into `log_task_step`.

### 5. Failed tasks are sealed as `done`

`src/main.rs:271` always calls `db.seal_task(..., TaskStatus::Done)` after the
event stream ends. `AgentEvent::Error` is not remembered as task failure, and a
client write break also falls through to `Done`.

**Fix**: track whether any terminal error occurred and seal as
`TaskStatus::Failed` when appropriate. Consider distinct handling for client
disconnect versus agent failure.

### 6. `list.fs` and `write.fs` are advertised but not implemented

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

### 7. Native tool names use dots and can be rejected by OpenAI-compatible APIs

`src/agent/prompt.rs:172-176` emits manifest names directly as native function
names, e.g. `search.web`, `read.fs`, `pay.x402`. Many OpenAI-compatible
chat-completions tool schemas allow only alphanumeric, underscore, and hyphen
characters for function names. This can make native tool mode fail before model
execution.

**Fix**: introduce a reversible mapping for native tool names, such as
`search_web` <-> `search.web`, and map back before dispatching.

---

## Medium

### 8. Filesystem write resolution creates directories before sandbox boundary check

`src/skills/fs_sandbox.rs:112-117` calls `create_dir_all(parent)` for write
targets before checking `canonical.starts_with(&self.root)` at
`src/skills/fs_sandbox.rs:120-127`. A denied absolute path outside `fs_root`
can still create directories before access is rejected.

**Fix**: canonicalize and boundary-check the nearest existing ancestor before
creating directories, then create only under the verified sandbox root.

### 9. WASM memory helpers trust signed pointer/length inputs too much

`src/skills/wasm_runtime.rs:560-568` accepts `i32` pointer and length values,
casts them to `usize`, and computes `(ptr + len)` before the bounds check.
Negative values or overflow from a malicious or corrupted guest can produce
surprising ranges or panic in debug builds.

**Fix**: reject negative pointers/lengths, use checked addition, and then
perform the `memory.data()` bounds check.

### 10. `search.web` does not handle `host_http_get` failure

`skills/web/search.web/src/lib.rs:168-184` unpacks and reads the packed result
without checking `packed == 0`. Other skills such as `scrape.web` correctly
treat `0` as host-call failure. This can turn an HTTP failure into unsafe guest
memory access behavior instead of a clean JSON error.

**Fix**: mirror the `packed == 0` guard used by `scrape.web`, `find.fs`,
`read.fs`, and payment skills.

### 11. `cargo build --workspace --target wasm32-wasip1` is not a valid release build

The full workspace WASM build fails because it includes `aria-daemon`, whose
`tokio = { features = ["full"] }` dependency is not valid for WASM. The README
currently shows individual package build examples, but there is no checked
script that builds exactly the runnable skills and skips the daemon.

**Fix**: add a `build-skills` script/xtask that builds only skill packages for
`wasm32-wasip1`, and document that as the supported release command.

### 12. Direct payment HashScan links are always testnet

`src/payments/direct.rs:53-55` always formats
`https://hashscan.io/testnet/transaction/...` even when `HEDERA_NETWORK=mainnet`
or `previewnet`.

**Fix**: derive the HashScan network segment from the selected Hedera network.

### 13. Direct payment amount handling uses `f64`

`src/payments/direct.rs:40-42` accepts `amount_hbar: f64` and converts via
`(amount_hbar * 100_000_000.0) as i64`. This can silently truncate fractional
tinybars and does not reject non-positive, NaN, or infinite values before
building a transfer.

**Fix**: parse/accept decimal strings or integer tinybars, validate finite
positive values, and reject values that are not exactly representable in
tinybars.

### 14. Facilitator client has no timeout and logs raw responses to stderr

`src/payments/facilitator_client.rs` uses `reqwest::Client::new()` without a
timeout and has unconditional `eprintln!` debug logs for `/verify` and
`/settle` responses. A hung facilitator can stall payment flow, and raw payment
responses should not be printed in normal operation.

**Fix**: build the client with a timeout and replace unconditional prints with
`tracing::debug!` or structured, redacted logs.

### 15. `pay.x402` policy-block/failure reasons never reach the client

`src/skills/wasm_runtime.rs::wire_x402_pay` (around lines 896-938) rejects a
payment for rate-limit, allowlist, or spend-cap reasons by `eprintln!`-ing the
specific reason server-side and then returning a bare `0` to the guest. The
guest (`skills/pay/x402.pay/src/lib.rs::read_packed`) turns *any* `ptr == 0`
into the same literal string, `"host call failed"`, with no way to
distinguish a policy block from a rate-limit hit or a genuine payment
failure. `run_wasm_instance_async` then wraps that as
`"Skill error: host call failed"` and it is sent to the client as a plain
`observation` event (not even `error`) — indistinguishable from an unrelated
transient failure.

This is asymmetric with the `hedera_pay` confirm/deny path: those same
governance checks run earlier, in `src/agent/react_loop.rs`, and *do* emit a
specific reason (e.g. `"Payment blocked by policy (aria.allowlist): ..."`) as
an `AgentEvent::Error`. Only the autonomous x402 path loses the reason.

Found while building the Aria-GUI payment-confirmation UI (three-outcome
policy-blocked / pending / auto-approved display): the GUI can reliably label
a `hedera_pay` block with its real reason, but for `pay.x402` it can only
show "failed or blocked, reason not exposed" — deliberately not guessing,
since today's data genuinely can't distinguish the cases.

**Fix**: give `wire_x402_pay` a structured failure channel back to the guest
(e.g. always write `{"error": "<reason>"}` bytes via `write_wasm_bytes`
instead of returning bare `0` for the allowlist/rate-limit/cap branches), and
update `pay.x402`'s `read_packed` to surface that string instead of the
hardcoded `"host call failed"`. Once that's in place, the daemon can emit the
same `AgentEvent::Error`-with-reason shape it already uses for `hedera_pay`.

### 16. `pay.x402` proposals likely never reach `wire_x402_pay` at all

`src/agent/react_loop.rs::skill_requires_confirmation` returns `true` for any
skill with `capabilities.x402_pay`, which routes `pay.x402` calls through the
same proposal-time gate as `hedera_pay` (`extract_payment_recipient_and_amount`
at `react_loop.rs:1551-1573`). For x402, that function requires a non-empty
`pay_to` in the *proposed* arguments — but the LLM only ever proposes `{url}`
for `pay.x402` (per its manifest); `pay_to`/`amount` are only discovered from
the target's 402 response inside `wire_x402_pay` itself. So a normal
`pay.x402` call looks likely to fail at proposal time with `"Payment proposal
for pay.x402 is missing recipient account."` before ever reaching the
autonomous governance/payment code in `wasm_runtime.rs`.

Not confirmed at runtime (found by static tracing while building the GUI's
auto-approved-payment display), but worth a real end-to-end test — if
correct, x402 payments may not currently execute via the react loop at all.

**Fix**: skip the `react_loop.rs` proposal-time recipient/amount gate for
`x402_pay`-only skills (it's redundant with, and incompatible with, the
inline checks `wire_x402_pay` already performs once `pay_to`/`amount` are
known from the 402 response).

---

## Lower Priority / Cleanup

### 15. OpenRouter/provider selection is compile-time hardcoded

`src/config.rs:17-23` hardcodes `Provider::Ollama`, the Ollama URL, and both
model names. `Provider::OpenRouter` is never constructed in normal code. This
makes deployment-specific provider selection require a source change.

**Fix**: load provider, base URL, and model from DB/env with sane defaults.

### 16. OpenRouter API-key prompt is interactive and unsuitable for services

If provider selection is changed to OpenRouter, `src/main.rs:130-149` prompts
on stdin when no key is in the DB. That is reasonable for a CLI setup flow but
bad for `aria daemon` under systemd.

**Fix**: fail fast with a clear config error in daemon mode, and keep prompting
only in an explicit interactive setup command.

### 17. `RuntimeConfig` is loaded once at startup

`src/main.rs:155` loads injected config once and clones it into every request.
Changes to DB-backed config such as `searxng_url`, `fs_root`, `fs_allow`, or
`node_url` do not affect a running daemon until restart.

**Fix**: reload injected config per request or add a config-watch / refresh
mechanism.

### 18. `Db` uses `Mutex<Connection>` and unwraps poisoned locks

Most DB methods call `self.conn.lock().unwrap()`. A panic while holding the DB
mutex would poison the lock and cause later daemon operations to panic instead
of returning an error.

**Fix**: map poisoned locks to `anyhow` errors consistently.

### 19. `verify_task_chain` does not recompute row hashes

`src/db.rs:270-295` checks `prev_hash` continuity and verifies signatures over
stored `chain_hash`, but it does not recompute `chain_hash` from stored
`step`, `skill_called`, `input_hash`, `result_hash`, `prev_hash`, and
`timestamp`. A row with internally inconsistent fields could pass if its
signature matches the stored `chain_hash`.

**Fix**: recompute the expected hash for every row and compare it with stored
`chain_hash` before verifying the signature.

### 20. Identity export is not passphrase-gated

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