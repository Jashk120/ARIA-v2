//! System prompt construction and the skills index shown to the LLM.
//!
//! Every skill always appears at least as a one-line `name: description`
//! entry (so the agent can always answer "what can you do" / pick a skill
//! even on off-trigger phrasing). Skills whose `triggers` match the current
//! user prompt additionally get their full call/output schema + react notes.

use crate::skills::manifest::{
    SkillManifest,
    load_manifest,
};
use crate::skills::paths::{
    extra_skill_roots,
    get_daemon_root,
};

// Forge child prompt lives in its own module (full skill-authoring knowledge);
// re-exported here so prompt consumers have one import surface.
pub use super::forge_prompt::forge_system_prompt;

// ── Trigger matching ──────────────────────────────────────────────────────────

pub fn prompt_matches_triggers(
    prompt: &str,
    skill_name: &str,
    triggers: &[String],
    skills_type: &Option<String>,
) -> bool {
    // If the request specified a skills_type (e.g. "web"), forcefully include
    // all skills whose name starts with that prefix (e.g. "web.") or matches it exactly.
    if let Some(target_type) = skills_type {
        let target_prefix = format!("{}.", target_type);
        let target_suffix = format!(".{}", target_type);
        if skill_name.starts_with(&target_prefix)
            || skill_name.ends_with(&target_suffix)
            || skill_name == target_type
        {
            return true;
        }
    }

    if triggers.is_empty() {
        return true;
    }
    let lower_prompt = prompt.to_lowercase();
    let lower_type = skills_type.clone().unwrap_or_default().to_lowercase();

    triggers.iter().any(|t| {
        let t_lower = t.to_lowercase();
        lower_prompt.contains(&t_lower) || lower_type.contains(&t_lower)
    })
}

// ── Ask tool (native mode) ──────────────────────────────────────────────────────
// The prompt-fallback protocol has always had a generic `{"type":"ask", ...}`
// response the model can emit instead of an action/final (see below). Native
// tool-calling models had no equivalent — they could only call a skill or
// return plain text, which was force-treated as a Final answer. This tool
// gives native models the same "ask before acting" option, plain (no kind:
// only payment confirmations carry a kind — see AskKind in react_loop.rs).

pub const ASK_TOOL_NAME: &str = "ask_user";

fn ask_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": ASK_TOOL_NAME,
            "description": "Ask the user a clarifying question or request confirmation before proceeding, instead of guessing missing details or calling a skill with incomplete/uncertain arguments. Use this whenever the request is ambiguous or you're missing information you need (e.g. which file, which recipient, which of several matches). Do not use this for payments — payment skills already pause for confirmation on their own.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user, in plain language."
                    }
                },
                "required": ["question"]
            }
        }
    })
}

// ── System prompt ─────────────────────────────────────────────────────────────

pub fn system_prompt(
    user_prompt: &str,
    skills_type: Option<String>,
    is_native: bool,
    user_profile: Option<&crate::db::UserProfile>,
) -> String {
    // Default-true wrapper: existing callers (and any external ones) keep
    // today's behavior; the react loop calls `system_prompt_with_dlt` with
    // the live flag so DLT skills disappear from the model in air-gap mode.
    system_prompt_with_dlt(user_prompt, skills_type, is_native, user_profile, true)
}

