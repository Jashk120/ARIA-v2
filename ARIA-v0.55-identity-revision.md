# ARIA — Governed Agent Protocol
## Design Document v0.55 — Identity Layer Revision

| Field | Value |
|---|---|
| Author | Jayesh |
| Status | Design — supersedes §4.5–4.7 of v0.5 |
| Version | 0.55 |
| Date | June 2026 |
| Scope | Replaces the DID hierarchy and manifest sections of v0.5. All other sections of v0.5 (architecture, WASM gate, audit chain, roadmap, business model) remain unchanged unless noted. |

---

## 0. What Changed and Why

v0.5 §4.5 described a **signing hierarchy**: `did:aria:acme` (root, ARIA-minted) signs `did:aria:acme:alice` (person, company-issued) signs `did:aria:acme:alice:assistant-v1` (agent, local key). This was framed as "self-sovereign," but it isn't — every agent and person DID is subordinate to, and exists only because of, an org DID that ARIA's server mints. It also tightly couples a person's identity to their employer: when Alice leaves Acme, what happens to `did:aria:acme:alice`? The hierarchy doesn't answer this cleanly.

v0.55 replaces the hierarchy with a **flat-identity + credential model**:

- DIDs are **personal and root-level**, created once, owned for life, portable across employers.
- Org membership, role, and capability scope are expressed as **short-lived signed credentials (VCs)** issued by the org to the person's DID — not as subordinate DIDs.
- **Org DIDs exist only as credential issuers and as the identity of the org's master agent** — they never mint identities for people.

This is a smaller change than it sounds: the WASM gate, audit chain, manifest concept, and master-agent gateway pattern all carry over unchanged. Only the *source of authorization* changes — from "DID hierarchy says you're allowed" to "DID + active credential says you're allowed."

**What this buys you:**
- Personal identity is genuinely self-sovereign (key custody + portability), without overclaiming decentralization of the registry.
- Org revocation ("Alice was fired") is instant and doesn't touch Alice's DID — the org just stops renewing/revokes her credential.
- The cryptographic chain stays intact at the org boundary, which is exactly the boundary the audit/compliance story (v0.5 §6, §13) depends on.
- Centralized registry is fine to acknowledge openly — it's no longer load-bearing for the "self-sovereign" claim, which now rests entirely on local key custody + portable DIDs.

---

## 1. Identity Model

### 1.1 DID Types

| Type | Pattern (illustrative) | Created by | Lifetime | Purpose |
|---|---|---|---|---|
| **Personal DID** | `did:aria:alice` | The individual, locally, once | Permanent, owned by the individual | Root identity. Used across employers, projects, contexts. |
| **Org DID** | `did:aria:acme` | The organization, once | Permanent, owned by the org | Identity of the org as a credential issuer and as the org's master agent. Does **not** mint subordinate person/agent DIDs. |

There is no third "agent" DID tier. An "agent" (e.g. Alice's assistant) is a *role* her personal DID operates in, distinguished by which manifest + credentials are active for a given task — not a separate identity. (If multiple physical devices need independent keys under one person, see §5 Open Problems — out of scope for v1.)

### 1.2 Key Generation (unchanged from v0.5 §4.5)

- Ed25519 keypair generated locally on device.
- Private key never leaves the device, encrypted at rest in local SQLite (`identity` table, v0.5 §7.2).
- Public key published to the ARIA Identity Server for resolution — this is a **directory entry**, not an authorization grant. Anyone can register a personal DID; registration confers no privileges.

### 1.3 Org Registration

An org registers `did:aria:acme` with the ARIA Identity Server, the same as a personal DID, but flagged `type: org`. This DID:

- Identifies the org's **master agent** (v0.5 §4.4's gateway concept) — the single point of contact for inter-org communication.
- Is the **issuer** for `AriaScopeCredential`s granted to members (§2).
- Is **not** a parent of any person DID. Acme cannot mint, sign, or revoke `did:aria:alice` — it can only issue or revoke credentials *about* `did:aria:alice`.

---

## 2. Authorization Model — AriaScopeCredential

### 2.1 Credential Shape

A single, generic credential type covers both employee-membership and org-to-org partnership cases:

```json
{
  "@context": "https://aria.network/vc/v1",
  "type": "AriaScopeCredential",
  "issuer": "did:aria:acme",
  "subject": "did:aria:alice",
  "relationship": "member",
  "scopes": ["read_email", "query_db:hr", "notify:slack"],
  "issued": "2026-06-11T09:00:00Z",
  "expires": "2026-06-11T17:00:00Z",
  "status_list": "https://acme.aria-server/status/3#94",
  "proof": {
    "type": "Ed25519Signature2020",
    "verificationMethod": "did:aria:acme#key-1",
    "proofValue": "z..."
  }
}
```

| Field | Notes |
|---|---|
| `issuer` | The org DID granting the scope. |
| `subject` | The person DID *or* another org DID (for partnerships). |
| `relationship` | `member` (employee), `partner` (org-to-org), `delegate` (future: contractor/temp access). |
| `scopes` | Skill names, optionally namespaced (`query_db:hr` = the `query_db` skill scoped to the `hr` schema). Union'd with the subject's personal manifest at gate-check time (§3). |
| `expires` | Short — hours, not days. Forces periodic re-issuance, bounding the blast radius of a stale credential. |
| `status_list` | W3C Status List 2021 bitstring reference — enables **instant** revocation independent of `expires`, for the "fired today" case. |
| `proof` | Signed by the issuer's key. This is the one signature that makes the org boundary cryptographically real (see v0.5 §1's core claim). |

