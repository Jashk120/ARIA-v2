# Known Issues & Scope Notes — x402 Payments + Identity Layer

Working notes from the x402-on-Hedera payment implementation and a pass over
the identity/key-storage layer. Nothing here blocks the core payment flow —
`X402PaymentVault::pay()` is proven end-to-end against a live facilitator
(`x402.org/facilitator`) with a real settled testnet transaction. These are
the rough edges worth cleaning up, in roughly descending priority.

---

## Payments — `src/payments/`

### 1. Debug logging left in production path (`x402_vault.rs`)
`eprintln!` calls at lines ~93 and ~98 dump the full base64 payment payload
and verify-failure reasons unconditionally on every call. Left over from the
signature-debugging session. Should be removed or downgraded to
`tracing::debug!` (respects log-level filtering, doesn't spam stderr in
normal operation, and stops leaking transaction payload contents into logs
by default).

**Fix**: remove or `tracing::debug!`-gate both lines.

### 2. Hardcoded `"SUCCESS"` status string (`x402_vault.rs`)
The payment-logging call writes the literal string `"SUCCESS"` rather than
deriving it from `settle_res.success`. Correct *today* only because of an
earlier `if !settle_res.success { return Err(...) }` guard — if that guard
ever moves or changes, this line silently keeps logging `"SUCCESS"`
regardless of actual outcome.

**Fix**: `if settle_res.success { "SUCCESS" } else { "FAILED" }`, even though
the false branch is currently unreachable — documents the invariant instead
of relying on it implicitly.

### 3. Internal bookkeeping smuggled through `PaymentRequirements.extra`
`skillCalled`, `taskId`, `memo`, `url`, `description`, `mimeType` are all
read out of `requirements.extra` in `x402_vault.rs`, rather than being passed
as explicit parameters. Two problems:
- `extra` is part of the *signed, wire-transmitted* `PaymentRequirements` —
  ARIA's internal task/skill bookkeeping is currently being sent to the
  external facilitator as part of the payment payload. Minor information
  leak, no functional harm today, but not ideal.
- If a real resource server ever populates `extra` with its own
  scheme-specific fields, a collision with `skillCalled`/`taskId`/`memo`
  keys would silently corrupt payment logging.

**Fix**: change `X402PaymentVault::pay()`'s signature to accept
`skill_called: &str`, `task_id: Option<&str>`, `memo: Option<&str>`
explicitly, rather than reading them out of `extra`.

### 4. `Db` stored by value in `X402PaymentVault`, not `Arc<Db>`
`x402_vault.rs`'s `db: crate::db::Db` field — worth confirming `Db::clone()`
(if it derives `Clone` at all) shares the underlying `Mutex<Connection>`
rather than creating a second independent connection/mutex to the same
SQLite file. Likely harmless (SQLite handles file-level locking regardless)
but worth a five-minute check rather than an assumption.

### 5. Hardcoded single-node pin (`x402_types.rs::build_payment_transaction`)
```rust
let node_id = AccountId::from_str("0.0.3").unwrap();
tx.node_account_ids([node_id]);
```
This exists to work around a real `TransactionList`-dimensionality
incompatibility (see BUG-1 below) between `hiero-sdk`'s default multi-node
freeze behavior and the JS-based facilitator's single-transaction decode
path. It was investigated whether a health-aware node could be pulled from
the `Client`'s own known node set instead of a hardcoded literal —
`hiero-sdk` 0.45.0's health-weighted `random_node_ids()` is `pub(crate)`,
not accessible from outside the crate, so no equally-robust alternative was
available at the time.

**Risk**: if testnet node `0.0.3` ever becomes unreachable, deprecated, or
degraded, every payment fails with a confusing, seemingly-unrelated network
error with no obvious link back to this line.

**Fix options, not yet decided**:
- Move to a config value (env var / constant) instead of an inline literal,
  so it can be changed without a code change if `0.0.3` becomes unreachable.
