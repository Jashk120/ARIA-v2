use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::CONFIG;
use crate::db::Db;

const AGENT_DID: &str = "did:aria:jayesh";
const MAX_REACT_STEPS: usize = 8;

const SYSTEM_PROMPT: &str = r#"You are ARIA, a governed agent runtime. You are helpful, concise, and precise.

You decide whether a user message needs tool use or is just a conversation.

== WHEN TO USE TOOLS ==
Use tools ONLY when the user explicitly wants to interact with files, data, or the system.
- "read my resume", "find files", "rate this document" → use tools
- "hi", "how are you", "explain X", "what is Y" → just reply normally, NO tools

== TOOL USE FORMAT ==
When you need tools, respond ONLY with a JSON object. No other text.

To think before acting:
{"type":"thought","content":"your reasoning here"}

To call a skill:
{"type":"action","skill":"skill_name","args":{"key":"value"}}

To ask the user for confirmation or clarification:
{"type":"ask","content":"your question here"}

To give the final answer after all tool steps:
{"type":"final","content":"your response here"}

For normal conversation (no tools needed):
{"type":"chat","content":"your response here"}

== AVAILABLE SKILLS (STUBBED) ==
- find_files: args: {"path": "~/some/dir/", "pattern": "*.pdf"}
- read_file: args: {"path": "/absolute/path/to/file.txt"}
- rate: args: {"text": "document content here"}
- summarize: args: {"text": "document content here"}
- notify: args: {"message": "text", "channel": "terminal"}

== RULES ==
- Always think before acting (emit a thought first)
- For file operations, use find_files first, then confirm with the user before reading
- Never call a skill without a preceding thought
- Keep thoughts short and practical
- Final answers should be friendly and summarize what was done"#;

// What the TUI actually renders per message
#[derive(Clone)]
enum MsgKind {
    Chat,      // normal agent reply
    Thought,   // 💭
    Action,    // ⚡
    Observe,   // 📋
    Ask,       // ❓
    Final,     // ✅
    You,
    System,
    Error,
}

#[derive(Clone)]
struct ChatMessage {
    kind: MsgKind,
    content: String,
}

impl ChatMessage {
    fn you(content: impl Into<String>) -> Self {
        Self { kind: MsgKind::You, content: content.into() }
    }
    fn system(content: impl Into<String>) -> Self {
        Self { kind: MsgKind::System, content: content.into() }
    }
    fn error(content: impl Into<String>) -> Self {
        Self { kind: MsgKind::Error, content: content.into() }
    }
    fn from_agent_raw(content: impl Into<String>) -> Self {
        Self { kind: MsgKind::Chat, content: content.into() }
    }
}

// Result of one ReAct step sent back through the channel
enum AgentEvent {
    Thought(String),
    Action { skill: String, args: serde_json::Value },
    Observation(String),
    Ask(String),
    Final(String),
    Chat(String),
    Error(String),
    Done,
}

