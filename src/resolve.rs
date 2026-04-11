use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Resolve a target for a given backend type.
/// Builds if needed, returns the path to the binary/script.
pub fn resolve(backend_type: &str, target: &str) -> Result<String> {
    match backend_type {
        "rust" | "c" | "cpp" | "zig" => resolve_native(target),
        "python" | "py" => resolve_python(target),
        "dotnet" | "csharp" | "fsharp" => resolve_dotnet(target),
        "go" => resolve_go(target),
        "java" | "kotlin" | "pprof" | "perf" | "callgrind" | "pyprofile" | "memcheck" | "valgrind" | "massif" | "dotnet-trace" => Ok(target.to_string()),
        _ => {
            // Unknown type — just check the file exists
            if Path::new(target).exists() {
                Ok(target.to_string())
            } else {
                bail!("file not found: {target}")
            }
        }
    }
}

fn resolve_native(target: &str) -> Result<String> {
    // Existing file
    if Path::new(target).is_file() {
        return Ok(target.to_string());
    }

    // target/debug/<name> (with hyphen-to-underscore)
    let underscore = target.replace('-', "_");
    for name in [target, underscore.as_str()] {
        let path = PathBuf::from("target/debug").join(name);
        if path.is_file() {
            return Ok(path.display().to_string());
        }
    }

    // Build it
    eprintln!("building {target}...");
    let status = Command::new("cargo")
        .args(["build", "-p", target])
        .status()
        .context("cargo not found")?;

    if !status.success() {
        bail!("cargo build -p {target} failed");
    }

    // Find the binary after build
    for name in [&underscore, target] {
        let path = PathBuf::from("target/debug").join(name);
        if path.is_file() {
            return Ok(path.display().to_string());
        }
    }

    bail!("cannot find binary for {target} after build")
}

fn resolve_python(target: &str) -> Result<String> {
    if Path::new(target).is_file() {
        Ok(target.to_string())
    } else {
        bail!("file not found: {target}")
    }
}

fn resolve_dotnet(target: &str) -> Result<String> {
    let path = Path::new(target);

    // Existing file — prefer apphost over DLL
    if path.is_file() {
        if target.ends_with(".dll") {
            let apphost = target.strip_suffix(".dll").unwrap();
            let apphost_path = Path::new(apphost);
            if apphost_path.is_file() {
                return Ok(apphost.to_string());
            }
        }
        return Ok(target.to_string());
    }

    // Directory with .csproj
    if path.is_dir() {
        let csproj = find_csproj(path)?;
        let name = csproj
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        eprintln!("building {name}...");
        let status = Command::new("dotnet")
            .args(["build", csproj.to_str().unwrap(), "-c", "Debug"])
            .status()
            .context("dotnet not found")?;

        if !status.success() {
            bail!("dotnet build failed");
        }

        // Find apphost or DLL
        return find_dotnet_output(path, &name);
    }

    bail!("cannot resolve: {target}")
}

fn find_csproj(dir: &Path) -> Result<PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "csproj") {
            return Ok(path);
        }
    }
    bail!("no .csproj found in {}", dir.display())
}

fn find_dotnet_output(dir: &Path, name: &str) -> Result<String> {
    let debug_dir = dir.join("bin/Debug");
    if !debug_dir.exists() {
        bail!("bin/Debug not found after build");
    }

    // Walk into framework subdirs (e.g., net10.0/)
    for entry in std::fs::read_dir(&debug_dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            // Prefer native apphost
            let apphost = entry.path().join(name);
            if apphost.is_file() {
                return Ok(apphost.display().to_string());
            }
            // Fall back to DLL
            let dll = entry.path().join(format!("{name}.dll"));
            if dll.is_file() {
                return Ok(dll.display().to_string());
            }
        }
    }
    bail!("cannot find {name} in {}", debug_dir.display())
}

fn resolve_go(target: &str) -> Result<String> {
    // Existing binary
    if Path::new(target).is_file() {
        return Ok(target.to_string());
    }

    // Directory — build it
    let dir = Path::new(target);
    if dir.is_dir() {
        eprintln!("building {target}...");
        let output_name = dir
            .file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("app");
        let output_path = dir.join(output_name);
        let status = Command::new("go")
            .args([
                "build",
                "-gcflags=all=-N -l",
                "-o",
                output_path.to_str().unwrap(),
                ".",
            ])
            .current_dir(dir)
            .status()
            .context("go not found")?;
        if !status.success() {
            bail!("go build failed");
        }
        return Ok(output_path.display().to_string());
    }

    bail!("not found: {target}")
}