- Revisit whether a newer `hiero-sdk` version exposes a public,
  health-aware single-node accessor.
- At minimum, expand the existing code comment to state explicitly that
  this is a known single-point-of-failure, not just an implementation note.

### 6. `extract_body_bytes` is dead code (`x402_types.rs`)
Hand-rolled protobuf varint parser, confirmed unused by `cargo test`'s own
warning output (`function 'extract_body_bytes' is never used`). Left over
from an earlier debugging attempt at manually inspecting `SignedTransaction`
bytes. No current call site.

**Fix**: delete. Manually-maintained wire-format parsing code sitting unused
in a payment path is pure risk (looks load-bearing, isn't) with no offsetting
benefit.

### 7. `tx.sign(...)` return value discarded (`x402_types.rs`, line ~131)
```rust
tx.sign(client_private_key.clone());
```
Not yet confirmed whether `hiero-sdk`'s `.sign()` here can fail (wrong key
type, etc. — exactly the class of bug found and fixed earlier this session)
and if so, whether that failure is silently swallowed by discarding the
return value.

**Fix**: check the actual method signature; if it returns `Result`, handle
the error explicitly rather than discarding it.

### 8. Test coverage doesn't cover the actual production key type
`x402_types.rs`'s only unit test (`test_build_payment_transaction_hbar`)
uses `PrivateKey::generate_ed25519()`. The real production path uses ECDSA
(secp256k1) keys, and the hardest bug found this session (BUG-2 below) was
an ECDSA-specific signature-deduplication failure that would not have been
caught by this test even if reintroduced by a future refactor.

**Fix**: add a second test case using an ECDSA key, ideally asserting the
resulting `SignatureMap` contains exactly one `SignaturePair` for the
signer (regression coverage for BUG-2).

### 9. `amount` parsed as `i64`, not arbitrary precision
`x402_types.rs::build_payment_transaction` parses `requirements.amount` as
`i64`. The x402 v2 spec doesn't formally cap amount precision (relevant for
HTS tokens with high decimal counts). Almost certainly fine for realistic
HBAR/tinybar amounts (`i64` caps at ~9.2 quintillion), but not spec-exact.
Low priority — noting as a known simplification, not a bug.

---

## Identity / Key Storage — `src/crypto.rs`, `src/identity/`

### 10. No user passphrase gate on private key export
`FileVault`/`crypto::load_signing_key` decrypt using a key derived from
`device_secret` (a random 32-byte file at `~/.aria/device.key`) + the DID
string as salt — **no user-supplied passphrase is involved anywhere in the
current code.** This is real encryption-at-rest (AES-256-GCM, Argon2id,
correct parameters, `Zeroizing` memory hygiene) against a narrow threat
model (a different local OS user, or a stolen disk that doesn't also include
`device.key`) — but it does **not** protect against anything that can read
files as the same OS user the daemon runs as (malware, a copied `~/.aria`
directory, etc.), and there is currently no passphrase-gated export flow.

**Planned fix** (scoped, not yet built): add an optional user passphrase as
an additional input to a *separate* derivation used only for the export
path — leave the existing `device_secret`-only derivation untouched for
normal day-to-day `sign()` calls (no reason to prompt for a passphrase on
every signature). The passphrase-derived key should use its own random
salt (not reuse the DID-string salt), go through Argon2id, and the export
function should re-encrypt a *copy* for output rather than mutate the
stored `id.key` blob. Add tests: correct passphrase decrypts, wrong
passphrase fails cleanly (mirroring the existing
`test_aes_gcm_wrong_key_fails` pattern), and confirm normal `sign()` calls
are unaffected.

---

## Bugs found and fixed this session (for reference / upstream reporting)

