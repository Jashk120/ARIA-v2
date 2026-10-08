ARIA — Governed Agent Protocol
Design Document v0.6 — Daemon/REPL Split, MCP Server Role, Enterprise Model Proxy

| Field | Value |
|---|---|
| Author | Jayesh |
| Status | Design — supersedes §3–4 of v0.55 where noted, additive elsewhere |
| Version | 0.6 |
| Date | June 2026 |
| Scope | Adds: daemon/REPL separation, ARIA-as-MCP-server, cheap routing sub-agent, enterprise model-proxy via existing credentials. All v0.55 identity/credential/WASM-gate sections remain unchanged unless noted. |

---

## 0. What Changed and Why

v0.55 nailed identity and authorization (flat DIDs, AriaScopeCredential, WASM gate) but said nothing about *how external callers reach the daemon*. Two things forced that question this cycle:

1. Wanting ARIA to act as a tool provider for desktop chat clients (Claude Desktop, etc.) that can't do filesystem/OS-level work themselves, sandboxed as they are to their own app context.
2. Realizing the REPL and the daemon are currently one process — which blocks (1), since a daemon needs to run headless and accept external connections while a REPL is interactive and blocks on stdin.

v0.6 is the fix for both, plus one enterprise-tier mechanism (model-access proxying) that turns out to need zero new credential machinery — it reuses AriaScopeCredential exactly as written.

What this buys you:
- A daemon with a real external API, instead of REPL-and-daemon being accidentally fused into one binary.
- A second product surface (MCP server) that doesn't touch the WASM gate or credential model at all — the transport changes, the enforcement boundary doesn't.
- Enterprise model billing/routing solved with the *same* mechanism already used for skill authorization, not a parallel system.

---

## 1. Daemon / REPL Separation

### 1.1 Current State (Problem)
REPL and daemon are one process. The REPL was bolted onto the same binary as the daemon's ReAct loop — sharing memory and state by accident of being co-located, not by design. REPL holds no actual logic of its own; it has just been calling the loop in-process directly.

### 1.2 Target State
The daemon is the only place the ReAct loop, skill gate, and manifest/credential resolution run. It exposes a request/response interface — transport TBD (gRPC, HTTP, or local IPC socket) — that does not exist yet today.

REPL becomes a thin client: take user input, send it as a prompt to the daemon over that interface, render the response. It holds zero core logic.

### 1.3 Why This Matters for MCP
Once the daemon has a real external interface, an MCP server wrapper is just *another client* of that same interface — symmetric to REPL, not a special case. There is no "shared core to extract from REPL," because REPL never had core logic to extract. The actual prerequisite is narrower than that framing suggests: build the daemon's external API once. REPL and the MCP wrapper both point at it afterward.

