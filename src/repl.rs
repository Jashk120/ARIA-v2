use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

#[derive(Clone)]
struct ChatMessage {
    sender: String,
    content: String,
}

pub fn run(api_key: &str) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage {
            sender: "system".to_string(),
            content: format!("ARIA v0.5 — API key: {}...{}", &api_key[..4], &api_key[api_key.len()-4..]),
        },
        ChatMessage {
            sender: "system".to_string(),
            content: "Type a message or /help".to_string(),
        },
    ];
    let mut input = String::new();
    let mut scroll: usize = 0;

    let result = loop {
        terminal.draw(|f| draw(f, &messages, &input, scroll))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Esc => break Ok(()),
                KeyCode::Enter => {
                    let cmd = input.trim().to_string();
                    input.clear();
                    scroll = 0;
                    if cmd.is_empty() {
                        continue;
                    }
                    match cmd.as_str() {
                        "/quit" | "/exit" | "/q" => break Ok(()),
                        "/help" | "/h" => {
                            messages.push(ChatMessage { sender: "system".into(), content: "/help, /quit, /clear, /key".into() });
                        }
                        "/clear" => {
                            messages.clear();
                            scroll = 0;
                        }
                        "/key" => {
                            messages.push(ChatMessage { sender: "system".into(), content: format!("Key: {}...{}", &api_key[..4], &api_key[api_key.len()-4..]) });
                        }
                        _ => {
                            messages.push(ChatMessage { sender: "you".into(), content: cmd });
                            messages.push(ChatMessage { sender: "agent".into(), content: "(No skills wired — Phase 1)".into() });
                        }
                    }
                }
                KeyCode::Char(c) => {
                    input.push(c);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Up => {
                    let max_scroll = messages.len().saturating_sub(1);
                    scroll = (scroll + 1).min(max_scroll);
                }
                KeyCode::Down => {
                    scroll = scroll.saturating_sub(1);
                }
                _ => {}
            }
        }
    };

    ratatui::restore();
    println!("Goodbye.");
    result
}

fn draw(f: &mut Frame, messages: &[ChatMessage], input: &str, scroll: usize) {
    let area = f.area();
    let layout = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
    ]);
    let [chat_area, input_area] = layout.areas(area);

    let mut lines: Vec<Line> = Vec::new();
    for msg in messages.iter().rev().skip(scroll).rev() {
        let (prefix, color) = match msg.sender.as_str() {
            "you" => ("▸ You:  ", Color::Cyan),
            "agent" => ("◂ Aria: ", Color::Green),
            "system" => ("•      ", Color::DarkGray),
            _ => ("      ", Color::White),
        };
        lines.push(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(&msg.content, Style::default().fg(color)),
            ])
        );
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  (no messages)", Style::default().fg(Color::DarkGray))));
    }

    let chat_block = Block::default()
        .borders(Borders::TOP)
        .title(" Chat ")
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