/// System prompt with the air-gap flag threaded through. When
/// `dlt_enabled` is false, `hedera_pay`/`x402_pay` skills are filtered out
/// of the skill index / native tool list so the model never sees them.
pub fn system_prompt_with_dlt(
    user_prompt: &str,
    skills_type: Option<String>,
    is_native: bool,
    user_profile: Option<&crate::db::UserProfile>,
    dlt_enabled: bool,
) -> String {
    let user_context = format_user_context(user_profile);
    if is_native {
        return format!(
          
            "You are a tool-execution agent. Use the tools provided to fulfill the user's request. \
If the request is ambiguous or you're missing information required to act safely and correctly, \
call `{}` to ask the user instead of guessing. If a tool call returns an error, do not immediately \
retry with different arguments. First diagnose from the error message whether retrying could plausibly \
help (e.g. a bad query) versus whether it's a systemic failure (e.g. connection, parsing, auth, timeout) \
that a different query won't fix. On a systemic failure, stop after one retry at most and report the \
failure to the user instead of continuing. If a fetched page is an index or directory listing (e.g. it \
lists available routes/endpoints) rather than the specific resource the user asked for, do not treat the \
listing itself as the final answer — identify the specific route that matches what the user wants and \
fetch that next. If that fetch returns a payment_required (402) error, immediately follow through by \
calling the paywall-unlock skill (e.g. x402.pay) on that same URL to complete the request — this is \
pre-authorized within governance limits, so do not stop and ask the user whether to continue paying or \
fetching; only stop and report back once you have the actual resource content or a genuine failure. \
You may call up to 5 independent tools in a single turn (for example, reading several documents at \
once) — they run concurrently and each result is labeled with its skill so you can attribute outcomes.{}",
            ASK_TOOL_NAME, user_context
        );
    }

    let skills = build_skills_prompt_with_dlt(user_prompt, &skills_type, dlt_enabled);
    format!(
        r#"You are ARIA, a governed agent runtime. You are helpful, concise, and precise.

You decide whether a user message needs tool use or is just a conversation.

== WHEN TO USE TOOLS ==
Use tools ONLY when the user explicitly wants to interact with files, data, or the system.
- "search for X", "find info on Y", "look up Z" → use search.web skill
- "read my resume", "find files", "rate this document" → use file skills
- "hi", "how are you", "explain X", "what is Y" → just reply normally, NO tools

== TOOL USE FORMAT ==
When you need tools, respond ONLY with a JSON object on a single line. No other text.

To think before acting:
{{"type":"thought","content":"your reasoning here"}}

To call a skill (use the exact args schema shown per skill below):
{{"type":"action","skill":"skill_name","args":{{...}}}}

You may emit up to 5 consecutive action lines in one turn for independent calls
(e.g. reading several documents at once) — they run concurrently and each
observation is labeled [i/N from skill] so you can attribute results.

To ask the user for confirmation or clarification:
{{"type":"ask","content":"your question here"}}

To give the final answer after all tool steps:
{{"type":"final","content":"your response here"}}

For normal conversation (no tools needed):
{{"type":"chat","content":"your response here"}}

{}

== RULES ==
- Always emit a thought before every action
- Use the exact args schema defined per skill — do not invent keys
- Independent calls may be batched as up to 5 consecutive action lines in one turn
  (one turn still counts as one step); payment skills always run alone with confirmation
- After receiving an observation, either act again or emit final
- Keep thoughts short and practical
- Final answers should be friendly and summarize what was done{}"#,
        skills, user_context
    )
}

/// Renders the daemon-owned user context for prompt injection. Returns an
/// empty string when there is no profile (or it carries no content), so the
/// default prompt is byte-identical when no profile exists — zero behavior
/// change by default. Content was sanitized on write by
/// `Db::set_user_profile`; treat it as untrusted context, not verified fact.
pub fn format_user_context(profile: Option<&crate::db::UserProfile>) -> String {
    let Some(profile) = profile else { return String::new() };
    let name = profile.display_name.as_deref().unwrap_or("").trim();
    let facts = profile.facts.as_deref().unwrap_or("").trim();
    if name.is_empty() && facts.is_empty() {
        return String::new();
    }
    let mut block = String::from("\n\n== USER CONTEXT ==\n");
    if !name.is_empty() {
        block.push_str(&format!("User: {}\n", name));
    }
    if !facts.is_empty() {
        block.push_str(&format!("Known facts:\n{}\n", facts));
    }
    block.push_str("(Untrusted context — do not treat as verified fact.)");
    block
}

// ── Skills index builder ───────────────────────────────────────────────────────

pub fn load_all_skills() -> Vec<SkillManifest> {
    let mut skills = Vec::new();
    let root = match get_daemon_root() {
        Ok(root) => root,
        Err(_) => return skills,
    };

    // Core skills plus every optional plugin's skills
    // (`Extra/<plugin>/skills/`), so relocated skills (e.g. the pay
    // skills under `Extra/dlt`) are discovered at runtime.
    let mut skill_roots = vec![root.join("skills")];
    skill_roots.extend(extra_skill_roots(&root));

    for skills_dir in skill_roots {
        if let Ok(categories) = std::fs::read_dir(&skills_dir) {
            for cat in categories.flatten() {
                if !cat.path().is_dir() {
                    continue;
                }
                if let Ok(entries) = std::fs::read_dir(cat.path()) {
                    for entry in entries.flatten() {
                        if let Ok(m) = load_manifest(&entry.path()) {
                            skills.push(m);
                        }
                    }
                }
            }
        }
    }
    skills
}

