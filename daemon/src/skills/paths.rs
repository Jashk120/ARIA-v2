//! Path resolution for skill directories and compiled wasm binaries.
//!
//! NOTE: this currently assumes a cargo workspace layout (skills/<category>/<name>.<category>/
//! and target/wasm32-wasip1/release/). When moving to a distributable binary with
//! installable skills (see ARIA roadmap: `aria add <skill>`), this is the file to
//! change — `skill_dir()` and `wasm_path()` should collapse into a single function
//! pointing at a user data dir (e.g. ~/.aria/skills/<category>/<name>/) containing
//! both manifest.toml and skill.wasm side by side.

use anyhow::{anyhow, bail};
use std::path::PathBuf;

/// Resolve the root directory of the daemon crate.
pub fn get_daemon_root() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var("ARIA_DAEMON_ROOT") {
        return Ok(PathBuf::from(val));
    }
    let exe = std::env::current_exe()?;
    let mut curr = exe.as_path();

    // Climb up from target/debug/aria or similar to find daemon/
    while let Some(parent) = curr.parent() {
        if parent.join("Cargo.toml").exists() && parent.join("src").exists() {
            return Ok(parent.to_path_buf());
        }
        curr = parent;
        if curr.as_os_str().is_empty() || curr.parent().is_none() { break; }
    }

    // Fallback: one level up from wherever we are
    exe.parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("Could not resolve daemon root. Set ARIA_DAEMON_ROOT."))
}

/// Split a skill name like "read.fs" into ("read", "fs").
pub fn split_skill_name(name: &str) -> anyhow::Result<(&str, &str)> {
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    if parts.len() != 2 {
        bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    Ok((parts[0], parts[1]))
}

/// Directory containing a skill's manifest.toml (and source crate).
pub fn skill_dir(name: &str) -> anyhow::Result<PathBuf> {
    let (action, category) = split_skill_name(name)?;
    let root = get_daemon_root()?;

    Ok(root
        .join("skills")
        .join(category)
        .join(format!("{}.{}", action, category)))
}

/// Path to the compiled wasm binary for a skill.
pub fn wasm_path(name: &str) -> anyhow::Result<PathBuf> {
    let (action, category) = split_skill_name(name)?;
    let root = get_daemon_root()?;

    Ok(root
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(format!("{}_{}.wasm", action, category)))
}
