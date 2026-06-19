# ARIA Daemon (`aria-daemon`)

The ARIA Daemon is the core host component of the **Agentic Interface for AI (ARIA)** system. Structured as a modular, high-performance host, the daemon is responsible for managing AI operations, coordinating the `tokio` async runtime, interacting with persistent storage via SQLite, and securely executing AI "skills" through WebAssembly (WASM).

## Features

- **WASM Skill Engine**: Leveraging the `wasmtime` engine, the daemon executes self-describing, isolated WASM skills (such as `search.web`, `read.fs`, `scrape.web`) within heavily sandboxed, linear memory boundaries. 
- **LLM Provider Support**: Flexible configuration routes AI inference natively handling both **Ollama** (`http://localhost:11434`) and **OpenRouter** APIs. 
- **SQLite Persistence**: Maintains crucial connection configurations and runtime data seamlessly by wrapping local SQLite bindings for immediate local access.
- **Headless Runtime & Auto-Start**: Includes native CLI options to install and run itself as a headless background `systemd` user service across Linux environments.
- **REPL Interface**: Provides direct user-facing interactions via an interactive shell to bootstrap models without needing external wrappers.

## Core Commands

The `aria-daemon` executable provides the following primary CLI commands:

- `aria` - Start the interactive chat REPL.
- `aria daemon` - Run the host headless in the background (ideal for instances triggered by `systemd`).
- `aria install` - Installs the daemon as an auto-starting systemd user service (`~/.config/systemd/user/aria-daemon.service`).
- `aria help` - Print usage and exit.

## Project Structure

- `src/main.rs`: Entry point defining the main process bootstrap, SQLite initializing, and handling subcommands (REPL, Daemon, Install).
- `src/agent/`: Core LLM integration and the prompt/ReAct interaction loop bridging user goals with available features.
- `src/config.rs`: Centralized runtime configuration defining URL mappings, target models, and injecting manifest constants into dynamic host environments.
- `src/db/`: Interacting directly with the SQLite datastore configurations and identities.
- `src/repl/`: Front-end interactive shell routines for prompt interactions with standard streams.
- `src/skills/`: Managing WASM modules, loading skills from `skills/`, parsing `.toml` manifests, enforcing memory offsets, and executing sandbox instructions.

## Development & Building

The Daemon relies on its workspace configuration located at `../Cargo.toml`. To build:

```bash
cargo build -p aria-daemon
```

To run directly via cargo (for the REPL):

```bash
cargo run -p aria-daemon
```

## Security & Memory Safety

The Daemon conforms strictly to the ARIA security capability model. WASM skills maintain zero access to Host network or filesystem constraints. All requests are marshaled dynamically through Host FFI string-pointers, mapped exclusively down to explicitly authorized `tokio` networking routines. Memory limits and sizes are strictly controlled directly by the Wasmtime engine context.
