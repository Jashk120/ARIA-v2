# Agent-to-Agent Protocol — Scope

Status: **not designed yet.** This doc exists to hold the idea — agent-to-agent communication between ARIA daemons — not to specify a build.

---

## What's known so far

- Agent-to-agent communication means one ARIA daemon's agent talking to another user's ARIA daemon/agent, mediated through an intermediary rather than direct daemon-to-daemon connections. What that intermediary is, is not yet decided.
- This lines up with the "master-agent gateway" concept already in the main
  ARIA roadmap (§1): inter-org agent traffic routes through one master agent
  per org, keeping trust relationships at O(N) rather than O(N×M). How
  agent-to-agent messaging relates to that gateway — same thing, evolution
  of it, or a separate mechanism entirely — is not yet decided.

## Explicitly not yet decided

- What the actual protocol/message format between agents looks like.
- Whether messages are relayed directly through the intermediary, or the
  intermediary just does discovery (agent A asks "how do I reach agent B's
  daemon") while the daemons then talk directly.
- Trust model: does an agent need to be introduced/allowlisted before another
  agent's daemon will accept a message from it? (Likely yes — but not
  decided for this context specifically.)
- How agents authenticate to each other — not yet decided.

## Action

None yet. This is a placeholder to prevent the idea from being lost.
Flesh out only when actually picked up as a work item — don't let
assumptions calcify here before that happens.
