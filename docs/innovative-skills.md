# Innovative Skills — Air-Gapped PS Differentiators

These five skills are the demo-winning layer for the air-gapped enterprise
workbench. Nobody's problem statement asks for them; that's the point.
Common thread: every other team demos "AI that works offline" — these demo
"AI that *proves* it's safe while working offline."

---

## 1. Skill-forge — the agent that grows its own tools

**Concept.** When the user asks for something no skill covers ("convert this
Excel to PDF"), the agent authors a new WASM skill + manifest itself. The
capability request goes to the human for sign-off ("this skill wants
`fs.write` — grant?"). Self-extending agent where every new power requires a
human grant ceremony.

**Demo moment.** "I need Excel→PDF conversion" → agent writes the skill,
presents the capability diff, human grants, skill installs, task completes —
end to end in ~two minutes.

**Why it wins.** No competing team will have self-extension with governed
capability grants. Turns the sandbox + manifest architecture into a story.

**Build notes.** Codegen sandbox (new skill crates built from templates),
manifest-diff review UI, capability grant ceremony reusing the
approve/release hold pattern, install path into `skills/<category>/`.

---

## 2. Classification-marking — built for how govt offices actually work

**Concept.** Every file noting in a PSU/defence unit carries a
classification. Make it automatic: every deliverable the agent produces gets
stamped with handling marking + content hash — headers/footers on Word/PDF,
metadata + signed sidecar alongside.

**Demo moment.** Generate an approval note, open it: classification banner,
hash, handling instructions — confidentiality compliance done by the AI.

**Why it wins.** "AI that does your confidentiality compliance for you"
lands harder with refinery/PSU/defence judges than any model benchmark.
Speaks their daily workflow, not ours.

**Build notes.** Extends the write.pdf / write.xlsx (and future write.docx)
skills with a marking pass; hash via existing `crypto` module; markings
policy in local config, never model-decided.

---

## 3. Birth-certificate provenance

**Concept.** Every generated file embeds its lineage — source document
hashes, model used, human approvals granted — verifiable fully offline.
When the evaluator asks "but how do we *trust* AI output?", hand them a
file that answers for itself.

**Demo moment.** Take any deliverable, run `aria verify <file>` (or the GUI
equivalent): lineage checks out against the local audit chain, all hashes
match, approvals listed.

**Why it wins.** Pairs perfectly with #2: marking says *how to handle it*,
provenance says *why to believe it*. Directly answers the enterprise fear
driving this whole PS.

**Build notes.** Lineage block (JSON sidecar + embedded metadata) written at
seal time from the task chain; verification replays hashes against SQLite
audit chain. No network required — fully air-gapped.

---

## 4. Drawing-diff

**Concept.** Revision comparison for engineering drawings: Rev A vs Rev B
through the on-device vision model + symbol library, outputting a marked
list of what changed. P&ID reviewers do this by eye today.

**Demo moment.** Two scanned P&ID revisions in, change list out — added /
removed / moved symbols with locations, ready for the review meeting.

**Why it wins.** Single most domain-resonant skill in the brief: slow,
skilled, sensitive work that can never go to cloud. Refineries live in
these drawings.

**Build notes.** Needs the OCR/vision read path first; symbol library as
local reference data; diff report as a deliverable (pairs with #2/#3).

---

## 5. Egress contracts

**Concept.** Not just proving zero external calls after the fact: per-task
network policy declared up front ("this task may reach intranet KB at X,
nothing else"), enforced at the host-function layer, every attempt logged.

**Demo moment.** Run a task, show its policy card, show a blocked attempt
being denied live. The network-monitor idea weaponized into a runtime
property.

**Why it wins.** Sovereignty as something the system *does*, not something
the deployment *claims*. Turns the PS's "prove no external calls" line from
a log file into a live capability.

**Build notes.** Policy attached to task creation; host gates
(`host_http_get`, DNS) check the per-task allowlist; denials logged to the
audit chain; GUI policy card + denial feed. Builds on the air-gap flag and
existing sandbox enforcement.
