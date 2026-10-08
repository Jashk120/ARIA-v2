//! skill-forge: tier-2 recursive subagent loop for authoring new skills.
//!
//! A parent turn that needs a brand-new capability (e.g. "I need Excel→PDF")
//! spawns a forge child via [`run_forge_subagent`]: a bounded child ReAct
//! loop driven by [`forge_system_prompt`](super::forge_prompt::forge_system_prompt)
//! with a fresh child task id, the parent's span as its parent, and a reduced
//! step budget ([`FORGE_MAX_STEPS`]).
//!
//! Trust model (the whole point of the staging design):
//! - The child writes ONLY under `~/.aria/forge-staging/<skill>/`. Unapproved
//!   code NEVER enters `skills/`.
//! - The child toolset is file skills (`read/list/find/write .fs`) + `exec`
//!   (`run.exec`) + three staging-scoped local tools (`forge_write`,
//!   `forge_read`, `forge_build`). Stock fs/exec skills are `fs_root`
//!   sandbox-bound and cannot reach the staging dir or the daemon tree, hence
//!   the local tools; all three are deliberately unprivileged:
//!   `forge_write` is path-contained to the staging root, `forge_read` is
//!   read-only, `forge_build` compiles a scratch copy and never installs.
//! - There is NO install primitive in the child toolset. Install happens only
//!   on the parent side ([`install_staged_skill`]) after a human approves a
//!   `skill_grant` Ask carrying the capability diff — parked/resumed through
//!   the same `awaiting_confirmation` machinery as payment holds.
//! - DLT-capable staged manifests (`hedera_pay`/`x402_pay`) additionally
//!   require `dlt_enabled` on, reusing the air-gap fail-closed rule.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{
    Value,
    json,
};
use tokio::sync::mpsc;

use super::prompt::{
    ASK_TOOL_NAME,
    forge_system_prompt,
};
use super::react_loop::{
    AgentEvent,
    AskKind,
};
use crate::skills::manifest::{
    Capabilities,
    SkillManifest,
    load_manifest,
};
use crate::skills::paths::{
    get_daemon_root,
    skill_dir,
    wasm_path,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Child turn budget: authoring needs real multi-step reasoning
/// (manifest → source → build → fix), but stays bounded well under doubt.
pub const FORGE_MAX_STEPS: usize = 10;

/// Skills the forge child may call through `SkillManager`: file skills +
/// exec only. Everything else gets a deterministic error observation.
const FORGE_SKILL_ALLOWLIST: &[&str] = &["read.fs", "list.fs", "find.fs", "write.fs", "run.exec"];

/// Local (host-side, staging-scoped) tools the child loop dispatches
/// without going through WASM skills.
const FORGE_LOCAL_TOOLS: &[&str] = &["forge_write", "forge_read", "forge_build"];

/// Release wasm build timeout for staged-skill compiles (host child process).
const FORGE_BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Max bytes returned by `forge_read` / embedded in grant cards.
const FORGE_READ_CAP: usize = 32 * 1024;

// ── Result ────────────────────────────────────────────────────────────────────

/// What the forge child produced. Returned by [`run_forge_subagent`] and
/// serialized into the `skill_grant` pending action for the human decision.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForgeResult {
    pub skill_name: String,
    pub staged_dir: String,
    pub manifest_toml: String,
    pub capabilities_diff: String,
    pub build_ok: bool,
    pub build_log: String,
    pub dlt_blocked: bool,
    pub child_task_id: String,
    pub forge_span: String,
    pub report: String,
    pub error: Option<String>,
}

// ── Names + staging paths ─────────────────────────────────────────────────────

/// Validate a forge skill name (`action.category`, lowercase alnum/underscore)
/// and split it into `(action, category)`.
pub fn validate_forge_name(name: &str) -> anyhow::Result<(String, String)> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        anyhow::bail!("Invalid skill name '{}' — expected format: action.category", name);
    }
    for p in &parts {
        if !p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            anyhow::bail!(
                "Invalid skill name '{}' — use lowercase letters, digits, underscores",
                name
            );
        }
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Crate name mapping: `find.fs` → `find_fs` (dots become underscores).
pub fn crate_name_for(action: &str, category: &str) -> String {
    format!("{}_{}", action, category)
}

/// Staging root: unapproved forge output lives here, never in `skills/`.
pub fn forge_staging_root() -> PathBuf {
    let mut root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    root.push(".aria");
    root.push("forge-staging");
    root
}

/// Staging dir for one skill: `~/.aria/forge-staging/<skill>/`.
pub fn forge_staging_dir(skill_name: &str) -> PathBuf {
    forge_staging_root().join(skill_name)
}

/// Scratch build dir inside the daemon tree (workspace membership requires
/// a path under the workspace root): `<daemon>/target/forge-build/<crate>/`.
fn forge_scratch_dir(crate_name: &str) -> anyhow::Result<PathBuf> {
    Ok(get_daemon_root()?.join("target").join("forge-build").join(crate_name))
}

// ── Capability diff ───────────────────────────────────────────────────────────

/// Human-readable capability diff for the grant card: which of the six
/// capabilities the staged manifest requests, and what each one unlocks.
pub fn capability_diff(manifest: &SkillManifest) -> String {
    capability_diff_for(&manifest.capabilities)
}

fn capability_diff_for(caps: &Capabilities) -> String {
    let rows = [
        (caps.fs, "fs", "host_fs_read/write/list/find — sandboxed filesystem I/O"),
        (caps.http, "http", "host_http_get — outbound HTTP (no sockets for guests)"),
        (caps.exec, "exec", "host_exec_run — sandboxed python/sh child processes"),
        (caps.db_query, "db_query", "host_db_query — read-only daemon database queries"),
        (caps.hedera_pay, "hedera_pay", "direct HBAR transfers — REAL MONEY, needs DLT on"),
        (caps.x402_pay, "x402_pay", "paywall-unlock payments — REAL MONEY, needs DLT on"),
    ];
    let mut out = String::from("Requested capabilities:\n");
    for (on, name, meaning) in rows {
        out.push_str(&format!("  [{}] {} — {}\n", if on { "x" } else { " " }, name, meaning));
    }
    out
}

fn dlt_gated(caps: &Capabilities, db: &crate::db::Db) -> bool {
    (caps.hedera_pay || caps.x402_pay) && !crate::config::dlt_enabled_live(db)
}

// ── Grant question + pending action (parent side) ─────────────────────────────

/// The `skill_grant` Ask body: capability diff + build status + staged path.
pub fn forge_grant_question(result: &ForgeResult) -> String {
    format!(
        "Skill Install Grant\n\nSkill: {}\nStaged at: {}\nBuild: {}\n{}\nManifest:\n```toml\n{}\n```\n\nReply \"yes\" to install (moves staging → skills/, registers the crate, builds the release WASM and reloads the skill), or \"no\" to discard staging.\nChild report: {}",
        result.skill_name,
        result.staged_dir,
        if result.build_ok { "OK (release wasm compiled)" } else { "FAILED (see log tail below)" },
        result.capabilities_diff,
        result.manifest_toml,
        result.report,
    )
}

/// Fingerprint over the staged ground truth (not the LLM's words): skill +
/// manifest text + build status. Recomputed at approve time; a mismatch
/// means staging changed since the grant was issued → re-ask.
pub fn forge_fingerprint(skill: &str, manifest_toml: &str, build_ok: bool) -> String {
    crate::crypto::sha256_hex_str(
        &json!({ "skill": skill, "manifest": manifest_toml, "build_ok": build_ok }).to_string(),
    )
}

/// Pending action stored via `save_awaiting_confirmation` for park/resume.
pub fn pending_skill_grant(result: &ForgeResult) -> Value {
    json!({
        "kind": "skill_grant",
        "skill": result.skill_name,
        "staged_dir": result.staged_dir,
        "manifest": result.manifest_toml,
        "build_ok": result.build_ok,
        "report": result.report,
        "fingerprint": forge_fingerprint(&result.skill_name, &result.manifest_toml, result.build_ok),
    })
}

