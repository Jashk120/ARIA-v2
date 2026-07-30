# ARIA Architecture

ARIA (Agentic Interface for AI) is a modular, high-performance host system designed to execute AI "skills" in a highly secure, sandboxed environment using WebAssembly (WASM).

## System Overview

The system is split into two primary domains: the **Host (Aria Daemon)** and **Guest (WASM Skills)**.

```mermaid
graph TD
    User([User]) <--> REPL[Aria REPL]
    REPL <--> Daemon[Aria Daemon]
    Daemon <--> DB[(SQLite DB)]
    Daemon <--> Wasmtime[Wasmtime Engine]
    Wasmtime <--> Skill[WASM Skill]
    Skill -- Host Call --> HostFFI[Host FFI Bridge]
    HostFFI -- Network/OS --> Internet((Internet))
```

## Core Components

### 1. Aria Daemon (`aria-daemon`)
Written in Rust, the daemon is the central nervous system of ARIA. It manages:
- **Skill Lifecycle**: Loading, caching, and executing WASM modules.
- **State Management**: Persistent storage via SQLite for configuration and skill-specific data.
- **Async Runtime**: Leveraging `tokio` for concurrent task management, while offloading synchronous WASM execution to dedicated threads via `spawn_blocking`.

### 2. WASM Skill System
Skills are standalone WASM binaries (compiled for `wasm32-wasip1`) that perform specific tasks (e.g., search, scraping).
- **Self-Describing**: Each skill includes a `manifest.toml` defining its capabilities, configurations, and display templates.
- **Isolation**: Skills have zero access to the host filesystem or network unless explicitly granted via Host FFI functions.

### 3. Host-Guest Communication Protocol
Communication occurs over linear memory using a specialized JSON-based protocol.

#### Memory Layout
- **Input Buffer (Offset 0)**: The host writes the execution arguments as a UTF-8 JSON string starting at offset 0.
- **Dynamic Output Allocation**: When a skill triggers a host function (like `host_http_get`), the host:
    1. Determines the current end of the WASM linear memory (`memory.data_size()`).
    2. Grows the memory by the required size of the response.
    3. Writes the response payload at the previous boundary point.
    4. Returns the pointer to the guest.
- **Null-Termination**: All strings passed between host and guest are null-terminated (C-string style) to ensure compatibility across different guest languages.

## Security Model

ARIA follows the principle of least privilege:
- **Capabilities**: Skills must declare requirements (like `http`) in their manifest. The host only wires functions matching these declarations.
- **Resource Limits**: The host enforces execution timeouts and maximum response sizes (e.g., 5MB for HTTP results) to prevent resource exhaustion.
- **Memory Safety**: By appending responses to the end of memory and growing the heap, the host avoids colliding with the guest's internal allocator.

## Payment Flows

ARIA supports two distinct payment paths, gated by different governance mechanisms. Both are backed by the same allowlist / spend-cap / audit machinery in `src/payments/`, but only one of them ever asks a human.

### x402 — autonomous payment (`host_x402_pay`)

A WASM skill hits a resource, gets an HTTP 402, and ARIA pays for it **without asking a human** — governance (rate limit, URL allowlist, per-task cap, per-day cap) runs synchronously inside the host call itself, right after the 402 response reveals `pay_to` / `amount`. See `src/skills/wasm_runtime.rs` (`wire_x402_pay`).

