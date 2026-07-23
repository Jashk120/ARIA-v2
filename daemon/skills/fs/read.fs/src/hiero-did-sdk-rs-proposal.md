# hiero-did-sdk-rs — Project Proposal 0.1

# Sponsor(s)

Jayesh Kathale, creator and primary maintainer, hiero-did-sdk-rs — jayeshkathale24@gmail.com

# Abstract

A Rust workspace implementing the `did:hedera` DID method over Hedera Consensus Service: DID create/update/deactivate, pluggable resolution (REST, gRPC, cached), DID URL dereferencing, and an AnonCreds registry layer. Currently hosted under `hiero-hackers`; proposed for promotion to an independent Hiero project.

# Context

This project originated as an independent implementation of the `did:hedera` method, built to bring Rust-ecosystem parity to Hiero's existing DID SDKs (DID SDK JS, and equivalents in other languages). It currently lives under the `hiero-hackers` sub-project. Cross-SDK interoperability with DID SDK JS has been verified end-to-end (see Solution section), and the project is now being proposed for promotion to a standalone Hiero project, following the same path taken by other DID SDKs in the ecosystem.

# Dependent Projects

- **hiero-sdk-rust** — the workspace's `hiero-did-hcs` and `hiero-did-client` crates wrap the Hedera Rust SDK's client, topic, and transaction primitives. No changes to hiero-sdk-rust are required for this proposal; one upstream contribution (exposing `get_transaction_body_bytes()` for external async signers) has already been contributed and is in use.
- **DID SDK JS (Hiero)** — not a build dependency, but the interoperability partner this SDK has been validated against at the wire-format and resolution level.

No Dependent Project's maintainers are blocked by this proposal; sign-off from Hiero Identity ecosystem stakeholders is in progress.

# Motivation

Hiero's `did:hedera` method currently has SDK support in other languages but no production-grade Rust implementation under the Hiero organization. Rust is increasingly used for infrastructure-grade, long-lived services (wallets, agents, backend identity issuers) where memory safety and a strong type system are valuable for handling cryptographic key material and DID document state. A first-party, Hiero-maintained Rust SDK:

- Closes the gap for Rust-based teams building on Hedera identity (agent frameworks, backend services, embedded/edge use cases) who currently have no in-ecosystem option and would otherwise need to bind to another language's SDK via FFI or reimplement the method themselves.
- Has already demonstrated wire-format and resolution-level interoperability with DID SDK JS, reducing the risk that a second implementation drifts from the method spec.
- Is architected as a workspace of focused crates (parsing, messages, signing, HCS, resolution, AnonCreds, lifecycle orchestration) rather than a monolith, which keeps the surface auditable and makes it easier for new contributors to take ownership of individual pieces.
- Opens up `did:hedera` to embedded and resource-constrained environments. Rust's small runtime footprint and lack of a garbage collector make it a natural fit for IoT devices, edge gateways, and other embedded software that needs to hold or present a DID — a class of use case (device identity, machine-to-machine authentication) that is difficult to reach from a JVM- or Node-based SDK.
- Is already in long-term maintenance by an active maintainer, rather than a one-off contribution.

# Status

