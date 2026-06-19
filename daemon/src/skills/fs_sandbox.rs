//! Filesystem sandbox enforcement for fs.* skills.
//!
//! Policy is per-installation config (db-backed, injected via manifest [config]
//! entries with inject=true), NOT per-skill-manifest — the manifest only declares
//! that a skill *needs* fs access (`capabilities.fs = true`); the actual root/
//! allow/deny values come from the user's local config, same as searxng_url etc.
//!
//! Enforcement order:
//!   1. Canonicalize the requested path.
//!   2. Reject anything outside `fs_root` (hard outer boundary, always enforced).
//!   3. Within fs_root, apply exactly one of:
//!      - mode = "whitelist": path must match one of the fs_allow globs.
//!        Empty fs_allow => nothing accessible (fail-safe default).
//!      - mode = "blacklist": path must NOT match any fs_deny glob.
//!        Empty fs_deny => everything under fs_root accessible.
//!      - anything else => fail closed (treated as whitelist with empty allow).

use anyhow::{anyhow, bail};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct FsSandbox {
    root: PathBuf,
    mode: FsMode,
    allow: Vec<String>,
    deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum FsMode {
    Whitelist,
    Blacklist,
    /// Anything unrecognized — fails closed.
    Invalid,
}

impl FsSandbox {
    /// Build sandbox config from already-enriched skill args.
    /// Expects keys injected by `enrich_args`: fs_root, fs_mode, fs_allow, fs_deny.
    /// fs_allow / fs_deny are ';'-separated glob lists.
    pub fn from_args(args: &Value) -> anyhow::Result<Self> {
        let get = |key: &str, default: &str| -> String {
            args.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or(default)
                .to_string()
        };

        let root_raw = get("fs_root", "~/aria-workspace");
        let root = expand_home(&root_raw);

        // Ensure the root exists so canonicalize() below doesn't fail for fresh installs.
        std::fs::create_dir_all(&root)
            .map_err(|e| anyhow!("Could not create fs_root '{}': {}", root.display(), e))?;
        let root = root
            .canonicalize()
            .map_err(|e| anyhow!("Could not resolve fs_root '{}': {}", root.display(), e))?;

        let mode = match get("fs_mode", "whitelist").as_str() {
            "whitelist" => FsMode::Whitelist,
            "blacklist" => FsMode::Blacklist,
            _ => FsMode::Invalid,
        };

        let split = |s: String| -> Vec<String> {
            s.split(';')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        };

        Ok(Self {
            root,
            mode,
            allow: split(get("fs_allow", "")),
            deny: split(get("fs_deny", "")),
        })
    }

    /// Resolve and validate a user/LLM-supplied path against this sandbox.
    /// Returns the canonical absolute path if access is permitted.
    ///
    /// `must_exist`: false for write targets (parent dir is checked instead).
    pub fn resolve(&self, requested: &str, must_exist: bool) -> anyhow::Result<PathBuf> {
        let requested_path = expand_home(requested);

        // Join relative paths against the sandbox root.
        let candidate = if requested_path.is_absolute() {
            requested_path
        } else {
            self.root.join(requested_path)
        };

        let canonical = if must_exist {
            candidate.canonicalize().map_err(|e| {
                anyhow!(
                    "Path '{}' does not exist or is unreadable: {}",
                    requested,
                    e
                )
            })?
        } else {
            // For write targets, canonicalize the parent and rejoin the filename —
            // the file itself may not exist yet.
            let parent = candidate
                .parent()
                .ok_or_else(|| anyhow!("Invalid path '{}'", requested))?;
            let file_name = candidate
                .file_name()
                .ok_or_else(|| anyhow!("Invalid path '{}' — no file name", requested))?;
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("Cannot create directory '{}': {}", parent.display(), e))?;
            parent
                .canonicalize()
                .map_err(|e| anyhow!("Cannot resolve directory '{}': {}", parent.display(), e))?
                .join(file_name)
        };

        // 1. Hard outer boundary — must be within fs_root.
        if !canonical.starts_with(&self.root) {
            bail!(
                "Access denied: '{}' resolves outside the allowed workspace ({})",
                requested,
                self.root.display()
            );
        }

        // 2. Whitelist/blacklist, evaluated relative to fs_root.
        let rel = canonical.strip_prefix(&self.root).unwrap_or(&canonical);
        let rel_str = rel.to_string_lossy();

        match self.mode {
            FsMode::Whitelist => {
                if !self.allow.iter().any(|pat| glob_match(pat, &rel_str)) {
                    bail!(
                        "Access denied: '{}' is not in the fs_allow whitelist",
                        requested
                    );
                }
            }
            FsMode::Blacklist => {
                if self.deny.iter().any(|pat| glob_match(pat, &rel_str)) {
                    bail!(
                        "Access denied: '{}' matches the fs_deny blacklist",
                        requested
                    );
                }
            }
            FsMode::Invalid => {
                bail!(
                    "fs_mode is invalid (must be 'whitelist' or 'blacklist') — denying all access for safety"
                );
            }
        }

        Ok(canonical)
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

/// Minimal glob matcher supporting '*' (any chars, no path separator) and
/// '**' (any chars including path separators). Patterns and input use '/'.
fn glob_match(pattern: &str, input: &str) -> bool {
    // Normalize Windows separators just in case.
    let pattern = pattern.replace('\\', "/");
    let input = input.replace('\\', "/");
    glob_match_inner(pattern.as_bytes(), input.as_bytes())
}

fn glob_match_inner(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(b'*'), _) => {
            // Check for '**'
            if pat.get(1) == Some(&b'*') {
                let rest = &pat[2..];
                // '**' matches zero or more of anything, including '/'
                if glob_match_inner(rest, s) {
                    return true;
                }
                if !s.is_empty() {
                    return glob_match_inner(pat, &s[1..]);
                }
                false
            } else {
                let rest = &pat[1..];
                // single '*' matches zero or more non-'/' chars
                if glob_match_inner(rest, s) {
                    return true;
                }
                if !s.is_empty() && s[0] != b'/' {
                    return glob_match_inner(pat, &s[1..]);
                }
                false
            }
        }
        (Some(p), Some(c)) if p == c => glob_match_inner(&pat[1..], &s[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basic() {
        assert!(glob_match("notes/*.md", "notes/todo.md"));
        assert!(!glob_match("notes/*.md", "notes/sub/todo.md"));
        assert!(glob_match("notes/**", "notes/sub/todo.md"));
        assert!(glob_match("**/*.rs", "src/skills/mod.rs"));
        assert!(!glob_match("*.md", "notes/todo.md"));
        assert!(glob_match("secrets/*", "secrets/keys.env"));
    }
}