pub async fn run(db: &Db, api_key: &str) -> anyhow::Result<()> {
    // Load history
    let history = db.get_history(AGENT_DID, 50)?;
    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage::system("ARIA v0.5 — Governed Agent Runtime"),
        ChatMessage::system("Type a message or /help"),
    ];

    let mut history_reversed = history.clone();
    history_reversed.reverse();
    for (direction, content) in &history_reversed {
        let kind = if direction == "sent" { MsgKind::You } else { MsgKind::Chat };
        messages.push(ChatMessage { kind, content: content.clone() });
    }

    let mut llm_history: Vec<serde_json::Value> = history_reversed
        .iter()
        .map(|(dir, content)| {
            let role = if dir == "sent" { "user" } else { "assistant" };
            json!({ "role": role, "content": content })
        })
        .collect();

    let mut input = String::new();
    let mut scroll: usize = 0;
    let mut thinking = false;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(32);
    let api_key = api_key.to_string();

    let mut terminal = ratatui::init();

    let result = loop {
        // Drain all pending agent events
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::Done => { thinking = false; }
                AgentEvent::Error(e) => {
                    thinking = false;
                    messages.push(ChatMessage::error(e));
                }
                AgentEvent::Thought(t) => {
                    messages.push(ChatMessage { kind: MsgKind::Thought, content: t });
                }
                AgentEvent::Action { skill, args } => {
                    let args_str = args.as_object()
                        .map(|o| o.iter()
                            .map(|(k, v)| format!("{}={}", k, v.as_str().unwrap_or(&v.to_string())))
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default();
                    messages.push(ChatMessage {
                        kind: MsgKind::Action,
                        content: format!("{}({})", skill, args_str),
                    });
                }
                AgentEvent::Observation(o) => {
                    messages.push(ChatMessage { kind: MsgKind::Observe, content: o });
                }
                AgentEvent::Ask(q) => {
                    thinking = false;
                    messages.push(ChatMessage { kind: MsgKind::Ask, content: q });
                }
                AgentEvent::Final(f) => {
                    thinking = false;
                    db.save_message(AGENT_DID, "received", &f).ok();
                    llm_history.push(json!({ "role": "assistant", "content": f.clone() }));
                    messages.push(ChatMessage { kind: MsgKind::Final, content: f });
                }
                AgentEvent::Chat(c) => {
                    thinking = false;
                    db.save_message(AGENT_DID, "received", &c).ok();
                    llm_history.push(json!({ "role": "assistant", "content": c.clone() }));
                    messages.push(ChatMessage { kind: MsgKind::Chat, content: c });
                }
            }
            scroll = 0;
        }

        terminal.draw(|f| draw(f, &messages, &input, scroll, thinking))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }
                match key.code {
                    KeyCode::Esc => break Ok(()),
                    KeyCode::Enter => {
                        let cmd = input.trim().to_string();
                        input.clear();
                        if cmd.is_empty() || thinking { continue; }

                        match cmd.as_str() {
                            "/quit" | "/exit" | "/q" => break Ok(()),
                            "/help" | "/h" => {
                                messages.push(ChatMessage::system("/help  /quit  /clear  /key"));
                            }
                            "/clear" => {
                                messages.retain(|m| matches!(m.kind, MsgKind::System));
                                scroll = 0;
                            }
                            "/key" => {
                                messages.push(ChatMessage::system(format!(
                                    "Key: {}...{}", &api_key[..4], &api_key[api_key.len()-4..]
                                )));
                            }
                            _ => {
                                db.save_message(AGENT_DID, "sent", &cmd).ok();
                                llm_history.push(json!({ "role": "user", "content": cmd.clone() }));
                                messages.push(ChatMessage::you(cmd.clone()));
                                thinking = true;
                                scroll = 0;

                                let tx = tx.clone();
                                let history_snap = llm_history.clone();
                                let key = api_key.clone();
                                tokio::spawn(async move {
                                    run_react_loop(key, history_snap, tx).await;
                                });
                            }
                        }
                    }
                    KeyCode::Char(c) => { input.push(c); }
                    KeyCode::Backspace => { input.pop(); }
                    KeyCode::Up => {
                        scroll = (scroll + 1).min(messages.len().saturating_sub(1));
                    }
                    KeyCode::Down => {
                        scroll = scroll.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
    };

    ratatui::restore();
    println!("Goodbye.");
    result
}

// The ReAct loop — runs in a spawned task, sends events back to TUI
async fn run_react_loop(
    api_key: String,
    mut history: Vec<serde_json::Value>,
    tx: mpsc::Sender<AgentEvent>,
) {
    for _ in 0..MAX_REACT_STEPS {
        let raw = match call_llm(&api_key, &history).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(format!("LLM error: {}", e))).await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }
        };

        // Try to parse as JSON agent response
        let parsed = parse_agent_response(&raw);

        match parsed {
            // Plain chat — no tools needed
            Some(AgentResponseKind::Chat(content)) => {
                let _ = tx.send(AgentEvent::Chat(content)).await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }

            // Agent is thinking
            Some(AgentResponseKind::Thought(thought)) => {
                let _ = tx.send(AgentEvent::Thought(thought.clone())).await;
                history.push(json!({ "role": "assistant", "content": raw }));
                history.push(json!({ "role": "user", "content": "Continue." }));
            }

            // Agent wants to call a skill
            Some(AgentResponseKind::Action { skill, args }) => {
                let _ = tx.send(AgentEvent::Action { skill: skill.clone(), args: args.clone() }).await;

                // Run stub
                let observation = run_stub_skill(&skill, &args);
                let _ = tx.send(AgentEvent::Observation(observation.clone())).await;

                history.push(json!({ "role": "assistant", "content": raw }));
                history.push(json!({
                    "role": "user",
                    "content": format!("Observation from {}: {}", skill, observation)
                }));
            }

            // Agent is asking the user something
            Some(AgentResponseKind::Ask(question)) => {
                let _ = tx.send(AgentEvent::Ask(question)).await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }

            // Agent has a final answer
            Some(AgentResponseKind::Final(answer)) => {
                let _ = tx.send(AgentEvent::Final(answer)).await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }

            // Couldn't parse JSON — treat as plain chat
            None => {
                let _ = tx.send(AgentEvent::Chat(raw)).await;
                let _ = tx.send(AgentEvent::Done).await;
                return;
            }
        }
    }

    // Hit step limit
    let _ = tx.send(AgentEvent::Error("Max steps reached without a final answer.".into())).await;
    let _ = tx.send(AgentEvent::Done).await;
}

enum AgentResponseKind {
    Chat(String),
    Thought(String),
    Action { skill: String, args: serde_json::Value },
    Ask(String),
    Final(String),
}