### BUG-1: `hiero-sdk` `TransactionList` dimensionality mismatch
`Transaction::freeze_with(Some(client))` selects and freezes against *all*
of the client's known nodes (e.g. 7 for testnet), producing a
multi-transaction `TransactionList` on `.to_bytes()`. The JS-based
facilitator's `Transaction.fromBytes()` + `instanceof` type-inference chain
only handles a single-transaction list. Fixed by using
`to_signed_transaction_bytes()` (single transaction) and manually
re-wrapping it in a single-element `TransactionList` envelope
(`encode_as_transaction_list` in `x402_types.rs`), combined with explicitly
pinning `node_account_ids` to one node before freezing (see item 5 above for
the residual hardcoding risk this introduced).

### BUG-2: `hiero-sdk` ECDSA signature deduplication failure (real upstream bug, confirmed from source)
When a transaction is frozen with `freeze_with(Some(client))` (which
auto-registers the client's operator key for signing) *and* explicitly
signed again via `tx.sign(key)` with the same ECDSA key, `hiero-sdk`
produces a `SignatureMap` with **two identical signature pairs** for the
same signer, causing Hedera nodes to reject the transaction at precheck
with `KEY_PREFIX_MISMATCH`.

Root cause, confirmed directly from `hiero-sdk` 0.45.0 source
(`src/transaction/mod.rs:942` vs. `src/key/public_key/mod.rs:222-227`): the
signature dedup check compares `public_key.to_bytes()` (which returns
**DER-encoded** bytes for ECDSA keys, by explicit design) against
`pub_key_prefix` (which is always the **raw compressed** form, via
`to_bytes_raw()`). `der_bytes.starts_with(&raw_bytes)` can never be true, so
the dedup check always concludes "no existing match" for ECDSA keys and
appends a duplicate `SignaturePair`. Ed25519 is unaffected because its
`to_bytes()` is defined to equal `to_bytes_raw()`.

**Workaround applied**: `tx.freeze_with(None)` instead of
`freeze_with(Some(client))` — since `node_account_ids` and `transaction_id`
are already set explicitly, the only thing `Some(client)` was providing was
operator auto-registration (which triggers the duplicate) and default
`max_transaction_fee` (which falls back safely to the transaction type's
built-in default when omitted). Confirmed via source that `freeze_with(None)`
is safe given our explicit field-setting.

**Status**: not yet reported upstream — plan is to file a GitHub issue
against `hiero-sdk-rust` with the exact line numbers, mechanism, and a
one-line proposed fix (compare `to_bytes_raw()` on both sides at line 942)
once bandwidth allows.

### BUG-3 (non-bug, ruled out): DER-vs-raw signature format
Initially suspected the *signature* itself (not the public key) was
DER-encoded and incompatible with the facilitator's expected raw 64-byte
format. Disproven directly: `k256::ecdsa::Signature::to_vec()` (`k256` v0.13.4,
confirmed via `Cargo.lock`) returns the raw 64-byte `(r || s)` format by
default; DER requires the separate, explicitly-invoked `.to_der()` method,
which `hiero-sdk` does not call in this path. The actual bug was BUG-2
above (public key encoding, not signature encoding).

### BUG-4 (root cause of first `KEY_PREFIX_MISMATCH`-adjacent symptom): Ed25519/ECDSA key-type ambiguity
`PrivateKey::from_str` in `hiero-sdk` cannot distinguish a bare 32-byte hex
Ed25519 key from a bare 32-byte hex ECDSA (secp256k1) key by length alone,
and defaults to Ed25519. The test Hedera account used
(`0.0.8859309`) is registered as `ECDSA_SECP256K1`; loading its key via the
generic `from_str` silently produced a mathematically valid but
wrong-algorithm signature, rejected by the facilitator as
`invalid_exact_hedera_payload_signature_invalid`.

**Fix applied**: use `PrivateKey::from_str_ecdsa(...)` explicitly wherever
`HEDERA_PRIVATE_KEY` is parsed, instead of the generic `from_str`.