### 1.4 Open Question
Whether REPL and an eventual MCP wrapper use the *same* protocol against the daemon is not a logic-sharing question (they don't share logic) — it's a "why maintain two request/response shapes into one daemon" question. Default: same protocol, one interface, multiple clients. Revisit only if REPL needs something lower-latency or more stateful than that interface supports.

---

## 2. ARIA as MCP Server

### 2.1 Direction, and Why It's Not the Same Conflict as Before
MCP integration was previously explored and dropped, because *ARIA-as-MCP-client* reintroduces a bypassable, runtime-negotiated capability surface — the exact failure mode ARIA's WASM gate exists to eliminate (a skill not compiled in cannot be invoked; an arbitrary external MCP server arbitrarily reachable at runtime is the opposite of that).

*ARIA-as-MCP-server* is the inverse role and does not carry the same risk. MCP is only the transport for a request to arrive — once it arrives, the existing WASM gate decides whether the requested skill is invokable, identical to a request originating from ARIA's own REPL. The skill manifest, credential check, and binary-boundary enforcement are unaffected by whether the caller is the daemon's own loop or an external client speaking MCP. This distinction (client = capability-fetching, bypassable; server = enforcing, consistent with the containment thesis) should be stated explicitly wherever v0.6 is read later, since "we use MCP now" and "we use MCP, but only as the enforcing server" look identical at a glance and mean opposite things for the security story.

### 2.2 Use Case
Desktop chat clients (Claude Desktop, etc.) are sandboxed to their own app/browser context and can't do agentic filesystem/OS-level work natively. ARIA's daemon, exposed as a local MCP server, becomes the thing that actually performs that work — gated through the existing skill manifest and credential model exactly as any other caller.

### 2.3 Positioning Against Existing MCP Filesystem Servers
Claude Desktop's own filesystem MCP server already solves "let my chat client touch my filesystem" — that capability alone is now a commodity, not a differentiator. What it does *not* have: a compiled-in skill boundary (once permission is granted, it can do whatever that permission scope allows), credential-based revocation, or an audit chain. ARIA's pitch is narrower and more defensible than "we also do filesystem access" — it's "ARIA is the hardened MCP server an enterprise would actually want running locally instead of an arbitrary one, specifically because they don't want employees' desktop clients reaching into company filesystems with unconstrained, unauditable permission."

### 2.4 Explicitly Out of Scope: Browser/Web Client Access
claude.ai (web) cannot reach a localhost MCP server, for three independent platform reasons, none of which are solvable from ARIA's side: (a) gRPC requires HTTP/2 framing and trailers that browser fetch/XHR cannot send natively; (b) HTTPS pages cannot fetch plain HTTP localhost endpoints under default browser security policy (and this is tightening further, not loosening); (c) CORS requires the local server to explicitly allow the calling origin, which doesn't change (a) or (b) regardless. This is why Claude Desktop's MCP integration is a desktop app and not a browser feature — desktop apps have OS-level process access outside the browser sandbox. ARIA targets desktop-client users only. No design time goes toward a browser-access path; it does not exist without a deliberate bridge built by the client vendor (Anthropic, OpenAI, etc.), which is out of ARIA's control.

### 2.5 Cheap Routing Sub-Agent
Incoming MCP requests are routed to the correct WASM skill by a small, cheap model running ARIA's own ReAct loop — *not* the calling model (Claude/GPT), so the expensive model isn't spent on routing/grunt work. This is provider-hosted, not locally run: a 4B-class local model cannot reliably do tool-calling, so the routing model is a larger but still cheap class (Gemma-class ~27B, provider-hosted) rather than something running on-device. This keeps cost down without claiming local inference that the parameter count can't support.

---

## 3. Enterprise Model-Access Proxy

### 3.1 Mechanism
The enterprise server holds the one real model-provider API key. It is never distributed to individual employee daemons. When an employee's daemon needs to make a model call, it authenticates to the enterprise server using the employee's existing personal DID plus their AriaScopeCredential — the same credential already used for skill authorization (v0.55 §2–3) — carrying a new scope (e.g. `model_inference`). The server checks the credential is valid, unexpired, and unrevoked, then forwards the call using the org's real provider key. The employee's daemon never sees or holds the actual provider key.

### 3.2 Why No New Credential Type Is Needed
This reuses `effective_scopes` resolution exactly as already specified — `model_inference` is just another entry in an AriaScopeCredential's `scopes` array, checked by the enterprise server's proxy logic the same way any skill gate checks any other scope. No new identity primitive, no separate auth path.

### 3.3 Explicit Rejection: Org DID as Bearer Key
An earlier framing of this feature proposed reusing `did:aria:acme` (the org DID) itself as the bearer credential presented for model-provider routing. This was rejected: `did:aria:acme`'s role is fixed by v0.55 §1.3 as credential *issuer* and master-agent identity — never a parent of person DIDs, and by extension never meant to double as a secret bearer token. Overloading it would mean (a) a credential-issuing key now sits in the request path of every employee's every message, vastly increasing its exposure surface, and (b) rotating the provider-auth key would require rotating the org's actual signing identity, breaking every AriaScopeCredential already issued under it (§2.1's `proof.verificationMethod` is keyed to that DID). Per-employee personal-DID-plus-credential auth avoids both problems and is also just less new surface area, since it's the existing mechanism.

### 3.4 Model Swap
Because the enterprise server's proxy is the only component that knows the actual target model, swapping models for an entire org is a server-side config change, invisible to every daemon instance.

---

## 4. Open Problems (carried over / updated from v0.55 §5)

