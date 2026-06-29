//! System prompt construction and the skills index shown to the LLM.
//!
//! Every skill always appears at least as a one-line `name: description`
//! entry (so the agent can always answer "what can you do" / pick a skill
//! even on off-trigger phrasing). Skills whose `triggers` match the current
//! user prompt additionally get their full call/output schema + react notes.

use crate::skills::manifest::{SkillManifest, load_manifest};
use crate::skills::paths::get_daemon_root;

// ── Trigger matching ──────────────────────────────────────────────────────────

pub fn prompt_matches_triggers(prompt: &str, triggers: &[String], skills_type: &Option<String>) -> bool {
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

// ── System prompt ─────────────────────────────────────────────────────────────

pub fn system_prompt(user_prompt: &str, skills_type: Option<String>) -> String {
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
            if prompt_matches_triggers(user_prompt, &m.triggers, skills_type) {
                format_skill_block(m)
            } else {
                format!("- {}: {}", m.name, m.description)
            }
        })
        .collect();

    format!("== AVAILABLE SKILLS ==\n{}", lines.join("\n\n"))
}

fn format_skill_block(m: &SkillManifest) -> String {
    let args_example = m
        .call
        .args_schema
        .as_deref()
        .unwrap_or(r#"{"key":"value"}"#);
    let call_line = format!(
        r#"  call:   {{"type":"action","skill":"{}","args":{}}}"#,
        m.name, args_example
    );

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