Pre-incubation / seed. The project is functionally complete for core DID operations and has passed a formal cross-SDK interoperability check. It is being brought to the Hiero TAC for review as a new incubation project, per the [project lifecycle](https://lf-decentralized-trust.github.io/governance/governing-documents/project-lifecycle.html).

# Solution

**Scope.** `hiero-did-sdk-rs` is a 12-crate Rust workspace, including an umbrella `hiero-did-sdk` re-export crate, implementing:
- DID write operations (create, update, deactivate) over HCS, including client-side message signing (CSM) prepare/submit flows for external signers.
- DID resolution via pluggable transport (`TopicReader` trait), with REST mirror-node, gRPC mirror subscription, and cached HCS implementations.
- DID URL dereferencing against resolved documents (verification methods, services).
- Content negotiation on resolution output (JSON, JSON-LD, CBOR, full resolution envelope).
- An AnonCreds registry layer (schemas, credential definitions, revocation registries) built on the same HCS primitives, per the Hiero AnonCreds Method spec.

**Architecture.** Responsibilities are split across single-purpose crates (`core`, `method`, `messages`, `signer`, `client`, `hcs`, `registrar`, `resolver`, `anoncreds`, `lifecycle`, `utils`, and the `sdk` re-export crate), with a strict dependency direction rooted in `hiero-did-core`. Domain logic (DID write/read semantics) is kept separate from transport (HCS primitives) and from wire format (message envelopes), so each concern can be tested and extended independently.

**Interoperability with DID SDK JS.** A dedicated cross-SDK check was completed comparing wire format end-to-end — envelope shape, message fields, event encoding, signing bytes, and key serialization (multibase, Ed25519VerificationKey2020) — and confirmed identical between the two SDKs. This surfaced and fixed one real interop bug (the Rust SDK encoding a payload instead of `null` for deactivate-operation events, which would have caused JS-side rejection of Rust-produced deactivate messages). A bidirectional interop test suite (`interop_fixtures`) now confirms DID create/resolve and AnonCreds object round-trips in both directions (Rust → JS, JS → Rust); results are published at https://github.com/Jashk120/DID-sdk-cross_Introp_Test/blob/main/interop_results.json.

Two spec-level gaps were also identified during this work and raised with Hiero Identity stakeholders directly rather than left implicit:
- DID error codes (`DIDError` in Rust vs `ErrorCodes` in JS) are not 1:1, but errors are not part of the wire format, so this was confirmed as implementation-local and out of interop scope.
- DID URL path semantics are unspecified in the Hedera DID method spec and unhandled in dereferencing by *both* SDKs (JS parses but drops path; Rust currently rejects any URL with a path). This is a DID Core / DID Resolution spec gap rather than a Rust-specific one; a specific example (incorrect `invalidDidUrl` vs. `NotFound` handling on unhandled path) was documented for future resolution.

**Signing.** Ed25519 signing/verification is abstracted behind a `Signer` trait in `hiero-did-core`, with an in-process signer and an optional HashiCorp Vault-backed signer (`VaultSigner`) that keeps private key material outside the SDK process. The Vault signer's `reqwest::blocking` usage is being replaced with a proper async implementation ahead of promotion (see Effort and Resources).

**Testing.** Unit tests live alongside each crate; integration tests run against live Hedera testnet/local-node per crate (client init, HCS topic/message/file operations and caching, registrar create/update/deactivate/CSM flows, resolver gRPC-vs-REST parity and CBOR round-trips, AnonCreds registry operations, and umbrella SDK wiring). Test isolation from live mirror-node latency (currently a source of flaky integration tests) is being addressed ahead of promotion.

**License.** Apache 2.0, consistent with the rest of the Hiero codebase. No trademarks are used in the project name.

**Backward compatibility / network effects.** This SDK is a client library only; it introduces no protocol or consensus changes and has no effect on network throughput or participation criteria.

# Effort and Resources

Current maintainer: Jayesh Kathale (sole active maintainer and long-term committed maintainer for the project).

Remaining action items before requesting incubation review:
- Replace `reqwest::blocking` in the Vault-backed signer with an async implementation, so the signer no longer blocks the async runtime.
- Add test isolation for integration tests so they do not depend on live mirror-node latency (flaky-test reduction).
- Resolve open DID URL path-semantics question with Hiero Identity stakeholders, or explicitly defer it pending a method-spec update.

No dedicated funding or infrastructure is being requested; the project runs against existing Hedera testnet/mirror-node infrastructure. Additional maintainers/reviewers from the Hiero community would be welcomed as the project moves through incubation.

# How To

- Source: workspace root `Cargo.toml`; `cargo build --workspace` builds all 12 workspace crates, including the `hiero-did-sdk` umbrella crate.
- Tests: `cargo test --workspace` runs unit tests; live-network integration tests are gated per-crate under `tests/` and require testnet or local-node credentials (`HederaClientConfiguration`).
- Usage: consumers depend on the `hiero-did-sdk` crate, which re-exports all public sub-crates and exposes `HieroDidSdk` as the main developer entrypoint, alongside lower-level access to `registrar` (create/update/deactivate) and `resolver` (resolve/dereference) functions for consumers who need finer control.
- Interop verification: cross-SDK test harness and results are published at https://github.com/Jashk120/DID-sdk-cross_Introp_Test.

# References

- Hedera DID Method specification.
- W3C Decentralized Identifiers (DIDs) v1.0 — https://www.w3.org/TR/did-1.0/
- W3C DID Resolution Working Draft — https://www.w3.org/TR/did-resolution/
- Hiero AnonCreds Method spec — https://hiero-ledger.github.io/identity-collaboration-hub/hiero-anoncreds-method/
- DID SDK JS (Hiero), for interoperability comparison.

# Closure

Success criteria for this proposal:
- Acceptance of `hiero-did-sdk-rs` as an independent Hiero incubation project.
- Sustained interoperability with DID SDK JS (regression-tested via the `interop_fixtures` harness on an ongoing basis).
- Growth of the maintainer/reviewer base beyond a single maintainer during incubation.
- Adoption by at least one downstream Hedera identity use case (wallet, issuer service, or agent framework) as a signal the SDK meets real integration needs.
