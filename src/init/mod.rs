use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::backend::Registry;

const SKILL_MD: &str = include_str!("../../skills/SKILL.md");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Silently update any installed skills whose version doesn't match the binary.
pub fn auto_update(registry: &Registry) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    for config_dir in [".claude", ".codex"] {
        let skill_dir = PathBuf::from(&home).join(config_dir).join("skills/dbg");
        let version_file = skill_dir.join(".version");
        if !version_file.exists() {
            continue;
        }
        let installed = std::fs::read_to_string(&version_file).unwrap_or_default();
        if installed.trim() == VERSION {
            continue;
        }
        // Version mismatch — update silently
        let agent_name = if config_dir == ".claude" { "Claude Code" } else { "Codex" };
        let _ = init_agent(registry, config_dir, agent_name);
    }
}

pub fn run_init(target: &str, registry: &Registry) -> Result<()> {
    match target {
        "claude" => init_agent(registry, ".claude", "Claude Code"),
        "codex" => init_agent(registry, ".codex", "Codex"),
        _ => bail!("unknown init target: {target} (use: claude, codex, agent-context)"),
    }
}

/// Print the YAML frontmatter of the embedded `SKILL.md` to stdout.
///
/// Intended for a harness SessionStart hook: the frontmatter enumerates
/// the phrases that should trigger the `dbg` skill, and harnesses that
/// discover tools via ToolSearch (Claude Code's recent behavior) would
/// otherwise only see the bare name "dbg" — which reveals nothing about
/// when to reach for it. Piping this to a session-start reminder puts
/// the trigger contract in the model's context directly.
///
/// Emits the frontmatter bytes verbatim (including the `---` fences) so
/// the receiver can keep it in its original YAML form or strip the
/// fences as needed.
pub fn emit_skill_yaml() {
    if let Some(fm) = extract_frontmatter(SKILL_MD) {
        println!("---\n{fm}\n---");
    } else {
        // No frontmatter found — dump the whole file so nothing silently
        // drops. This is a build-time invariant violation rather than a
        // user error, so no error return.
        print!("{SKILL_MD}");
    }
}

/// Return the YAML frontmatter block of a markdown document (the
/// contents between the first pair of `---` lines), or None if absent.
fn extract_frontmatter(doc: &str) -> Option<&str> {
    let rest = doc.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some(&rest[..end])
}

fn init_agent(registry: &Registry, config_dir: &str, agent_name: &str) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let skill_dir = PathBuf::from(&home).join(config_dir).join("skills/dbg");
    let adapters_dir = skill_dir.join("references/adapters");

    let adapters: Vec<(&str, &str)> = registry
        .all_backends()
        .iter()
        .flat_map(|b| b.adapters())
        .collect();

    let version_file = skill_dir.join(".version");

    #[cfg(not(debug_assertions))]
    if version_file.exists() {
        if let Ok(existing) = std::fs::read_to_string(&version_file) {
            if existing.trim() == VERSION {
                eprintln!("dbg v{VERSION} skill already up to date");
                match ensure_on_path()? {
                    Some(path) => eprintln!("installed: {}", path.display()),
                    None => eprintln!("dbg already on PATH"),
                }
                return Ok(());
            }
        }
    }

    // Remove stale .hash file from older versions
    let _ = std::fs::remove_file(skill_dir.join(".hash"));

    std::fs::create_dir_all(&adapters_dir)
        .context("failed to create skill directory")?;

    std::fs::write(skill_dir.join("SKILL.md"), SKILL_MD)?;

    for (filename, content) in &adapters {
        std::fs::write(adapters_dir.join(filename), content)?;
    }

    std::fs::write(&version_file, VERSION)?;

    eprintln!("dbg v{VERSION} skill installed: {}", skill_dir.display());

    match ensure_on_path()? {
        Some(path) => eprintln!("installed: {}", path.display()),
        None => eprintln!("dbg already on PATH"),
    }

    eprintln!("done — restart {agent_name} to activate");
    Ok(())
}

/// Ensure dbg is on PATH by copying to ~/.local/bin/
pub fn ensure_on_path() -> Result<Option<PathBuf>> {
    let bin_dir = dirs_home().join(".local/bin");
    let dest = bin_dir.join("dbg");

    let self_exe = std::env::current_exe()?;
    let self_canon = std::fs::canonicalize(&self_exe)?;

    // Already installed at dest and same binary?
    if dest.exists() {
        if let Ok(dest_canon) = std::fs::canonicalize(&dest) {
            if dest_canon == self_canon {
                return Ok(None);
            }
        }
    }

    // Already on PATH from somewhere else?
    if let Ok(found) = which::which("dbg") {
        if let Ok(found_canon) = std::fs::canonicalize(&found) {
            if found_canon == self_canon {
                return Ok(None);
            }
        }
    }

    // Create ~/.local/bin if needed
    std::fs::create_dir_all(&bin_dir)?;

    // Remove stale symlink or old copy
    if dest.exists() || dest.is_symlink() {
        std::fs::remove_file(&dest)?;
    }

    // Copy binary
    std::fs::copy(&self_exe, &dest)?;

    // Ensure executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(Some(dest))
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
