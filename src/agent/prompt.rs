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
use crate::skills::paths::get_daemon_root;

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

pub fn system_prompt(user_prompt: &str, skills_type: Option<String>, is_native: bool) -> String {
    if is_native {
        return format!(
          
            "You are a tool-execution agent. Use the tools provided to fulfill the user's request. \
If the request is ambiguous or you're missing information required to act safely and correctly, \
call `{}` to ask the user instead of guessing. If a tool call returns an error, do not immediately \
retry with different arguments. First diagnose from the error message whether retrying could plausibly \
help (e.g. a bad query) versus whether it's a systemic failure (e.g. connection, parsing, auth, timeout) \
that a different query won't fix. On a systemic failure, stop after one retry at most and report the \
failure to the user instead of continuing.",
            ASK_TOOL_NAME
        );
    }

    let skills = build_skills_prompt(user_prompt, &skills_type);
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
- After receiving an observation, either act again or emit final
- Keep thoughts short and practical
- Final answers should be friendly and summarize what was done"#,
        skills
    )
}

// ── Skills index builder ───────────────────────────────────────────────────────

pub fn load_all_skills() -> Vec<SkillManifest> {
    let mut skills = Vec::new();
    let skills_dir = match get_daemon_root() {
        Ok(root) => root.join("skills"),
        Err(_) => return skills,
    };

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
    skills
}

fn build_skills_prompt(user_prompt: &str, skills_type: &Option<String>) -> String {
    let all_skills = load_all_skills();

    let lines: Vec<String> = all_skills
        .iter()
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

// ── Native tools builder ──────────────────────────────────────────────────────

pub fn build_native_tools(
    user_prompt: &str,
    skills_type: &Option<String>,
) -> Vec<serde_json::Value> {
    let all_skills = load_all_skills();
    let mut tools = vec![ask_tool_definition()];
    let mut matched_any_skill = false;

    for m in &all_skills {
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
    if !matched_any_skill {
        for m in all_skills {
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
