use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::ensure_on_path;
use crate::backend::Registry;

const SKILL_MD: &str = include_str!("../../skills/SKILL.md");

pub fn init(registry: &Registry) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let skill_dir = PathBuf::from(&home).join(".codex/skills/dbg");
    let adapters_dir = skill_dir.join("references/adapters");

    // Collect all adapter content from backends
    let adapters: Vec<(&str, &str)> = registry
        .all_backends()
        .iter()
        .flat_map(|b| b.adapters())
        .collect();

    let hash = content_hash(SKILL_MD, &adapters);
    let hash_file = skill_dir.join(".hash");

    // In release builds, skip if already installed with same content
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

    // Create directories
    std::fs::create_dir_all(&adapters_dir)
        .context("failed to create skill directory")?;

    // Write skill file
    std::fs::write(skill_dir.join("SKILL.md"), SKILL_MD)?;

    // Write all adapter files from registered backends
    for (filename, content) in &adapters {
        std::fs::write(adapters_dir.join(filename), content)?;
    }

    std::fs::write(&hash_file, &hash)?;

    eprintln!("skill installed: {}", skill_dir.display());

    // Ensure dbg is on PATH
    match ensure_on_path()? {
        Some(path) => eprintln!("installed: {}", path.display()),
        None => eprintln!("dbg already on PATH"),
    }

    eprintln!("done — restart Codex to activate");
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