/// True when a parked pending action is a forge grant.
pub fn is_skill_grant_pending(pending: &Value) -> bool {
    pending.get("kind").and_then(|v| v.as_str()) == Some("skill_grant")
}

// ── Staging-scoped local tools (child toolset) ─────────────────────────────────

/// `forge_write {path, content}`: write a file under the staging root only.
/// `path` is staging-relative (`<skill>/manifest.toml`); absolute paths and
/// `..` escapes are rejected. Returns the byte count written.
fn forge_write_local(path: &str, content: &str) -> anyhow::Result<String> {
    let dest = contained_staging_path(path)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, content)?;
    Ok(format!("Wrote {} ({} bytes)", dest.display(), content.len()))
}

/// `forge_read {path}`: read-only. Accepts staging-relative paths plus
/// absolute paths under the daemon root (reference skills, conventions,
/// Cargo.toml) for authoring reference. Everything else is denied —
/// notably `~/.aria` secrets outside staging.
fn forge_read_local(path: &str) -> anyhow::Result<String> {
    let abs = if std::path::Path::new(path).is_absolute() {
        contained_daemon_or_staging_path(path)?
    } else {
        contained_staging_path(path)?
    };
    let bytes = std::fs::read(&abs)?;
    if bytes.len() > FORGE_READ_CAP {
        anyhow::bail!(
            "File {} is {} bytes — over the {} byte read cap",
            abs.display(),
            bytes.len(),
            FORGE_READ_CAP
        );
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// Resolve a staging-relative path, rejecting absolute paths and escapes.
/// The staging root is created first so containment is decided on the
/// canonical root plus a lexical `..` rejection — no filesystem probing.
fn contained_staging_path(path: &str) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    let rel = std::path::Path::new(path);
    if rel.is_absolute() {
        anyhow::bail!("forge_write takes staging-relative paths, got absolute '{}'", path);
    }
    for comp in rel.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("Path '{}' escapes the staging root", path);
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    let root = forge_staging_root();
    std::fs::create_dir_all(&root)?;
    let root_canon = root.canonicalize()?;
    Ok(root_canon.join(rel))
}

/// Absolute reads allowed under the daemon root (references) or staging.
fn contained_daemon_or_staging_path(path: &str) -> anyhow::Result<PathBuf> {
    let staging = forge_staging_root().canonicalize().unwrap_or_else(|_| forge_staging_root());
    let daemon = get_daemon_root()?;
    let daemon_canon = daemon.canonicalize().unwrap_or(daemon);
    let p = PathBuf::from(path);
    if p == staging || p.starts_with(&staging) || p == daemon_canon || p.starts_with(&daemon_canon)
    {
        // Refuse live secrets even under allowed roots.
        let s = p.to_string_lossy();
        if s.contains(".aria") && !(p == staging || p.starts_with(&staging)) {
            anyhow::bail!("Reading '{}' is not allowed", path);
        }
        return Ok(p);
    }
    anyhow::bail!("forge_read is limited to staging and the daemon tree, got '{}'", path)
}

/// Top-level staging subdir a `forge_write` path lands in (`<skill>/...`).
fn staging_topdir(path: &str) -> Option<String> {
    let rel = std::path::Path::new(path);
    if rel.is_absolute() {
        return None;
    }
    rel.components().next().and_then(|c| c.as_os_str().to_str()).map(|s| s.to_string())
}

// ── Workspace membership ──────────────────────────────────────────────────────

/// Ensure `rel` (workspace-relative, e.g. `skills/fs/echo.fs`) is listed in
/// the daemon `[workspace] members`. Returns true when a line was added.
pub fn ensure_workspace_member(rel: &str) -> anyhow::Result<bool> {
    ensure_workspace_member_at(&get_daemon_root()?.join("Cargo.toml"), rel)
}

/// Remove `rel` from workspace members. Returns true when a line was removed.
pub fn remove_workspace_member(rel: &str) -> anyhow::Result<bool> {
    remove_workspace_member_at(&get_daemon_root()?.join("Cargo.toml"), rel)
}

/// `*_at` variants take an explicit manifest path so tests can exercise
/// member registration against a temp Cargo.toml without touching the real
/// tree or the process environment (env mutation would race parallel tests).
fn ensure_workspace_member_at(cargo: &std::path::Path, rel: &str) -> anyhow::Result<bool> {
    let text = std::fs::read_to_string(cargo)?;
    if member_present(&text, rel) {
        return Ok(false);
    }
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut in_members = false;
    for line in lines.iter_mut() {
        let t = line.trim();
        if t.starts_with("members") && t.contains('[') {
            in_members = true;
            if t.contains(']') {
                in_members = false;
            }
            continue;
        }
        if in_members && t == "]" {
            *line = format!("    \"{}\",\n]", rel);
            break;
        }
        if in_members && t.contains(']') {
            // Single-line tail like `"x", ]` — normalize by inserting before `]`.
            *line = line.replacen(']', &format!("\"{}\", ]", rel), 1);
            break;
        }
    }
    std::fs::write(cargo, lines.join("\n") + "\n")?;
    Ok(true)
}

/// Remove `rel` from workspace members. Returns true when a line was removed.
fn remove_workspace_member_at(cargo: &std::path::Path, rel: &str) -> anyhow::Result<bool> {
    let text = std::fs::read_to_string(cargo)?;
    if !member_present(&text, rel) {
        return Ok(false);
    }
    let needle = format!("\"{}\"", rel);
    let kept: Vec<&str> =
        text.lines().filter(|l| !(l.contains(&needle) && l.trim().starts_with('"'))).collect();
    std::fs::write(cargo, kept.join("\n") + "\n")?;
    Ok(true)
}

fn member_present(cargo_text: &str, rel: &str) -> bool {
    let needle = format!("\"{}\"", rel);
    cargo_text.lines().any(|l| l.contains(&needle) && l.trim().starts_with('"'))
}

// ── Compile (host child process, never in the child toolset directly) ─────────

/// Outcome of compiling a staged skill through a scratch workspace member.
pub struct ForgeBuild {
    pub ok: bool,
    pub log: String,
}

/// Compile the staged skill: copy staging → scratch member inside the daemon
/// tree, register the member, run
/// `cargo build -p <crate> --target wasm32-wasip1 --release` with a 5-minute
/// timeout, then unregister + delete scratch. Staging itself is never a
/// member and never built in place.
pub async fn build_staged_skill(skill_name: &str) -> ForgeBuild {
    match build_staged_skill_inner(skill_name).await {
        Ok(b) => b,
        Err(e) => ForgeBuild { ok: false, log: format!("forge build failed: {}", e) },
    }
}

async fn build_staged_skill_inner(skill_name: &str) -> anyhow::Result<ForgeBuild> {
    let (action, category) = validate_forge_name(skill_name)?;
    let crate_name = crate_name_for(&action, &category);
    let staged = forge_staging_dir(skill_name);
    if !staged.join("manifest.toml").exists() {
        anyhow::bail!(
            "Nothing staged for '{}' — no manifest.toml in {}",
            skill_name,
            staged.display()
        );
    }
    // Manifest must parse before we spend a build on it.
    let manifest = load_manifest(&staged)?;
    let _ = manifest;

    let root = get_daemon_root()?;
    let scratch = forge_scratch_dir(&crate_name)?;
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    let scratch_parent =
        scratch.parent().ok_or_else(|| anyhow::anyhow!("Invalid scratch path"))?;
    std::fs::create_dir_all(scratch_parent)?;
    copy_dir_recursive(&staged, &scratch)?;

    let rel = format!("target/forge-build/{}", crate_name);
    ensure_workspace_member(&rel)?;

    let build = run_cargo_build(&root, &crate_name).await;

    // Scratch never persists — the only durable states are staging (pre-grant)
    // and skills/ (post-grant).
    let _ = remove_workspace_member(&rel);
    let _ = std::fs::remove_dir_all(&scratch);

    Ok(build)
}

async fn run_cargo_build(root: &std::path::Path, crate_name: &str) -> ForgeBuild {
    let mut child = match tokio::process::Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg(crate_name)
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("--release")
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ForgeBuild { ok: false, log: format!("cannot spawn cargo: {}", e) };
        }
    };
    match tokio::time::timeout(FORGE_BUILD_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => match child.wait_with_output().await {
            Ok(out) => {
                let mut log = String::from_utf8_lossy(&out.stderr).to_string();
                if !out.stdout.is_empty() {
                    log.push_str(&String::from_utf8_lossy(&out.stdout));
                }
                ForgeBuild { ok: out.status.success(), log: tail_lines(&log, 40) }
            }
            Err(e) => ForgeBuild { ok: false, log: format!("cargo wait failed: {}", e) },
        },
        Ok(Err(e)) => ForgeBuild { ok: false, log: format!("cargo wait failed: {}", e) },
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait_with_output().await;
            ForgeBuild { ok: false, log: "cargo build timed out after 5 minutes".to_string() }
        }
    }
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Move a directory: same-filesystem fast path, recursive copy + delete
/// across mounts. Shared by install (staging → skills/) and its tests.
fn move_dir(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    if std::fs::rename(src, dst).is_err() {
        copy_dir_recursive(src, dst)?;
        std::fs::remove_dir_all(src)?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

// ── Install / discard (parent side, post-grant only) ──────────────────────────

/// Install a staged skill after a human grant: move staging → skills/,
/// register the workspace member, build the release WASM, reload the skill.
/// Fails closed: existing skills are never overwritten, and DLT-capable
/// manifests require `dlt_enabled` on.
pub async fn install_staged_skill(
    db: &crate::db::Db,
    skills: &crate::skills::SkillManager,
    skill_name: &str,
) -> anyhow::Result<String> {
    let (action, category) = validate_forge_name(skill_name)?;
    let crate_name = crate_name_for(&action, &category);
    let staged = forge_staging_dir(skill_name);
    if !staged.join("manifest.toml").exists() {
        anyhow::bail!("Nothing staged for '{}'", skill_name);
    }
    let manifest = load_manifest(&staged)?;
    if manifest.name != skill_name {
        anyhow::bail!(
            "Staged manifest name '{}' does not match requested skill '{}'",
            manifest.name,
            skill_name
        );
    }
    if dlt_gated(&manifest.capabilities, db) {
        anyhow::bail!(
            "Skill '{}' blocked: DLT disabled (air-gap mode) — re-enable with `aria dlt on`",
            skill_name
        );
    }

    let root = get_daemon_root()?;
    let dest = root.join("skills").join(&category).join(skill_name);
    if dest.exists() {
        anyhow::bail!(
            "Skill '{}' already exists at {} — forge never overwrites",
            skill_name,
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Same-filesystem fast path, copy fallback across mounts.
    move_dir(&staged, &dest)?;

    let rel = format!("skills/{}/{}", category, skill_name);
    ensure_workspace_member(&rel)?;

    // Release wasm build (5-min timeout, host child process).
    let build = run_cargo_build(&root, &crate_name).await;
    if !build.ok {
        anyhow::bail!(
            "Installed '{}' but the release wasm build failed:\n{}",
            skill_name,
            build.log
        );
    }

    skills.invalidate(skill_name);

    // Verify discovery exactly the way the runtime does it.
    let dir = skill_dir(skill_name)?;
    let _ = load_manifest(&dir)?;
    let wasm = wasm_path(skill_name)?;
    if !wasm.exists() {
        anyhow::bail!("Build reported success but {} is missing", wasm.display());
    }
    Ok(format!(
        "Installed '{}' (crate {}) → {} — release wasm ready, skill reloaded.",
        skill_name,
        crate_name,
        dir.display()
    ))
}

/// Discard staging after a denied grant (or an abandoned forge): remove any
/// scratch member, delete scratch + staging. Never touches `skills/`.
pub fn discard_staged_skill(skill_name: &str) -> String {
    let (action, category) = match validate_forge_name(skill_name) {
        Ok(p) => p,
        Err(e) => return format!("Discarded nothing — {}", e),
    };
    let crate_name = crate_name_for(&action, &category);
    let scratch_rel = format!("target/forge-build/{}", crate_name);
    let _ = remove_workspace_member(&scratch_rel);
    if let Ok(scratch) = forge_scratch_dir(&crate_name) {
        let _ = std::fs::remove_dir_all(scratch);
    }
    let staged = forge_staging_dir(skill_name);
    if staged.exists() {
        match std::fs::remove_dir_all(&staged) {
            Ok(()) => format!("Discarded — staged skill '{}' was deleted.", skill_name),
            Err(e) => format!("Could not delete staging for '{}': {}", skill_name, e),
        }
    } else {
        format!("Discarded — nothing was staged for '{}'.", skill_name)
    }
}

// ── Child loop ────────────────────────────────────────────────────────────────

/// Spawn the forge child: a bounded ReAct loop with the forge system prompt,
/// a FRESH child task id, the parent span as its parent, and [`FORGE_MAX_STEPS`]
/// turns. The child writes to staging via its toolset; it never installs.
/// Returns the staged path + parsed manifest caps + build status.
///
/// Model plumbing reuses the existing `run_react_loop` machinery
/// (`call_llm_streaming` + response parsing); only the system prompt, the
/// toolset, and the executor (staging-scoped) are forge-specific. No
/// model-call behavior of the parent loop changes.
pub async fn run_forge_subagent(
    db: std::sync::Arc<crate::db::Db>,
    skills: std::sync::Arc<crate::skills::SkillManager>,
    parent_task_id: String,
    request: String,
) -> ForgeResult {
    let child_task_id = db.new_task_id();
    let forge_span = super::react_loop::new_span_id();
    let _ = std::fs::create_dir_all(forge_staging_root());

    let staging_display = forge_staging_root().display().to_string();
    let sys = format!(
        "{}\n\n== CURRENT TASK ==\nRequest: {}\nParent task: {}\nStaging root: {}\n\nProtocol: write the skill tree to `<staging-root>/<action.category>/` with forge_write (paths like `<action.category>/manifest.toml`), read references with forge_read, compile-check with forge_build {{\"skill\":\"<action.category>\"}}. Write manifest FIRST, then source, then build, then emit a final report with the skill name + capability diff and stop. You do NOT install — the parent owns the grant + install.",
        forge_system_prompt(),
        request,
        parent_task_id,
        staging_display,
    );

    let api_key = db.get_config("openrouter_api_key").ok().flatten().unwrap_or_default();
    let injected_config = crate::config::RuntimeConfig::load(&db).injected_config;
    let agent_did = format!("forge:{}", parent_task_id);
    let ctx = ForgeCtx {
        db: db.clone(),
        skills: skills.clone(),
        injected_config,
        agent_did,
        child_task_id: child_task_id.clone(),
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(100);
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // Child progress is traced at debug; the parent surfaces only the
            // final ForgeResult + the skill_grant Ask.
        }
    });

    let mut history = vec![json!({ "role": "user", "content": request })];
    let mut written: HashSet<String> = HashSet::new();
    let mut report = String::new();
    let mut step = 0;
    let mut fallback = false;
    let mut llm_error: Option<String> = None;

    while step < FORGE_MAX_STEPS {
        let settings = crate::config::resolve_llm(&db);
        let dlt_enabled = crate::config::dlt_enabled_live(&db);
        let tools = if fallback { None } else { Some(build_forge_tools(&request, dlt_enabled)) };
        let sys_for_turn = if fallback { fallback_forge_prompt(&request) } else { sys.clone() };

        let stream_result = super::react_loop::call_llm_streaming(
            &settings,
            &api_key,
            &child_task_id,
            &sys_for_turn,
            &history,
            &tx,
            tools,
            !fallback,
        )
        .await;
        let (raw, tool_calls) = match stream_result {
            Ok(r) => r,
            Err(e) => {
                let s = e.to_string();
                if !fallback && s.starts_with("TOOL_REJECTION_ERROR") {
                    fallback = true;
                    continue;
                }
                llm_error = Some(s);
                break;
            }
        };

        // Parse: native tool calls, or prompt-fallback JSON lines.
        let mut parsed = Vec::new();
        if !fallback {
            if !tool_calls.is_empty() {
                for tc in tool_calls.iter() {
                    if let Some(func) = tc.get("function")
                        && let (Some(name), Some(args_str)) =
                            (func.get("name"), func.get("arguments"))
                    {
                        let name = name.as_str().unwrap_or_default().to_string();
                        let args_text = args_str.as_str().unwrap_or_default();
                        let args: Value =
                            serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
                        if name == ASK_TOOL_NAME {
                            let q = args
                                .get("question")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no question)")
                                .to_string();
                            parsed.push(super::react_loop::AgentResponseKind::Ask(q));
                        } else {
                            parsed.push(super::react_loop::AgentResponseKind::Action {
                                skill: name,
                                args,
                            });
                        }
                    }
                }
            } else if !raw.is_empty() {
                if super::react_loop::is_question(&raw) {
                    parsed.push(super::react_loop::AgentResponseKind::Ask(raw.clone()));
                } else {
                    parsed.push(super::react_loop::AgentResponseKind::Final(raw.clone()));
                }
            } else {
                break;
            }
        } else {
            parsed = super::react_loop::parse_agent_responses(&raw);
            if parsed.is_empty() {
                break;
            }
        }

        let mut turn_done = false;
        for kind in parsed {
            match kind {
                super::react_loop::AgentResponseKind::Chat(c)
                | super::react_loop::AgentResponseKind::Thought(c) => {
                    history.push(json!({ "role": "assistant", "content": c }));
                }
                super::react_loop::AgentResponseKind::Action { skill, args } => {
                    let (observation, is_error) =
                        dispatch_forge_action(&ctx, &skill, &args, &mut written).await;
                    let label = if is_error { "Error" } else { "Observation" };
                    history.push(json!({
                        "role": "user",
                        "content": format!("{} from {}: {}", label, skill, observation)
                    }));
                }
                super::react_loop::AgentResponseKind::Ask(q) => {
                    // No user is reachable from the child — the question becomes
                    // the report and ends the loop.
                    report = q;
                    turn_done = true;
                    break;
                }
                super::react_loop::AgentResponseKind::Final(f) => {
                    report = f;
                    turn_done = true;
                    break;
                }
            }
        }
        if turn_done {
            break;
        }
        step += 1;
    }

    finalize_forge(&db, written, child_task_id, forge_span, report, llm_error).await
}

/// Shared context for one forge child turn: handles + identity. Bundles the
/// dispatch parameters so `dispatch_forge_action` stays a 4-argument function.
struct ForgeCtx {
    db: std::sync::Arc<crate::db::Db>,
    skills: std::sync::Arc<crate::skills::SkillManager>,
    injected_config: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    agent_did: String,
    child_task_id: String,
}

/// Execute one child action: local staging tools first, then the allowlisted
/// file/exec skills. Anything else (including any install-shaped primitive —
/// none exists) is a deterministic error observation.
async fn dispatch_forge_action(
    ctx: &ForgeCtx,
    skill: &str,
    args: &Value,
    written: &mut HashSet<String>,
) -> (String, bool) {
    if FORGE_LOCAL_TOOLS.contains(&skill) {
        if skill == "forge_write" {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            return match forge_write_local(path, content) {
                Ok(msg) => {
                    if let Some(top) = staging_topdir(path) {
                        written.insert(top);
                    }
                    (msg, false)
                }
                Err(e) => (e.to_string(), true),
            };
        }
        if skill == "forge_read" {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            return match forge_read_local(path) {
                Ok(s) => (s, false),
                Err(e) => (e.to_string(), true),
            };
        }
        let name = args.get("skill").and_then(|v| v.as_str()).unwrap_or("");
        if validate_forge_name(name).is_err() {
            return (
                format!("forge_build needs a valid skill name action.category, got '{}'", name),
                true,
            );
        }
        let build = build_staged_skill(name).await;
        let status = if build.ok { "BUILD OK" } else { "BUILD FAILED" };
        return (format!("{} for '{}':\n{}", status, name, build.log), !build.ok);
    }
    if !FORGE_SKILL_ALLOWLIST.contains(&skill) {
        return (
            format!(
                "Skill '{}' is not in the forge toolset (file skills + exec only). Install happens parent-side after grant.",
                skill
            ),
            true,
        );
    }
    if let Some(msg) = super::react_loop::dlt_blocked_error(&ctx.db, skill) {
        return (msg, true);
    }
    let mut enriched = args.clone();
    if let Some(obj) = enriched.as_object_mut()
        && let Some(cfg) = ctx.injected_config.get(skill)
    {
        for (k, v) in cfg {
            obj.insert(k.clone(), json!(v));
        }
    }
    match ctx
        .skills
        .run_skill_raw(
            skill,
            &enriched,
            Some(ctx.db.clone()),
            None,
            None,
            ctx.agent_did.clone(),
            Some(ctx.child_task_id.clone()),
        )
        .await
    {
        Ok(val) => (val.to_string(), false),
        Err(e) => (e.to_string(), true),
    }
}

/// Ground-truth the child outcome from staging files (never from LLM text):
/// exactly one top-level staged skill must exist, its manifest must parse,
/// then run the authoritative release build.
async fn finalize_forge(
    db: &std::sync::Arc<crate::db::Db>,
    written: HashSet<String>,
    child_task_id: String,
    forge_span: String,
    report: String,
    llm_error: Option<String>,
) -> ForgeResult {
    let err_result = |skill_name: &str, error: String| ForgeResult {
        skill_name: skill_name.to_string(),
        staged_dir: forge_staging_dir(skill_name).display().to_string(),
        manifest_toml: String::new(),
        capabilities_diff: String::new(),
        build_ok: false,
        build_log: String::new(),
        dlt_blocked: false,
        child_task_id: child_task_id.clone(),
        forge_span: forge_span.clone(),
        report: report.clone(),
        error: Some(error),
    };
    // Reconcile what the child claims with what is actually staged: prefer
    // the tracked topdirs, fall back to scanning the staging root.
    let mut candidates: Vec<String> = written.into_iter().collect();
    if candidates.is_empty()
        && let Ok(entries) = std::fs::read_dir(forge_staging_root())
    {
        for e in entries.flatten() {
            if e.path().is_dir()
                && let Some(name) = e.file_name().to_str()
            {
                candidates.push(name.to_string());
            }
        }
    }
    if candidates.len() != 1 {
        let detail = match llm_error {
            Some(e) => format!(" forge loop ended with LLM error: {}", e),
            None => String::new(),
        };
        let first = candidates.first().cloned().unwrap_or_default();
        return err_result(
            &first,
            format!("Expected exactly one staged skill, found {}.{}", candidates.len(), detail),
        );
    }
    let skill_name = candidates.pop().unwrap();
    let staged = forge_staging_dir(&skill_name);
    let manifest_text = match std::fs::read_to_string(staged.join("manifest.toml")) {
        Ok(t) => t,
        Err(e) => {
            return err_result(&skill_name, format!("Staged manifest unreadable: {}", e));
        }
    };
    let manifest = match load_manifest(&staged) {
        Ok(m) => m,
        Err(e) => return err_result(&skill_name, format!("Staged manifest invalid: {}", e)),
    };
    let build = build_staged_skill(&skill_name).await;
    ForgeResult {
        skill_name: skill_name.clone(),
        staged_dir: staged.display().to_string(),
        manifest_toml: manifest_text,
        capabilities_diff: capability_diff(&manifest),
        build_ok: build.ok,
        build_log: build.log,
        dlt_blocked: dlt_gated(&manifest.capabilities, db),
        child_task_id,
        forge_span,
        report,
        error: None,
    }
}

// ── Child toolset construction ────────────────────────────────────────────────

/// Native tools for the child: staging-scoped local tools + allowlisted file
/// skills + exec + ask. DLT skills can never appear (not in the allowlist).
fn build_forge_tools(request: &str, dlt_enabled: bool) -> Vec<Value> {
    let mut tools = forge_local_tool_defs();
    for t in super::prompt::build_native_tools_with_dlt(&request.to_string(), &None, dlt_enabled) {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_default();
        if name == ASK_TOOL_NAME || FORGE_SKILL_ALLOWLIST.contains(&name) {
            tools.push(t);
        }
    }
    tools
}

fn forge_local_tool_defs() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "forge_write",
                "description": "Write a file under the forge staging root. Path is staging-relative like '<action.category>/manifest.toml'. Absolute paths and '..' escapes are rejected. Parent dirs are created.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Staging-relative destination path" },
                        "content": { "type": "string", "description": "Full file content to write" }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "forge_read",
                "description": "Read a file (max 32KB). Staging-relative paths read staging; absolute paths under the daemon root read authoring references (existing skills, conventions). Nothing else is readable.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Staging-relative or daemon-root-absolute path" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "forge_build",
                "description": "Compile-check a staged skill: copies staging to a scratch workspace member and runs cargo build -p <crate> --target wasm32-wasip1 --release (5-min timeout). Never installs. Use after manifest + source are written.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "skill": { "type": "string", "description": "Skill name action.category" }
                    },
                    "required": ["skill"]
                }
            }
        }),
    ]
}

