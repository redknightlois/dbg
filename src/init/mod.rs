pub mod claude;
pub mod codex;

use std::path::PathBuf;

use anyhow::{Result, bail};

pub fn run_init(target: &str, registry: &crate::backend::Registry) -> Result<()> {
    match target {
        "claude" => claude::init(registry),
        "codex" => codex::init(registry),
        _ => bail!("unknown init target: {target} (use: claude, codex)"),
    }
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
        .unwrap_or_else(|_| PathBuf::from("~"))
}