### 2.2 Employee Case (`relationship: member`)

1. Acme's identity server issues Alice a `AriaScopeCredential` on onboarding (and re-issues periodically while she remains active — e.g. daemon refreshes hourly).
2. Alice's daemon caches the current credential alongside her personal manifest.
3. To fire Alice: Acme stops renewing the credential (expires within hours) and/or flips her bit in the `status_list` (instant). **`did:aria:alice` itself is never touched** — she keeps her DID, her keys, her personal manifest, and can use them at her next job.

### 2.3 Org-to-Org Case (`relationship: partner`)

Same shape, `subject` is an org DID instead of a person DID. E.g. Acme issues Globex's master agent a credential scoping `aria_call:invoice-api` — defining exactly what Acme's master agent will accept from Globex's master agent. This reuses the identical verification logic in §3; no second schema, no second code path.

---

## 3. Enforcement — WASM Gate Scope Resolution

v0.5 §4.8 (WASM skill gate) is unchanged in mechanism. What changes is the **scope set the gate checks against**:

```
effective_scopes(task) =
    personal_manifest.skills
    ∪ ( active_credential.scopes   if task is in an org context
                                     and credential is unexpired
                                     and credential not in issuer's status_list revocation set
        else ∅ )
```

- **Personal context** (no org credential involved): gate checks `personal_manifest.skills` only — exactly as v0.5 described for the solo/personal tier.
- **Org context** (task originates from or targets an org's master agent): gate checks the union. A skill must be in *either* set; org credentials can only **add** scope for that context, never remove from the personal manifest, and never persist beyond their validity window.
- On credential expiry/revocation mid-session, the daemon drops the org-scoped portion of `effective_scopes` immediately — in-flight calls to org-only skills fail with "credential expired/revoked," logged as a capability request (v0.5 §4.9).

`.wasm` modules themselves are unaware of this distinction — the daemon resolves `effective_scopes` once per task/context switch and the gate logic (v0.5 §4.8) operates on the result exactly as before.

---

## 4. Master Agent — Gateway Pattern

Carries over from discussion, formalizing v0.5 §4.4's "enterprise runner" concept:

- Each org runs **one master agent** (`did:aria:acme`'s operational identity) as the sole point of contact for inter-org agent communication.
- Alice's agent never talks to Globex's agents directly. Flow: Alice's agent → Acme's master agent → Globex's master agent → Globex's relevant agent.
- This keeps the N×M trust-relationship problem at O(N): each org maintains trust relationships (partner credentials, §2.3) only with its master agent's counterparts, not with every external agent individually.
- IT/security teams get a single egress/ingress point to monitor and firewall — operationally this is also just a more deployable architecture for enterprise infosec review.

---

## 5. Open Problems (additions to v0.5 §12)

| Problem | Notes |
|---|---|
| **Registry centralization** | The ARIA Identity Server is the sole resolver for personal and org DIDs in v1. This is acknowledged, not hidden: self-sovereignty here means *key custody and portability*, not registry decentralization. Phase 3+: optional self-hosted identity server (already in Enterprise tier, v0.5 §8) gives orgs data sovereignty over their own credential issuance without requiring a decentralized registry. |
| **Cross-org registry trust** | If Acme's master agent wants to verify "is `did:aria:globex` really Globex" without trusting Globex's identity server's self-report, registry-level facts (DID existence, public key) could eventually be cross-checked against a mutually trusted source. **Deferred — not roadmapped.** Revisit only if a specific cross-org pilot makes this concrete; current design (fetch + verify signature chain, cache) is sufficient until an org's identity server lying about its own registry is a demonstrated problem, not a hypothetical one. |
| **Credential refresh availability** | If Acme's identity server is down, Alice's cached credential simply expires (within hours, by design) and her org-scoped access lapses until the server returns. Acceptable for v1; revisit if enterprise customers need offline-tolerant org access windows longer than a few hours. |
| **Multi-device personal DID** | One DID, one device, one private key in v1 (consistent with v0.5 §12 "Cross-device identity"). A person with a laptop + phone agent currently needs two DIDs or a portable-key solution — unresolved, not urgent. |

---

## 6. Migration Note (for whoever implements this)

Sections of v0.5 affected:

- **§4.5 (DID Hierarchy)** — replaced wholesale by §1–2 above.
- **§4.6 (Trust Verification)** — step 2 ("verify DID chain... did:aria:acme:alice signed by did:aria:acme") is replaced by: verify `AriaScopeCredential` signature (issuer = org DID, subject = agent's personal DID) is valid, unexpired, and not in the issuer's status list, in addition to the existing manifest/wasm-hash/audit-chain checks (steps 1, 3–5 unchanged).
- **§4.7 (Manifest Format)** — the manifest (`wasm_hashes`, `skills`, self-signed by the personal DID) is unchanged and remains the *personal* capability declaration. `AriaScopeCredential` is additive, not a replacement.
- **§7.1 (Postgres schema)** — `did_registry.parent_did` and `did_registry.type ('root'|'person'|'agent')` are no longer meaningful; replace with `type ('person'|'org')`, no parent. Add a `scope_credentials` table mirroring §2.1's fields plus a `status_list_bitstring` table or column for revocation.
- **§7.2 (Daemon SQLite)** — add `cached_credentials` table (mirrors `cached_manifests`, but for active `AriaScopeCredential`s, keyed by issuer org DID, with `expires_at` checked on every org-context gate resolution per §3).

---

*ARIA — Design Document v0.55 — Identity Layer Revision — Confidential — Not for Distribution*
