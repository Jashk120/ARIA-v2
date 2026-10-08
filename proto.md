# ARIA GUI ↔ Daemon Protocol

This document outlines the data structures exchanged between the ARIA GUI and the ARIA Daemon over the local TCP socket (`127.0.0.1:5005`). 

## 1. Client Request (GUI → Daemon)
To initiate a task, the GUI opens a TCP connection and sends a one-off JSON payload. 

**Structure:**
```json
{
  "task": "String",
  "Type": "String (Optional)",
  "images": ["String (Optional)"],
  "task_id": "String (Optional)",
  "history": [{ "role": "user | assistant", "content": "String" }]
}
```

- `task`: The user's prompt or instruction.
- `Type`: Indicates the category or group of skills to inject into the daemon (e.g., `"fs"`, `"web"`, `"os"`). This restricts or focuses the daemon to a specific subset of tool capabilities.
- `images`: Optional list of absolute local file paths (e.g. `"/abs/a.png"`) that the daemon reads itself and forwards to the LLM as vision parts. Paths only — not base64. If any path is missing, not a file, or larger than 10 MiB, the daemon replies with an `error` event naming the path, followed by `done`, without running the task. Omit the field when no images are needed.
- `task_id` (optional): Resume a task previously paused on a human confirmation (`ask`). When present and the named task is in `awaiting_confirmation`, the daemon reloads that task's parked history and treats `task` as the human's reply. Otherwise a fresh task is started and this field is ignored.
- `history` (optional): Prior conversation turns, oldest first, used only when starting a **fresh** task. The daemon seeds the ReAct loop's LLM context with these turns (capped to the most recent 40), so a follow-up message keeps its conversation context instead of starting from nothing. The daemon ensures the final turn is the current `task`, so it is safe to include the in-flight user message. Ignored on the `task_id` resume path. Omit or send `[]` for stateless one-off callers.

**Example:**
```json
{
  "task": "Find all log files modified today",
  "Type": "fs"
}
```

With images:
```json
{
  "task": "What is wrong with this screenshot?",
  "Type": "fs",
  "images": ["/abs/a.png", "/abs/b.jpg"]
}
```

Note: the TCP request reader reads a single ~16 KiB chunk, so inline base64 image payloads do not fit. That is why the field carries local file paths, and inline image bytes are not supported today.

## 1b. Single-Shot Request/Response Verbs

Unlike a task submission (which streams events), these write ONE JSON line back and close the socket.

Read current settings:

Request:
```json
{"query": "query_llm_settings"}
```

Response:
```json
{"type":"llm_settings","url_template":"","token":"","url":"","model":"","api_key":"","resolved_url":"http://127.0.0.1:8000/v1/chat/completions","resolved_model":"gemma-4-31b-it"}
```

`url_template`, `token`, `url`, `model`, `api_key` are the raw stored values (empty string when unset). `resolved_url` / `resolved_model` are what the daemon will actually use.

Write settings:

Request:
```json
{"mutate": "mutate_llm_settings", "payload": {"url_template":"https://example.com/{TOKEN}/chat","token":"s3cret","url":"http://127.0.0.1:8000","model":"gemma-4-31b-it","api_key":"sk-..."}}
```

Response:
```json
{"type":"ok","settings":{ ...same shape as the query response... }}
```

Only the keys present in `payload` are written. A JSON `null` (or an empty string) CLEARS that key so it falls back to the environment/default. Unknown payload keys are ignored.

Any failure, or an unrecognised verb, returns:
```json
{"type":"query_error","message":"..."}
```

Field meanings: `url_template` is a FULL endpoint URL used verbatim when set (it may contain the literal `{TOKEN}`, which is replaced by `token`); `url` is a base/endpoint URL that gets the usual normalization (`/v1/chat/completions` appended when missing); `model` is the model name; `api_key` is sent as `Authorization: Bearer <key>` (the header is omitted entirely when empty).

## 2. Event Stream (Daemon → GUI)
Once the task is submitted, the daemon replies with a stream of newline-delimited JSON objects representing the agent's Thought/Action/Observation loop execution.

**Base Structure:**
Every event has a `type` string (all lowercase).
```json
{
  "type": "thought | action | observation | ask | token | final | chat | error | done",
  // additional fields depend on the type...
}
```

### Event Variants:

- **Thought**
  The agent's internal reasoning.
  ```json
  { "type": "thought", "content": "I should search the file system." }
  ```
- **Action**
  The agent calling a skill/tool.
  ```json
  { 
    "type": "action", 
    "skill": "find.fs", 
    "args": { "query": "*.log", "dir": "/var/log" } 
  }
  ```
- **Observation**
  The result of a skill execution, returned back to the agent (and optionally displayed in the UI).
  ```json
  { "type": "observation", "content": "Found 3 files: syslog, auth.log, dmesg" }
  ```
- **Ask**
  The agent asking the user a clarifying question before proceeding.
  ```json
  { "type": "ask", "content": "Should I compress these logs?" }
  ```
- **Token (Streamed Chunk)**
  Raw streaming text from the LLM, used when generating unstructured (chat) responses. Print directly without newlines.
  ```json
  { "type": "token", "content": "Hello" }
  ```
- **Final**
  The agent's final synthesized response or answer to the user's task.
  ```json
  { "type": "final", "content": "I found 3 log files." }
  ```
- **Chat**
  A fully assembled unstructured chat response (used when no tools were needed).
  ```json
  { "type": "chat", "content": "How can I help you today?" }
  ```
- **Error**
  An error string if something went wrong in the loop.
  ```json
  { "type": "error", "content": "LLM API failure" }
  ```
- **Done**
  Signals the end of the stream for the current task. No more events will follow.
  ```json
  { "type": "done" }
  ```

## 3. GUI Internal State (`ReactEvent`)
For reference, inside the Rust GUI code (`gui/src/llm/mod.rs`), the JSON `DaemonEvent`s are mapped into standard `ReactEvent` enumerations so the Slint UI bridge can easily react to state transitions.

```rust
pub enum ReactEvent {
    LlmToken(String),
    LlmDone { text: String },
    DaemonStarted { skill_type: String },
    DaemonThought(String),
    DaemonAction { skill: String },
    DaemonOutput { content: String },  // Maps from Observation
    DaemonDone,
    Finished,
    Error(String),
}
```
