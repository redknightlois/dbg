use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::backend::Registry;

const SKILL_MD: &str = include_str!("../../skills/SKILL.md");

pub fn run_init(target: &str, registry: &Registry) -> Result<()> {
    match target {
        "claude" => init_agent(registry, ".claude", "Claude Code"),
        "codex" => init_agent(registry, ".codex", "Codex"),
        _ => bail!("unknown init target: {target} (use: claude, codex)"),
    }
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

    let hash = content_hash(SKILL_MD, &adapters);
    let hash_file = skill_dir.join(".hash");

    #[cfg(not(debug_assertions))]
    if hash_file.exists() {
        if let Ok(existing) = std::fs::read_to_string(&hash_file) {
            if existing.trim() == hash {
                eprintln!("skill already up to date");
                match ensure_on_path()? {
                    Some(path) => eprintln!("installed: {}", path.display()),
                    None => eprintln!("dbg already on PATH"),
                }
                return Ok(());
            }
        }
    }

    std::fs::create_dir_all(&adapters_dir)
        .context("failed to create skill directory")?;

    std::fs::write(skill_dir.join("SKILL.md"), SKILL_MD)?;

    for (filename, content) in &adapters {
        std::fs::write(adapters_dir.join(filename), content)?;
    }

    std::fs::write(&hash_file, &hash)?;

    eprintln!("skill installed: {}", skill_dir.display());

    match ensure_on_path()? {
        Some(path) => eprintln!("installed: {}", path.display()),
        None => eprintln!("dbg already on PATH"),
    }

    eprintln!("done — restart {agent_name} to activate");
    Ok(())
}

fn content_hash(skill: &str, adapters: &[(&str, &str)]) -> String {
    let mut h = DefaultHasher::new();
    skill.hash(&mut h);
    for (name, content) in adapters {
        name.hash(&mut h);
        content.hash(&mut h);
    }
    format!("{:08x}", h.finish() as u32)
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