```mermaid
flowchart TD
    A["Skill calls host_x402_pay(url)"] --> B["GET url (no auth headers)"]
    B --> C{"Response?"}

    C -->|"2xx"| D["Return body as-is<br/>transaction_id: null<br/>(no payment needed)"]
    C -->|"402, requirements parse OK"| E["Extract PaymentRequirements<br/>(pay_to, amount)"]
    C -->|"402, requirements unparseable"| F["Error: server doesn't support<br/>the x402/Hedera protocol"]
    C -->|"other HTTP error"| G["Error: not a payment-gated resource"]
    C -->|"network error"| H["Error: network error reaching url"]

    E --> I["Log payment attempt<br/>(unconditional, before any decision)"]
    I --> J{"recent attempts to this URL<br/>in last hour > 10?"}
    J -->|"yes"| K["BLOCKED — aria.rate-limit<br/>'wait before retrying'"]
    J -->|"no"| L{"is_url_allowlisted(agent, url)?"}

    L -->|"no"| M["BLOCKED — aria.allowlist<br/>'add it in Settings → URL Allowlist'"]
    L -->|"yes"| N{"amount > per_task_cap?"}

    N -->|"yes"| O["BLOCKED — aria.spend-limit<br/>(per-task cap exceeded)"]
    N -->|"no"| P["try_reserve_spend()<br/>against rolling 24h per_day_cap"]

    P --> Q{"reserved (within<br/>daily budget)?"}
    Q -->|"no"| R["BLOCKED — aria.spend-limit<br/>(daily cap exceeded)"]
    Q -->|"yes"| S["Audit log: aria.approval-tier<br/>= auto_approved<br/>(x402 NEVER asks a human)"]

    S --> T["X402PaymentVault.pay(requirements)<br/>— executes Hedera transfer"]
    T --> U{"payment tx<br/>succeeded?"}
    U -->|"no"| V["release_spend_hold()"] --> W["Error: Hedera payment failed"]
    U -->|"yes"| X["commit_spend_hold()"]

    X --> Y["Retry original GET with<br/>PAYMENT-SIGNATURE header"]
    Y --> AA{"retry response<br/>2xx?"}
    AA -->|"no"| AB["mark payment delivery_failed"] --> AC["Error: payment sent (tx id included)<br/>but content not delivered<br/>+ server's rejection reason"]
    AA -->|"yes"| AD{"PAYMENT-RESPONSE header<br/>present & decodable?"}

    AD -->|"settle_resp.success = true"| AE["mark SUCCESS<br/>adopt server-settled tx id"]
    AD -->|"settle_resp.success = false"| AF["mark FAILED"]
    AD -->|"missing / undecodable header"| AG["leave status PENDING<br/>(200 received but settlement unconfirmed)"]

    AE --> AH["Parse response body<br/>(JSON or raw string)"]
    AF --> AH
    AG --> AH
    AH --> AI["Return { data, transaction_id,<br/>hashscan_url } to the WASM skill"]

    K --> Z["Structured JSON error<br/>written back to skill / surfaced to LLM"]
    M --> Z
    O --> Z
    R --> Z
    W --> Z
    F --> Z
    G --> Z
    H --> Z
    AC --> Z

    style D fill:#1f6f43,color:#fff
    style AI fill:#1f6f43,color:#fff
    style K fill:#8a1f1f,color:#fff
    style M fill:#8a1f1f,color:#fff
    style O fill:#8a1f1f,color:#fff
    style R fill:#8a1f1f,color:#fff
    style W fill:#8a1f1f,color:#fff
    style AC fill:#8a1f1f,color:#fff
```

**Notes:** rate limiting and the allowlist are keyed on the request **URL**, not the payout account (`pay_to`), since one payout account can serve many distinct resources. Settlement is server-attested via the signed `PAYMENT-RESPONSE` header — if it's missing, ARIA leaves the payment `PENDING` rather than assuming success.

### Direct transfer — human confirm/deny (`hedera_pay`)

A skill with the `hedera_pay` capability can move money directly, so it **must not fire without an explicit human "yes."** Governance runs once at proposal time, then the agent parks the pending action and waits for the user to confirm or deny. See `src/agent/react_loop.rs` (`approve_hold`, `release_hold`).