| Problem | Status |
|---|---|
| Registry centralization | Unchanged from v0.55 — acknowledged, not load-bearing for the self-sovereignty claim. |
| Cross-org registry trust | Unchanged from v0.55 — deferred. |
| Credential refresh availability | Unchanged from v0.55 — acceptable for v1. |
| Multi-device personal DID | Unchanged from v0.55 — unresolved, not urgent. |
| Device DID (e.g. IoT/robotics endpoints, owner-only key custody) | **Considered and explicitly dropped for v0.6** — no extension built or planned. Noted here only so a future revisit isn't mistaken for an oversight. If ever pursued, the natural fit is closer to the "multi-device personal DID" problem inverted (one device, one owner-key) than to a new agent-DID tier. |
| Remote daemon access (non-MCP, e.g. exposing the daemon beyond localhost) | **New.** Public tunneling (e.g. ngrok) was considered and rejected as a default — it turns a local-only daemon into an internet-facing service, which breaks the implicit "local-only" threat model the WASM gate was designed against, and requires authentication-at-the-network-layer that the gate alone doesn't provide. Building custom tunnel/transport crypto in-house was also considered and rejected — existing solutions (Tailscale, mTLS) are already hardened against real attacks, and "we wrote our own transport crypto" is a red flag to security-conscious enterprise buyers, not a feature. Direction if ever needed: apply the existing DID/credential check at the connection layer on top of an off-the-shelf transport, not a home-grown one. Not committed to v0.6 — parked.

---

## 5. Migration Note (for whoever implements this)

- §1 (Daemon/REPL split) — new work, no existing section superseded. Requires defining the daemon's external interface before any MCP work begins; this is a hard prerequisite, not a parallel task.
- §2 (MCP server) — new work, depends on §1. Does not modify the WASM gate (v0.55 §3) or credential schema (v0.55 §2) — MCP is a transport arriving at the existing gate, nothing in the gate's logic changes.
- §3 (Enterprise model proxy) — additive to v0.55 §2.1's `scopes` field (new scope value `model_inference`); no schema change beyond agreeing on that scope name. No change to §3's `effective_scopes` resolution logic.
- §4 — device DID entry added for record-keeping only; remote daemon access entry added as a new parked problem, not scheduled.

ARIA — Design Document v0.6 — Confidential — Not for Distribution


What ARIA is technically:
A local-first agent runtime written in Rust. Skills are WASM binaries sandboxed by Wasmtime — zero host access unless explicitly declared in a manifest. The ReAct loop drives execution, Gemma 4 26B (MoE, ~4B active parameters) handles reasoning and tool-calling, SQLite persists state. The daemon runs headless, REPL is a thin client on top. Identity layer uses Ed25519 DIDs, AriaScopeCredentials gate skill authorization, and a hash-chained audit log makes execution history tamper-evident.
v0.6 adds three things: daemon/REPL split with a real external interface, ARIA exposed as an MCP server (enforcing, not consuming — important distinction), and an enterprise model proxy that reuses the existing credential mechanism so the org API key never touches employee machines.
What makes it technically non-trivial:
The WASM gate is a hard boundary — a skill not compiled in cannot be invoked at runtime. That's categorically different from permission-based sandboxing where a bad actor can escalate. The credential + audit chain means every skill invocation is attributable, revocable, and verifiable. No existing local agent runtime has this combination.
What the business strategy is:
Consulting-first, SAP-style. Learn actual workflows through paid engagements in a chosen domain, extract reusable components into a commercial skill package, sell that package to the next client with lighter implementation overhead. Over time the ratio flips from mostly services to mostly product.
OSS runtime is the foundation and credibility signal. Two skill tiers: community (free, public, unvetted, stays separate from commercial) and enterprise/commercial (curated, built during engagements or pre-packaged, monetized).
The Infosys/TCS analogy is the shape but not the scale — you're not building a headcount business. You're using the consulting phase to fund domain knowledge acquisition, with the exit being a vertical SaaS product.
Where the strategy is honest about its weaknesses:
Big tech (Nvidia/Microsoft OpenShell, various US AI startups) is moving fast on the runtime layer, which means the window to differentiate at infrastructure level is closing. The moat has to be domain knowledge in whichever vertical you pick, not the runtime itself.