/// Prompt-fallback variant: forge system prompt plus the reduced toolset with
/// call schemas, so non-native models emit the same action shapes.
fn fallback_forge_prompt(request: &str) -> String {
    let dlt_enabled = true;
    let mut blocks = Vec::new();
    for m in super::prompt::load_all_skills() {
        if !FORGE_SKILL_ALLOWLIST.contains(&m.name.as_str()) {
            continue;
        }
        let args = m.call.args_schema.as_deref().unwrap_or(r#"{"key":"value"}"#);
        blocks.push(format!(
            "- {}: {}\n  call:   {{\"type\":\"action\",\"skill\":\"{}\",\"args\":{}}}",
            m.name, m.description, m.name, args
        ));
    }
    blocks.push("- forge_write: write a staging file\n  call:   {\"type\":\"action\",\"skill\":\"forge_write\",\"args\":{\"path\":\"<skill>/manifest.toml\",\"content\":\"...\"}}".to_string());
    blocks.push("- forge_read: read staging or a daemon-tree reference\n  call:   {\"type\":\"action\",\"skill\":\"forge_read\",\"args\":{\"path\":\"...\"}}".to_string());
    blocks.push("- forge_build: compile-check a staged skill\n  call:   {\"type\":\"action\",\"skill\":\"forge_build\",\"args\":{\"skill\":\"<action.category>\"}}".to_string());
    let _ = (request, dlt_enabled);
    format!(
        "{}\n\n== AVAILABLE SKILLS (forge toolset: file skills + exec + staging tools) ==\n{}",
        forge_system_prompt(),
        blocks.join("\n\n")
    )
}

// ── Grant approval (parent side, mirrors approve_hold) ────────────────────────

/// Outcome of approving a parked skill grant — the exact effect of a human
/// replying "yes" in chat. Mirrors [`ApproveHoldOutcome`](super::react_loop::ApproveHoldOutcome)
/// so the chat resume path handles grants with the same determinism as holds.
pub enum GrantOutcome {
    /// Staging verified + installed. Pending already cleared.
    Installed { observation: String },
    /// Staging changed since the grant was issued. Nothing installed; the
    /// caller should re-ask with `question` and park `refreshed_pending`.
    FingerprintChanged { question: String, refreshed_pending: Value },
    /// DLT-capable manifest while air-gap is on. Nothing installed and the
    /// pending grant stays parked — the user can enable DLT and approve again.
    Refused { message: String },
    /// Install failed (nothing staged, name mismatch, build failure…).
    /// Pending already cleared; staging state is described in `message`.
    Failed { message: String },
}

/// Execute a parked skill grant: fingerprint check against staged ground
/// truth, air-gap gate, then [`install_staged_skill`]. Deterministic — no
/// model calls.
pub async fn approve_skill_grant(
    db: &std::sync::Arc<crate::db::Db>,
    skills: &std::sync::Arc<crate::skills::SkillManager>,
    task_id: &str,
    pending: &Value,
) -> GrantOutcome {
    let skill = pending.get("skill").and_then(|v| v.as_str()).unwrap_or_default();
    let stored_fingerprint = pending.get("fingerprint").and_then(|v| v.as_str()).unwrap_or_default();
    let stored_build_ok = pending.get("build_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let stored_report = pending.get("report").and_then(|v| v.as_str()).unwrap_or_default();
    let staged = forge_staging_dir(skill);
    let manifest_text = match std::fs::read_to_string(staged.join("manifest.toml")) {
        Ok(t) => t,
        Err(_) => {
            let _ = db.clear_pending_action(task_id);
            return GrantOutcome::Failed {
                message: format!("Grant for '{}' failed — nothing is staged anymore.", skill),
            };
        }
    };
    if forge_fingerprint(skill, &manifest_text, stored_build_ok) != stored_fingerprint {
        let rebuilt = ForgeResult {
            skill_name: skill.to_string(),
            staged_dir: staged.display().to_string(),
            manifest_toml: manifest_text.clone(),
            capabilities_diff: load_manifest(&staged)
                .map(|m| capability_diff(&m))
                .unwrap_or_else(|_| "Manifest no longer parses.".to_string()),
            build_ok: stored_build_ok,
            build_log: String::new(),
            dlt_blocked: false,
            child_task_id: String::new(),
            forge_span: String::new(),
            report: stored_report.to_string(),
            error: None,
        };
        return GrantOutcome::FingerprintChanged {
            question: forge_grant_question(&rebuilt),
            refreshed_pending: pending_skill_grant(&rebuilt),
        };
    }
    let manifest = match load_manifest(&staged) {
        Ok(m) => m,
        Err(e) => {
            let _ = db.clear_pending_action(task_id);
            return GrantOutcome::Failed {
                message: format!("Grant for '{}' failed — staged manifest invalid: {}", skill, e),
            };
        }
    };
    if dlt_gated(&manifest.capabilities, db) {
        return GrantOutcome::Refused {
            message: format!(
                "Skill '{}' blocked: DLT disabled (air-gap mode) — re-enable with `aria dlt on`, then approve again",
                skill
            ),
        };
    }
    let _ = db.clear_pending_action(task_id);
    match install_staged_skill(db, skills, skill).await {
        Ok(summary) => GrantOutcome::Installed { observation: summary },
        Err(e) => GrantOutcome::Failed { message: format!("Install of '{}' failed: {}", skill, e) },
    }
}

/// Resume context when the grant reply is neither yes nor no: the grant stays
/// parked, the loop reconsiders with the user's words attached.
pub fn skill_grant_resume_context(pending: &Value, user_prompt: &str) -> String {
    format!(
        "A skill install is awaiting a grant decision.\n\nPending grant:\n{}\n\nThe user has NOT granted this install.\n\nUser reply:\n{}\n\nDo not install anything. If the user wants the skill with changes, say what would change and present a NEW grant request only after the staging matches.",
        serde_json::to_string_pretty(pending).unwrap_or_else(|_| pending.to_string()),
        serde_json::to_string(user_prompt).unwrap_or_else(|_| format!("{:?}", user_prompt)),
    )
}

// ── Spawn trigger + parent request flow ───────────────────────────────────────

/// Deterministic entry heuristic: explicit `forge:` prefix, skill-authoring
/// phrasing, or the demo's "I need <artifact>" conversion shape. Returns the
/// forge request with any prefix stripped.
pub fn forge_triggered_by(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if let Some(rest) = trimmed.strip_prefix("forge:").or_else(|| trimmed.strip_prefix("Forge:")) {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(rest.to_string());
        }
    }
    let lower = trimmed.to_lowercase();
    for phrase in [
        "new skill",
        "create a skill",
        "create skill",
        "build a skill",
        "build me a skill",
        "make me a skill",
        "make a skill",
        "write a skill",
        "forge a skill",
        "generate a skill",
    ] {
        if lower.contains(phrase) {
            return Some(trimmed.to_string());
        }
    }
    if lower.contains("i need")
        && (lower.contains("excel")
            || lower.contains("pdf")
            || lower.contains("spreadsheet")
            || lower.contains("convert")
            || lower.contains("→")
            || lower.contains("->"))
    {
        return Some(trimmed.to_string());
    }
    None
}

/// Parent-side spawn + grant: run the forge child, then park on a
/// `skill_grant` Ask carrying the capability diff — the same
/// Ask/park/resume shape as payment holds. Called instead of
/// `run_react_loop` for forge-triggering tasks; grant resolution runs inside
/// `run_react_loop`'s resume path on the next message.
pub async fn forge_request_flow(
    db: std::sync::Arc<crate::db::Db>,
    skills: std::sync::Arc<crate::skills::SkillManager>,
    tx: mpsc::Sender<AgentEvent>,
    task_id: String,
    mut history: Vec<Value>,
    request: String,
) -> anyhow::Result<()> {
    let result = run_forge_subagent(db.clone(), skills, task_id.clone(), request).await;
    if let Some(error) = result.error {
        let _ = tx.send(AgentEvent::Error { content: error }).await;
        let _ = tx.send(AgentEvent::Done).await;
        return Ok(());
    }
    let question = forge_grant_question(&result);
    let _ = tx
        .send(AgentEvent::Ask {
            content: question.clone(),
            task_id: task_id.clone(),
            kind: Some(AskKind::SkillGrant),
        })
        .await;
    history.push(json!({
        "role": "assistant",
        "content": format!(
            "Forged skill '{}' staged at {} — proposed install awaiting human grant.",
            result.skill_name, result.staged_dir
        )
    }));
    let history_json = serde_json::to_string(&history).unwrap_or_default();
    let pending_json = serde_json::to_string(&pending_skill_grant(&result)).unwrap_or_default();
    let _ = db.save_awaiting_confirmation(&task_id, &history_json, &pending_json);
    let _ = tx.send(AgentEvent::Done).await;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scaffold (deterministic staging writer) ───────────────────────────────────

    /// What to stage. The child loop produces equivalent trees through
    /// `forge_write`; this constructor is the deterministic primitive the parent
    /// and the tests use (no LLM needed to verify the grant/install path).
    pub struct ForgeSpec {
        pub name: String,
        pub description: String,
        pub triggers: Vec<String>,
        pub http: bool,
        pub fs: bool,
        pub exec: bool,
        pub db_query: bool,
        pub hedera_pay: bool,
        pub x402_pay: bool,
        pub args_schema: String,
        pub output_schema: String,
        pub action_template: String,
    }

    /// Write a complete skill crate tree to staging. Returns the staged dir.
    /// Overwrites any previous staging for the same name (staging is disposable).
    pub fn scaffold_forge_skill(spec: &ForgeSpec) -> anyhow::Result<PathBuf> {
        let (action, category) = validate_forge_name(&spec.name)?;
        let crate_name = crate_name_for(&action, &category);
        let dir = forge_staging_dir(&spec.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(dir.join("src"))?;

        std::fs::write(dir.join("manifest.toml"), scaffold_manifest(spec))?;
        std::fs::write(dir.join("Cargo.toml"), scaffold_cargo_toml(&crate_name))?;
        std::fs::write(dir.join("src").join("lib.rs"), scaffold_lib_rs(spec))?;
        Ok(dir)
    }

    fn scaffold_manifest(spec: &ForgeSpec) -> String {
        let triggers =
            spec.triggers.iter().map(|t| format!("\"{}\"", t)).collect::<Vec<_>>().join(", ");
        // Sandbox config must be complete: FsSandbox defaults to
        // whitelist-with-empty-allow (deny-all), so a lone fs_root would
        // brick the skill. fs skills follow find.fs (blacklist under ~/);
        // exec skills follow run.exec (whitelist * under ~/aria-workspace).
        let fs_config = if spec.exec {
            r#"
[config.fs_root]
default = "~/aria-workspace"
inject  = true
secret  = false

[config.fs_mode]
default = "whitelist"
inject  = true
secret  = false

[config.fs_allow]
default = "*"
inject  = true
secret  = false

[config.fs_deny]
default = ""
inject  = true
secret  = false
"#
        } else if spec.fs {
            r#"
[config.fs_root]
default = "~/"
inject  = true
secret  = false

[config.fs_mode]
default = "blacklist"
inject  = true
secret  = false

[config.fs_allow]
default = ""
inject  = true
secret  = false

[config.fs_deny]
default = ".git;node_modules;target;__pycache__"
inject  = true
secret  = false
"#
        } else {
            ""
        };
        format!(
            r#"name        = "{name}"
version     = "0.1.0"
description = "{description}"

triggers = [{triggers}]

[display]
action = "{action_template}"

[capabilities]
http       = {http}
fs         = {fs}
hedera_pay = {hedera_pay}
db_query   = {db_query}
x402_pay   = {x402_pay}
exec       = {exec}

[call]
args_schema   = '{args_schema}'
output_schema = '{output_schema}'

[call.parameters]
type = "object"

[react]
max_steps = 3
{fs_config}"#,
            name = spec.name,
            description = spec.description,
            triggers = triggers,
            action_template = spec.action_template,
            http = spec.http,
            fs = spec.fs,
            hedera_pay = spec.hedera_pay,
            db_query = spec.db_query,
            x402_pay = spec.x402_pay,
            exec = spec.exec,
            args_schema = spec.args_schema,
            output_schema = spec.output_schema,
            fs_config = fs_config,
        )
    }

    fn scaffold_cargo_toml(crate_name: &str) -> String {
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
serde = {{ workspace = true }}
serde_json = {{ workspace = true }}
"#,
            crate_name = crate_name,
        )
    }

    /// Minimal-but-real guest crate: alloc/run ABI, packed u64 returns, and host
    /// imports for exactly the declared capabilities (fs → list, http → get,
    /// exec → run). Undeclared imports are never emitted — an unwired import
    /// would fail instantiation at runtime.
    fn scaffold_lib_rs(spec: &ForgeSpec) -> String {
        let mut imports = String::new();
        if spec.fs {
            imports.push_str(
            r#"    /// host_fs_list(path_ptr, path_len) -> packed(ptr,len) of JSON array bytes, or 0 on error
    fn host_fs_list(path_ptr: *const u8, path_len: usize) -> u64;
"#,
        );
        }
        if spec.http {
            imports.push_str(
            r#"    /// host_http_get(url_ptr, url_len, headers_ptr, headers_len) -> packed bytes, negative sentinel on error
    fn host_http_get(
        url_ptr: *const u8,
        url_len: usize,
        headers_ptr: *const u8,
        headers_len: usize,
    ) -> u64;
"#,
        );
        }
        if spec.exec {
            imports.push_str(
            r#"    /// host_exec_run(lang_ptr, lang_len, code_ptr, code_len, timeout_ms) -> packed JSON bytes, or 0 on error
    fn host_exec_run(
        lang_ptr: *const u8,
        lang_len: usize,
        code_ptr: *const u8,
        code_len: usize,
        timeout_ms: i32,
    ) -> u64;
"#,
        );
        }

        let mut logic = String::new();
        if spec.exec {
            logic.push_str(
            r#"
    let language = args.get("language").and_then(|v| v.as_str()).unwrap_or("sh");
    let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code.is_empty() {
        return Err("No code provided — pass 'code' with source to execute".to_string());
    }
    let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(10000).clamp(100, 60000) as i32;
    let raw = host_call_exec(language, code, timeout_ms)?;
    let out: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Bad JSON from host_exec_run: {}", e))?;
    if let Some(err) = out.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    return Ok(out);"#,
        );
        } else if spec.http {
            logic.push_str(
                r#"
    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if url.is_empty() {
        return Err("No url provided — pass 'url' to fetch".to_string());
    }
    let raw = host_call_http_get(url)?;
    return Ok(json!({ "url": url, "body": raw }));"#,
            );
        } else if spec.fs {
            logic.push_str(
                r#"
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let raw = host_call_fs_list(path)?;
    let entries: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Bad JSON from host_fs_list: {}", e))?;
    return Ok(json!({ "path": path, "entries": entries }));"#,
            );
        } else {
            logic.push_str(
                r#"
    return Ok(json!({ "ok": true, "args": args }));"#,
            );
        }

        let mut wrappers = String::new();
        if spec.fs {
            wrappers.push_str(
                r#"
fn host_call_fs_list(path: &str) -> Result<String, String> {
    let packed = unsafe {
        let p = path.as_bytes();
        host_fs_list(p.as_ptr(), p.len())
    };
    if packed == 0 {
        return Err(format!(
            "host_fs_list returned NULL — list of \"{}\" failed (access denied or path not found)",
            path
        ));
    }
    Ok(take_wasm_string(packed))
}
"#,
            );
        }
        if spec.http {
            wrappers.push_str(
                r#"
fn host_call_http_get(url: &str) -> Result<String, String> {
    let packed = unsafe {
        let u = url.as_bytes();
        let h = b"";
        host_http_get(u.as_ptr(), u.len(), h.as_ptr(), h.len())
    };
    if packed == 0 || (packed as i64) < 0 {
        return Err(format!("host_http_get failed for \"{}\"", url));
    }
    Ok(take_wasm_string(packed))
}
"#,
            );
        }
        if spec.exec {
            wrappers.push_str(
            r#"
fn host_call_exec(language: &str, code: &str, timeout_ms: i32) -> Result<String, String> {
    let packed = unsafe {
        let l = language.as_bytes();
        let c = code.as_bytes();
        host_exec_run(l.as_ptr(), l.len(), c.as_ptr(), c.len(), timeout_ms)
    };
    if packed == 0 {
        return Err(
            "host_exec_run returned NULL — execution failed (denied, unsupported language, or host error)"
                .to_string(),
        );
    }
    Ok(take_wasm_string(packed))
}
"#,
        );
        }

        format!(
            r#"//! {name} — {description}
//! Staged by ARIA skill-forge; installed only after a human grants capabilities.

use serde_json::{{
    Value,
    json,
}};

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {{
{imports}}}

// ── Entry point ───────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn alloc(size: usize) -> *mut u8 {{
    let mut buf = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}}

#[unsafe(no_mangle)]
pub extern "C" fn run(input_ptr: *const u8, input_len: usize) -> u64 {{
    let input = unsafe {{
        let slice = std::slice::from_raw_parts(input_ptr, input_len);
        std::str::from_utf8(slice).unwrap_or("")
    }};

    let output = match execute(input) {{
        Ok(v) => v.to_string(),
        Err(e) => json!({{ "error": e }}).to_string(),
    }};

    let len = output.len();
    let ptr = to_wasm_ptr(output);
    ((ptr as u64) << 32) | (len as u64)
}}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {{
    let args: Value =
        serde_json::from_str(input).map_err(|e| format!("Invalid input: {{}}", e))?;
{logic}
}}

// ── Host call wrappers ────────────────────────────────────────────────────────
{wrappers}
fn take_wasm_string(packed: u64) -> String {{
    let (ptr, len) = unpack_ptr_len(packed);
    unsafe {{
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        let owned = String::from_utf8_lossy(slice).to_string();
        let _ = Vec::from_raw_parts(ptr as *mut u8, len, len);
        owned
    }}
}}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unpack_ptr_len(packed: u64) -> (usize, usize) {{
    ((packed >> 32) as usize, (packed & 0xFFFFFFFF) as usize)
}}

fn to_wasm_ptr(s: String) -> *mut u8 {{
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}}
"#,
            name = spec.name,
            description = spec.description,
            imports = imports,
            logic = logic,
            wrappers = wrappers,
        )
    }

    fn test_spec(name: &str) -> ForgeSpec {
        ForgeSpec {
            name: name.to_string(),
            description: "Test echo skill forged in unit tests".to_string(),
            triggers: vec!["echo".to_string()],
            http: false,
            fs: true,
            exec: false,
            db_query: false,
            hedera_pay: false,
            x402_pay: false,
            args_schema: r#"{"path":"string"}"#.to_string(),
            output_schema: r#"{"path":"string","entries":"array"}"#.to_string(),
            action_template: "Echoing {path}".to_string(),
        }
    }

    #[test]
    fn forge_name_validation_accepts_action_category() {
        assert_eq!(
            validate_forge_name("echo.test").unwrap(),
            ("echo".to_string(), "test".to_string())
        );
        assert_eq!(crate_name_for("echo", "test"), "echo_test");
    }

    #[test]
    fn forge_name_validation_rejects_bad_names() {
        assert!(validate_forge_name("no-dot").is_err());
        assert!(validate_forge_name("UPPER.test").is_err());
        assert!(validate_forge_name("a.b.c").is_err());
        assert!(validate_forge_name(".test").is_err());
        assert!(validate_forge_name("has space.test").is_err());
    }

    #[test]
    fn forge_write_rejects_escapes_and_absolute_paths() {
        assert!(forge_write_local("/tmp/evil.toml", "x").is_err());
        assert!(forge_write_local("../../evil.toml", "x").is_err());
        assert!(forge_write_local("ok.forge/manifest.toml", "test").is_ok());
        // Cleanup the probe file (staging is disposable by design).
        let _ = std::fs::remove_dir_all(forge_staging_dir("ok.forge"));
    }

    #[test]
    fn capability_diff_marks_requested_caps() {
        let caps = Capabilities { fs: true, exec: true, ..Default::default() };
        let diff = capability_diff_for(&caps);
        assert!(diff.contains("[x] fs"));
        assert!(diff.contains("[x] exec"));
        assert!(diff.contains("[ ] http"));
        assert!(diff.contains("[ ] hedera_pay"));
    }

    #[test]
    fn scaffold_writes_complete_staged_tree() {
        let dir = scaffold_forge_skill(&test_spec("echo.forge")).unwrap();
        assert!(dir.join("manifest.toml").exists());
        assert!(dir.join("Cargo.toml").exists());
        assert!(dir.join("src").join("lib.rs").exists());
        let manifest = load_manifest(&dir).unwrap();
        assert_eq!(manifest.name, "echo.forge");
        assert!(manifest.capabilities.fs);
        assert!(!manifest.capabilities.exec);
        let src = std::fs::read_to_string(dir.join("src").join("lib.rs")).unwrap();
        assert!(src.contains("host_fs_list"));
        assert!(!src.contains("host_exec_run"));
        assert!(src.contains("#[unsafe(no_mangle)]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_pending_round_trips_skill_and_fingerprint() {
        let result = ForgeResult {
            skill_name: "echo.forge".to_string(),
            staged_dir: "/tmp/staged".to_string(),
            manifest_toml: "name = \"echo.forge\"".to_string(),
            capabilities_diff: "[x] fs".to_string(),
            build_ok: true,
            build_log: "ok".to_string(),
            dlt_blocked: false,
            child_task_id: "child-1".to_string(),
            forge_span: "span-1".to_string(),
            report: "done".to_string(),
            error: None,
        };
        let pending = pending_skill_grant(&result);
        assert!(is_skill_grant_pending(&pending));
        assert_eq!(pending["skill"], json!("echo.forge"));
        assert_eq!(
            pending["fingerprint"],
            json!(forge_fingerprint("echo.forge", "name = \"echo.forge\"", true))
        );
        let question = forge_grant_question(&result);
        assert!(question.contains("echo.forge"));
        assert!(question.contains("[x] fs"));
    }

    #[test]
    fn forge_trigger_routes_demo_request() {
        assert!(forge_triggered_by("I need Excel→PDF").is_some());
        assert!(forge_triggered_by("forge: convert csv to pdf").is_some());
        assert!(forge_triggered_by("Create a skill that lists PDFs").is_some());
        assert!(forge_triggered_by("hi, how are you").is_none());
        assert!(forge_triggered_by("find my resume").is_none());
    }

    #[test]
    fn ask_kind_skill_grant_serializes_for_gui() {
        let ev = AgentEvent::Ask {
            content: "grant?".to_string(),
            task_id: "t-1".to_string(),
            kind: Some(AskKind::SkillGrant),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains(r#""kind":"skill_grant""#));
    }

    /// Grant/install path test with zero environment mutation (env writes
    /// would race parallel tests that resolve skills through the daemon
    /// root): stage for real, move into a temp skills tree (exercising the
    /// cross-device copy fallback), validate the manifest, and round-trip
    /// member registration against a temp Cargo.toml. The live-tree
    /// discovery half is covered by the `#[ignore]`d end-to-end test.
    #[test]
    fn install_moves_staging_to_skills_and_discovers() {
        let tmp = std::env::temp_dir().join(format!("forge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("skills")).unwrap();
        let cargo = tmp.join("Cargo.toml");
        std::fs::write(&cargo, "[workspace]\nmembers = [\n    \".\",\n]\n").unwrap();

        // Stage through the real scaffolder, then drive the pure install
        // helpers (not the wasm build — that needs the real workspace).
        let staged = scaffold_forge_skill(&test_spec("grant.forge")).unwrap();
        assert!(staged.exists());
        // Move = the install core (mirrors install_staged_skill pre-build).
        let dest = tmp.join("skills").join("forge").join("grant.forge");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        move_dir(&staged, &dest).unwrap();
        assert!(!staged.exists());
        assert!(crate::skills::manifest::load_manifest(&dest).is_ok());
        // Workspace member registration round-trips.
        assert!(ensure_workspace_member_at(&cargo, "skills/forge/grant.forge").unwrap());
        assert!(!ensure_workspace_member_at(&cargo, "skills/forge/grant.forge").unwrap());
        assert!(remove_workspace_member_at(&cargo, "skills/forge/grant.forge").unwrap());
        assert!(!remove_workspace_member_at(&cargo, "skills/forge/grant.forge").unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(forge_staging_dir("grant.forge"));
    }

    /// Real end-to-end forge against the live tree: scaffold → release wasm
    /// build → grant approve → installed + callable → full cleanup, plus the
    /// deny path. `#[ignore]`: mutates the real `skills/` tree and takes
    /// minutes (release wasm build). Run explicitly:
    /// `cargo test --bin aria forge_end_to_end -- --ignored --nocapture`.
    #[ignore]
    #[tokio::test]
    async fn forge_end_to_end_ping_forge() {
        let db = std::sync::Arc::new(crate::db::Db::new().unwrap());
        let skills = std::sync::Arc::new(crate::skills::SkillManager::new().unwrap());

        // Idempotence: a previous interrupted run must not block this one.
        let _ = std::fs::remove_dir_all(forge_staging_dir("ping.forge"));
        if let Ok(root) = crate::skills::paths::get_daemon_root() {
            let _ = std::fs::remove_dir_all(root.join("skills").join("forge").join("ping.forge"));
            let _ = remove_workspace_member("skills/forge/ping.forge");
        }

        // 1. Scaffold (what the child loop's forge_write calls produce).
        let staged = scaffold_forge_skill(&ForgeSpec {
            name: "ping.forge".to_string(),
            description: "E2E probe: list a directory".to_string(),
            triggers: vec!["ping".to_string()],
            http: false,
            fs: true,
            exec: false,
            db_query: false,
            hedera_pay: false,
            x402_pay: false,
            args_schema: r#"{"path":"string"}"#.to_string(),
            output_schema: r#"{"path":"string","entries":"array"}"#.to_string(),
            action_template: "Pinging {path}".to_string(),
        })
        .unwrap();
        let manifest_text = std::fs::read_to_string(staged.join("manifest.toml")).unwrap();

        // 2. Authoritative release build through a scratch member.
        let build = build_staged_skill("ping.forge").await;
        assert!(build.ok, "staged build failed:\n{}", build.log);

        // 3. Grant approve (fingerprint over staged ground truth).
        let fp = forge_fingerprint("ping.forge", &manifest_text, build.ok);
        let pending = json!({
            "kind": "skill_grant",
            "skill": "ping.forge",
            "build_ok": build.ok,
            "manifest": manifest_text,
            "fingerprint": fp,
        });
        match approve_skill_grant(&db, &skills, "forge-e2e-task", &pending).await {
            GrantOutcome::Installed { observation } => println!("installed: {}", observation),
            other => panic!(
                "expected Installed, got {}",
                match other {
                    GrantOutcome::FingerprintChanged { .. } => "FingerprintChanged",
                    GrantOutcome::Refused { message } => panic!("Refused: {}", message),
                    GrantOutcome::Failed { message } => panic!("Failed: {}", message),
                    GrantOutcome::Installed { .. } => unreachable!(),
                }
            ),
        }

        // 4. Discovered via skill_dir + load_all_skills path, wasm present.
        let dir = skill_dir("ping.forge").unwrap();
        assert!(dir.join("manifest.toml").exists());
        assert!(wasm_path("ping.forge").unwrap().exists());

        // 5. Callable: list the installed dir through the fresh module.
        // `run_skill` (not raw) so manifest [config.*] injection applies.
        let out = skills
            .run_skill(
                "ping.forge",
                &serde_json::json!({"path": dir.to_string_lossy()}),
                Some(db.clone()),
                None,
                None,
                "forge:e2e".to_string(),
                None,
            )
            .await
            .expect("staged skill must run");
        assert!(out.get("entries").is_some(), "unexpected output: {}", out);

        // 6. Cleanup: installed tree + member + cache entry removed.
        assert!(remove_workspace_member("skills/forge/ping.forge").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
        skills.invalidate("ping.forge");
        assert!(!dir.exists());

        // 7. Deny path: staged skill is discarded, skills/ untouched.
        let deny_staged = scaffold_forge_skill(&ForgeSpec {
            name: "deny.forge".to_string(),
            description: "E2E probe: never installed".to_string(),
            triggers: vec![],
            http: false,
            fs: false,
            exec: false,
            db_query: false,
            hedera_pay: false,
            x402_pay: false,
            args_schema: "{}".to_string(),
            output_schema: r#"{"ok":"bool"}"#.to_string(),
            action_template: "Denying".to_string(),
        })
        .unwrap();
        assert!(deny_staged.exists());
        let msg = discard_staged_skill("deny.forge");
        assert!(msg.contains("Discarded"), "{}", msg);
        assert!(!deny_staged.exists());
    }
}