```mermaid
flowchart TD
    A["LLM emits Action for a skill"] --> B{"skill_requires_confirmation?<br/>(manifest capabilities.hedera_pay)"}
    B -->|"no (e.g. x402_pay skill)"| Z0["Runs immediately —<br/>not gated by this flow"]
    B -->|"yes"| C{"payment_proposal_error?<br/>(recipient empty / amount<br/>not a positive number)"}

    C -->|"invalid"| D1["Error surfaced to chat<br/>step aborted, nothing reserved"]
    C -->|"valid"| E["is_account_allowlisted(agent, recipient)?<br/>+ audit log: aria.allowlist"]

    E -->|"not allowlisted"| F1["BLOCKED — aria.allowlist<br/>Error to chat, step aborted"]
    E -->|"allowlisted"| G{"amount > per_task_cap?"}

    G -->|"yes"| F2["BLOCKED — aria.spend-limit<br/>(per-task cap exceeded)"]
    G -->|"no"| H["try_reserve_spend()<br/>against rolling 24h per_day_cap"]

    H --> I{"reserved (within<br/>daily budget)?"}
    I -->|"no"| F3["BLOCKED — aria.spend-limit<br/>(daily cap exceeded)"]
    I -->|"yes"| J{"amount ≤ governance.auto_under?"}

    J -->|"yes → auto-approved"| K["Audit: aria.approval-tier<br/>= auto_approved"]
    K --> L["Execute immediately<br/>(no Ask, no wait for user)"]

    J -->|"no → needs human"| M["Audit: aria.approval-tier<br/>= approval_required"]
    M --> N["Send AgentEvent::Ask<br/>(kind: Payment) with confirmation<br/>message (recipient, amount, memo…)"]
    N --> O["Persist pending action + fingerprint<br/>+ conversation history to DB<br/>(save_awaiting_confirmation)"]
    O --> P["Wait for the user's next message"]

    P --> Q{"confirmation_decision(reply)"}
    Q -->|"'yes' / 'y' / 'confirm' /<br/>'ok' / 'okay' / 'sure' /<br/>starts with 'go ahead' / 'do it'"| R["Confirmed"]
    Q -->|"'no' / 'n' / 'cancel(led)' /<br/>'deny' / 'denied' / 'stop' /<br/>'don't' / 'dont'"| S["Denied"]
    Q -->|"anything else"| T["ContinueConversation<br/>(treated as revising the request,<br/>not a yes/no)"]

    R --> R1["approve_hold(): re-check<br/>payment_proposal_error"]
    R1 -->|"now invalid"| R2["release hold, clear pending<br/>Error surfaced"]
    R1 -->|"still valid"| R3{"stored fingerprint ==<br/>current fingerprint?"}

    R3 -->|"no — skill/config<br/>changed since the hold"| R4["Release OLD hold<br/>Build refreshed pending action<br/>Re-send Ask with new confirmation<br/>message — user must confirm again"]
    R4 --> P

    R3 -->|"yes — unchanged"| R5["run_skill_raw() executes<br/>the actual Hedera TransferTransaction"]
    R5 --> R6{"execution<br/>succeeded?"}
    R6 -->|"no"| R7["release_spend_hold()<br/>Error surfaced, step ends"]
    R6 -->|"yes"| R8["commit_spend_hold()<br/>spawn background mirror-node<br/>settlement watcher"]
    R8 --> R9["Observation appended to history —<br/>LLM synthesizes the final<br/>user-facing reply"]

    S --> S1["release_hold(): release spend hold<br/>+ clear_pending_action(task_id)"]
    S1 --> S2["'Cancelled — {skill} was<br/>not executed.' sent as Final"]

    T --> T1["Release the OLD hold<br/>clear_pending_action(task_id)"]
    T1 --> T2["User's reply injected as new context —<br/>NOT executed. LLM may propose a<br/>revised transaction (new Ask)"]

    L --> U["Observation → history.<br/>Payment settlement watched<br/>in background"]

    style L fill:#1f6f43,color:#fff
    style R8 fill:#1f6f43,color:#fff
    style S2 fill:#8a1f1f,color:#fff
    style F1 fill:#8a1f1f,color:#fff
    style F2 fill:#8a1f1f,color:#fff
    style F3 fill:#8a1f1f,color:#fff
    style R2 fill:#8a1f1f,color:#fff
    style R7 fill:#8a1f1f,color:#fff
    style D1 fill:#8a1f1f,color:#fff
    style N fill:#8a5a1f,color:#fff
```

**Notes:** the `auto_under` threshold lets small payments skip human confirmation entirely while still passing through the same allowlist/cap checks. Approve/deny matching is deterministic (`confirmation_decision`), not re-interpreted by the LLM, so a reply at the moment money would move can't be reframed by the model. A **fingerprint** (hash of skill + args + execution context + skill/wasm artifact hashes) guards against a stale "yes" executing a transaction that changed after the confirmation was issued — a mismatch releases the old hold and re-asks instead of executing silently.

## Key Technologies
- **Execution**: [Wasmtime](https://wasmtime.dev/)
- **Runtime**: [Tokio](https://tokio.rs/)
- **Storage**: SQLite
- **Serialization**: Serde (JSON)