fn parse_agent_response(raw: &str) -> Option<AgentResponseKind> {
    // Strip markdown code fences if model wraps in ```json
    let cleaned = raw.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let v: serde_json::Value = serde_json::from_str(cleaned).ok()?;

    match v["type"].as_str()? {
        "chat"    => Some(AgentResponseKind::Chat(v["content"].as_str()?.to_string())),
        "thought" => Some(AgentResponseKind::Thought(v["content"].as_str()?.to_string())),
        "ask"     => Some(AgentResponseKind::Ask(v["content"].as_str()?.to_string())),
        "final"   => Some(AgentResponseKind::Final(v["content"].as_str()?.to_string())),
        "action"  => Some(AgentResponseKind::Action {
            skill: v["skill"].as_str()?.to_string(),
            args: v["args"].clone(),
        }),
        _ => None,
    }
}

// Stub skill dispatcher — returns fake observations until WASM skills are wired
fn run_stub_skill(skill: &str, args: &serde_json::Value) -> String {
    match skill {
        "find_files" => {
            let path = args["path"].as_str().unwrap_or("~/");
            let pattern = args["pattern"].as_str().unwrap_or("*");
            format!(
                "[STUB] Searched {} for '{}' — found: resume_john.pdf, resume_jane.pdf, notes.txt",
                path, pattern
            )
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("unknown");
            format!(
                "[STUB] Read {} — skill not yet implemented. Real WASM skill coming in Phase 2.",
                path
            )
        }
        "rate" => {
            "[STUB] Rated document — score: 7.4/10. Reasoning: skill not yet implemented.".to_string()
        }
        "summarize" => {
            "[STUB] Summarized document — skill not yet implemented.".to_string()
        }
        "notify" => {
            let msg = args["message"].as_str().unwrap_or("");
            format!("[STUB] Would notify: '{}' — skill not yet implemented.", msg)
        }
        other => {
            format!("[STUB] Unknown skill '{}' — not in manifest.", other)
        }
    }
}

async fn call_llm(api_key: &str, history: &[serde_json::Value]) -> anyhow::Result<String> {
    let client = reqwest::Client::new();

    let mut messages = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    messages.extend_from_slice(history);

    let body = json!({
        "model": CONFIG.default_model,
        "messages": messages,
    });

    let resp = client
        .post(CONFIG.openrouter_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenRouter error {}: {}", status, text);
    }

    let json: serde_json::Value = resp.json().await?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No content in response"))?
        .to_string();

    Ok(content)
}

fn draw(f: &mut Frame, messages: &[ChatMessage], input: &str, scroll: usize, thinking: bool) {
    let area = f.area();
    let layout = Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]);
    let [chat_area, input_area] = layout.areas(area);

    let mut lines: Vec<Line> = Vec::new();
    for msg in messages.iter().rev().skip(scroll).rev() {
        let line = render_message(msg);
        lines.push(line);
    }

    if thinking {
        lines.push(Line::from(Span::styled(
            "  ⏳ thinking...",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
        )));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no messages)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let chat_block = Block::default()
        .borders(Borders::TOP)
        .title(" ARIA ")
        .border_style(Style::default().fg(Color::Cyan));
    let chat = Paragraph::new(lines)
        .block(chat_block)
        .wrap(Wrap { trim: false });
    f.render_widget(chat, chat_area);

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let input_para = Paragraph::new(input)
        .block(input_block)
        .style(Style::default().fg(Color::White));
    f.render_widget(input_para, input_area);

    f.set_cursor_position((
        input_area.x + 1 + input.len() as u16 % (input_area.width.saturating_sub(2)),
        input_area.y + 1,
    ));
}

fn render_message(msg: &ChatMessage) -> Line<'static> {
    let (prefix, color, modifier) = match msg.kind {
        MsgKind::You     => ("▸ You:    ", Color::Cyan,    Modifier::BOLD),
        MsgKind::Chat    => ("◂ Aria:   ", Color::Green,   Modifier::empty()),
        MsgKind::Final   => ("✅ Aria:   ", Color::Green,   Modifier::BOLD),
        MsgKind::Thought => ("💭        ", Color::Magenta, Modifier::ITALIC),
        MsgKind::Action  => ("⚡ skill:  ", Color::Yellow,  Modifier::BOLD),
        MsgKind::Observe => ("📋 result: ", Color::Blue,    Modifier::empty()),
        MsgKind::Ask     => ("❓ Aria:   ", Color::Cyan,    Modifier::BOLD),
        MsgKind::System  => ("•         ", Color::DarkGray,Modifier::empty()),
        MsgKind::Error   => ("✗ error:  ", Color::Red,     Modifier::BOLD),
    };

    Line::from(vec![
        Span::styled(prefix.to_string(), Style::default().fg(color).add_modifier(modifier)),
        Span::styled(msg.content.clone(), Style::default().fg(color)),
    ])
}
