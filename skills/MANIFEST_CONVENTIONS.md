# Skill Manifest Conventions

`manifest.toml` is read by **two independent parsers** and must satisfy both:

- `src/repl.rs` → `SkillMeta` — used to build the LLM system prompt (`build_skills_prompt`)
- `src/skills.rs` → `SkillManifest` — used by the host runtime (capabilities, config injection, display)

Both use `#[serde(default)]` heavily, so a missing section won't error — it'll
silently degrade (e.g. the LLM sees a useless `{"key":"value"}` placeholder).
There is no validation step that catches this. Treat every field below as
required unless marked optional.

## Required sections

```toml
name        = "action.category"   # must match dir name action.category/ and binary action_category.wasm
version     = "0.1.0"
description = "One line, shown to the LLM as the skill's purpose."

[display]
action = "Human-readable progress line, e.g. \"Searching for {query}\""
# {key} placeholders are substituted from args via render_template (skills.rs).
# Only top-level string args are substituted; non-string values render as "...".

[capabilities]
http = true   # only flag this true if the skill imports host_http_get.
              # Capabilities not declared here are NOT wired into the linker —
              # the WASM module will fail to instantiate if it imports an
              # unwired host function.

[call]
args_schema   = '{"key":"type","key2":"type"}'
output_schema = '{"key":"type"}'
# These two strings are interpolated VERBATIM into the LLM system prompt.
# - Must be valid JSON (not enforced, but treat as if it were).
# - Describe the FULL input the LLM is responsible for. Do NOT include
#   any key that has [config.*].inject = true — those are host-injected
#   and must never be requested from or filled by the model.
# - output_schema describes the SUCCESS shape only. Errors are surfaced
#   to the loop as Err(...) (see skills.rs run_wasm — any {"error":...}
#   returned by the skill is converted to an Err before reaching repl.rs),
#   so do not document an "error" key here.

[react]
max_steps = 3       # optional, default = unlimited within MAX_REACT_STEPS (8).
                     # Set this on any skill that can fail repeatedly
                     # (network calls, lookups) so a bad loop can't burn
                     # the whole step budget on one skill.
terminal  = false    # optional, default false. If true, the skill's raw
                     # output is sent directly as Final — no LLM synthesis
                     # pass. Only use for skills whose raw JSON output is
                     # itself a fit user-facing answer.
```

## `[config.*]` — host-injected values

```toml
[config.some_key]
default = "..."     # used if not set in db
inject  = true       # if true, host writes args["some_key"] = <value> before
                      # the WASM call. The LLM never sees or sets this key —
                      # it must NOT appear in [call].args_schema.
secret  = false      # if true, never log this value (search.web's
                      # brave_api_key is the canonical example)
```

`searxng_url` / `brave_api_key`-style values are injected in **two places**
that must agree:
- `enrich_args` in `skills.rs` (db-backed path, `run_skill`)
- the inline injection in `repl.rs`'s `run_react_loop` (`searxng_url`,
  `brave_api_key` hardcoded — bypasses `enrich_args` entirely)

If you add a new `inject = true` config key, the `repl.rs` inline injection
won't pick it up automatically — either route that skill through
`run_skill` (db path) or add the key to the inline injection block.

## Checklist for a new skill manifest

- [ ] `name` matches `<action>.<category>` and the built binary
      `<action>_<category>.wasm` in `target/wasm32-wasip1/release/`
- [ ] `[display].action` template only references top-level string args
- [ ] `[capabilities]` lists every host import the WASM module uses —
      undeclared imports = instantiation failure at runtime
- [ ] `[call].args_schema` is valid JSON, lists every LLM-supplied arg,
      and excludes all `inject = true` keys
- [ ] `[call].output_schema` documents the success shape only
- [ ] `[react].max_steps` set for any skill with non-deterministic
      failure modes (network, external services)
- [ ] `[react].terminal` only set if raw skill JSON is a valid
      user-facing final answer
- [ ] every `[config.*]` with `inject = true` has a corresponding
      injection path in both `enrich_args` (skills.rs) and, if called
      from the REPL's inline ReAct loop, `run_react_loop` (repl.rs)