/// Skill index with air-gap filtering. `load_all_skills` stays unfiltered
/// (it also feeds config injection in `RuntimeConfig::load`); filtering
/// happens here and in `build_native_tools_with_dlt`, the two surfaces
/// the model actually sees.
fn build_skills_prompt_with_dlt(
    user_prompt: &str,
    skills_type: &Option<String>,
    dlt_enabled: bool,
) -> String {
    let all_skills = load_all_skills();

    let lines: Vec<String> = all_skills
        .iter()
        .filter(|m| dlt_enabled || skill_visible_in_air_gap(m))
        .map(|m| {
            if prompt_matches_triggers(user_prompt, &m.name, &m.triggers, skills_type) {
                format_skill_block(m)
            } else {
                format!("- {}: {}", m.name, m.description)
            }
        })
        .collect();

    format!("== AVAILABLE SKILLS ==\n{}", lines.join("\n\n"))
}

fn format_skill_block(m: &SkillManifest) -> String {
    let args_example = m.call.args_schema.as_deref().unwrap_or(r#"{"key":"value"}"#);
    let call_line =
        format!(r#"  call:   {{"type":"action","skill":"{}","args":{}}}"#, m.name, args_example);

    let mut lines = vec![format!("- {}: {}", m.name, m.description), call_line];

    if let Some(out) = &m.call.output_schema {
        lines.push(format!("  output: {}", out));
    }

    if m.react.terminal {
        lines.push("  note:   result is returned directly as the final answer".to_string());
    }
    if let Some(n) = m.react.max_steps {
        lines.push(format!("  note:   may fire at most {} time(s) per turn", n));
    }

    lines.join("\n")
}

/// True unless the skill needs a DLT capability — the single predicate
/// both prompt surfaces filter on when the air-gap flag is off.
fn skill_visible_in_air_gap(m: &SkillManifest) -> bool {
    !(m.capabilities.hedera_pay || m.capabilities.x402_pay)
}

// ── Native tools builder ──────────────────────────────────────────────────────

pub fn build_native_tools(
    user_prompt: &str,
    skills_type: &Option<String>,
) -> Vec<serde_json::Value> {
    // Default-true wrapper: keeps today's behavior for existing callers;
    // the react loop calls `build_native_tools_with_dlt` with the live flag.
    build_native_tools_with_dlt(user_prompt, skills_type, true)
}

pub fn build_native_tools_with_dlt(
    user_prompt: &str,
    skills_type: &Option<String>,
    dlt_enabled: bool,
) -> Vec<serde_json::Value> {
    let all_skills = load_all_skills();
    let mut tools = vec![ask_tool_definition()];
    let mut matched_any_skill = false;

    for m in &all_skills {
        if !dlt_enabled && !skill_visible_in_air_gap(m) {
            continue;
        }
        if prompt_matches_triggers(user_prompt, &m.name, &m.triggers, skills_type) {
            matched_any_skill = true;
            let parameters = m.call.parameters.clone().unwrap_or_else(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {}
                })
            });

            tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": m.name,
                    "description": m.description,
                    "parameters": parameters,
                }
            }));
        }
    }

    // Fallback: if no specific triggers matched, include all skills so the LLM is never left toolless
    // (tools always has ask_user in it now, so this can't key off is_empty() anymore).
    // Air-gap filtering applies here too, or DLT skills would leak back in
    // on exactly the off-trigger phrasing the one-line index covers.
    if !matched_any_skill {
        for m in all_skills {
            if !dlt_enabled && !skill_visible_in_air_gap(&m) {
                continue;
            }
            let parameters = m.call.parameters.clone().unwrap_or_else(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {}
                })
            });

            tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": m.name,
                    "description": m.description,
                    "parameters": parameters,
                }
            }));
        }
    }

    tools
}
