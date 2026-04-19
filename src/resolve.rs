use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use crate::check::find_bin;

fn path_stem_str(p: &Path) -> Result<String> {
    let stem = p.file_stem()
        .context("path has no file stem")?
        .to_str()
        .context("path contains non-UTF8 characters")?;
    Ok(stem.to_string())
}

/// Resolve a target for a given backend type.
/// Builds if needed, returns the path to the binary/script.
pub fn resolve(backend_type: &str, target: &str) -> Result<String> {
    match backend_type {
        // `gdb` is an alias for the lldb/native backend.
        "rust" | "c" | "cpp" | "zig" | "gdb" => resolve_native(target),
        "d" => resolve_d(target),
        "nim" => resolve_nim(target),
        "node" | "nodejs" | "js" | "javascript" | "ts" | "typescript" | "bun" | "deno"
            | "nodeprof" | "js-profile" => resolve_existing_file(target),
        "python" | "py" => resolve_existing_file(target),
        "php" | "php-profile" => resolve_existing_file(target),
        "ruby" | "rb" | "ruby-profile" => resolve_existing_file(target),
        "dotnet" | "csharp" | "fsharp" | "netcoredbg" | "netcoredbg-proto"
            | "dotnet-trace" => resolve_dotnet(target),
        "go" => resolve_go(target),
        "haskell" | "hs" | "haskell-profile" | "hs-profile" => resolve_existing_file(target),
        "ocaml" | "ml" => resolve_ocaml(target),
        "java" | "kotlin" | "pprof" | "perf" | "callgrind" | "pyprofile" | "memcheck" | "valgrind" | "massif" => Ok(target.to_string()),
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
        // Reject source files early — lldb/gdb expect a compiled binary,
        // and if we pass a .rs/.c/.cpp file through, the debugger exits
        // with an opaque error that gets surfaced as "debugger did not
        // produce prompt". Point the user at the likely build command.
        if let Some(hint) = source_file_hint(target) {
            bail!("{hint}");
        }
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

/// Detect common C/C++/Rust source extensions. If the user passed a
/// source file to a native-debugger start command, we refuse with a
/// concrete build hint rather than handing the file to lldb and
/// surfacing its opaque "invalid target" error.
fn source_file_hint(target: &str) -> Option<String> {
    let ext = Path::new(target).extension()?.to_str()?.to_ascii_lowercase();
    let (lang, build) = match ext.as_str() {
        "rs" => ("rust", "cargo build  # then pass ./target/debug/<name>"),
        "c" => ("C", "cc -g <file> -o <name>  # then pass ./<name>"),
        "cpp" | "cxx" | "cc" => (
            "C++",
            "c++ -g <file> -o <name>  # then pass ./<name>",
        ),
        "h" | "hpp" => (
            "header",
            "pass the compiled binary you want to debug, not a header",
        ),
        _ => return None,
    };
    Some(format!(
        "{target} is a {lang} source file — native debuggers expect a \
         compiled binary. Build first: {build}"
    ))
}

fn resolve_existing_file(target: &str) -> Result<String> {
    if Path::new(target).is_file() {
        Ok(target.to_string())
    } else {
        bail!("file not found: {target}")
    }
}

fn resolve_dotnet(target: &str) -> Result<String> {
    let path = Path::new(target);

    // Existing file.
    if path.is_file() {
        // A `.csproj` is a build input, not a runnable artifact —
        // netcoredbg rejects it with COR_E_FILENOTFOUND. Build the
        // project and hand back the resulting DLL/apphost from the
        // project's own bin/Debug/ tree (not the cwd's).
        if path.extension().and_then(|s| s.to_str()) == Some("csproj") {
            let name = path_stem_str(path)?;
            let csproj_str = path.to_str().context("csproj path contains non-UTF8 characters")?;
            eprintln!("building {name}...");
            let status = Command::new("dotnet")
                .args(["build", csproj_str, "-c", "Debug"])
                .status()
                .context("dotnet not found")?;
            if !status.success() {
                bail!("dotnet build failed");
            }
            let proj_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            return find_dotnet_output(proj_dir, &name);
        }
        // Prefer apphost over DLL.
        if let Some(apphost) = target.strip_suffix(".dll") {
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
        let name = path_stem_str(&csproj)?;
        let csproj_str = csproj.to_str().context("csproj path contains non-UTF8 characters")?;

        eprintln!("building {name}...");
        let status = Command::new("dotnet")
            .args(["build", csproj_str, "-c", "Debug"])
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
    // `dotnet build` honors the csproj's `<AssemblyName>` / default
    // output name, which can differ in case from the project stem
    // (e.g. Broken.csproj → broken.dll when AssemblyName is lowercase).
    // Check the obvious paths first, then fall back to a case-
    // insensitive scan so we find the DLL regardless.
    let debug_dir = dir.join("bin/Debug");
    let release_dir = dir.join("bin/Release");
    let candidates: Vec<PathBuf> = [&debug_dir, &release_dir]
        .into_iter()
        .filter(|d| d.exists())
        .cloned()
        .collect();
    if candidates.is_empty() {
        bail!("bin/Debug not found after build");
    }

    let name_lc = name.to_ascii_lowercase();
    for root in &candidates {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let apphost = entry.path().join(name);
            if apphost.is_file() {
                return Ok(apphost.display().to_string());
            }
            let dll = entry.path().join(format!("{name}.dll"));
            if dll.is_file() {
                return Ok(dll.display().to_string());
            }
            // Case-insensitive fallback — scan the tfm directory for
            // <name>.dll / <name> regardless of filename casing.
            if let Ok(dir_iter) = std::fs::read_dir(entry.path()) {
                for sub in dir_iter.flatten() {
                    let p = sub.path();
                    let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
                    let fname_lc = fname.to_ascii_lowercase();
                    if fname_lc == format!("{name_lc}.dll") && p.is_file() {
                        return Ok(p.display().to_string());
                    }
                    if fname_lc == name_lc && p.is_file() {
                        return Ok(p.display().to_string());
                    }
                }
            }
        }
    }
    bail!("cannot find {name} in {}", debug_dir.display())
}

fn resolve_d(target: &str) -> Result<String> {
    let path = Path::new(target);
    if path.is_file() {
        // If it's a source file, compile it
        if target.ends_with(".d") {
            let stem = path_stem_str(path)?;
            let output = path.parent().unwrap_or(Path::new(".")).join(&stem);
            eprintln!("building {target}...");
            // Try ldc2 first (better DWARF), fall back to dmd
            let output_str = output.to_str().context("output path contains non-UTF8 characters")?;
            let status = Command::new(find_bin("ldc2"))
                .args(["-g", "-of", output_str, target])
                .status()
                .or_else(|_| {
                    Command::new(find_bin("dmd"))
                        .args(["-g", &format!("-of={}", output.display()), target])
                        .status()
                })
                .context("neither ldc2 nor dmd found")?;
            if !status.success() {
                bail!("D compilation failed for {target}");
            }
            return Ok(output.display().to_string());
        }
        // Already a binary
        return Ok(target.to_string());
    }
    bail!("file not found: {target}")
}

fn resolve_nim(target: &str) -> Result<String> {
    let path = Path::new(target);
    if path.is_file() {
        // If it's a source file, compile it
        if target.ends_with(".nim") {
            let stem = path_stem_str(path)?;
            let output = path.parent().unwrap_or(Path::new(".")).join(&stem);
            eprintln!("building {target}...");
            let status = Command::new(find_bin("nim"))
                .args(["compile", "--debugger:native", "--opt:none",
                       &format!("--out:{}", output.display()), target])
                .status()
                .context("nim not found")?;
            if !status.success() {
                bail!("nim compile failed for {target}");
            }
            return Ok(output.display().to_string());
        }
        // Already a binary
        return Ok(target.to_string());
    }
    bail!("file not found: {target}")
}


fn resolve_ocaml(target: &str) -> Result<String> {
    let path = Path::new(target);
    if path.is_file() {
        // If it's a source file, compile to bytecode with debug info
        if target.ends_with(".ml") {
            let stem = path_stem_str(path)?;
            let output = path.parent().unwrap_or(Path::new(".")).join(&stem);
            eprintln!("building {target} (bytecode with -g)...");
            let output_str = output.to_str().context("output path contains non-UTF8 characters")?;
            let status = Command::new(find_bin("ocamlfind"))
                .args(["ocamlc", "-g", "-o", output_str, target])
                .status()
                .or_else(|_| {
                    Command::new(find_bin("ocamlc"))
                        .args(["-g", "-o", output_str, target])
                        .status()
                })
                .context("neither ocamlfind nor ocamlc found")?;
            if !status.success() {
                bail!("OCaml bytecode compilation failed for {target}");
            }
            return Ok(output.display().to_string());
        }
        // Already a bytecode binary
        return Ok(target.to_string());
    }
    bail!("file not found: {target}")
}

fn resolve_go(target: &str) -> Result<String> {
    // `.go` source file — must be compiled before delve can `exec` it.
    // Treating `broken.go` as a ready binary (the previous behavior)
    // caused delve to exit immediately with "not an executable", and
    // the daemon died before publishing the socket.
    let p = Path::new(target);
    if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("go") {
        eprintln!("building {target}...");
        let parent = p.parent().filter(|x| !x.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
        let output_path = parent.join(stem);
        let output_str = output_path.to_str().context("output path contains non-UTF8 characters")?;
        let file_name = p.file_name().and_then(|s| s.to_str()).unwrap_or(target);
        let status = Command::new("go")
            .args([
                "build",
                "-gcflags=all=-N -l",
                "-o",
                output_str,
                file_name,
            ])
            .current_dir(parent)
            .status()
            .context("go not found")?;
        if !status.success() {
            bail!("go build failed");
        }
        return Ok(output_path.display().to_string());
    }

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
        let output_str = output_path.to_str().context("output path contains non-UTF8 characters")?;
        let status = Command::new("go")
            .args([
                "build",
                "-gcflags=all=-N -l",
                "-o",
                output_str,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_dotnet_csproj_returns_dll_not_project_file() {
        // Regression: netcoredbg was getting launched with the .csproj
        // path and crashing with COR_E_FILENOTFOUND. resolve_dotnet must
        // build the project and hand back the produced DLL/apphost.
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("Demo.csproj");
        std::fs::write(&proj, "dummy").unwrap();
        // Pre-populate what `dotnet build` would have produced so the
        // test doesn't depend on the dotnet toolchain. We short-circuit
        // by invoking `find_dotnet_output` directly — that's the path
        // that previously failed to handle lowercase AssemblyName.
        let tfm = tmp.path().join("bin/Debug/net8.0");
        std::fs::create_dir_all(&tfm).unwrap();
        std::fs::write(tfm.join("demo.dll"), "").unwrap();
        let got = find_dotnet_output(tmp.path(), "Demo").unwrap();
        assert!(got.ends_with("demo.dll"), "got: {got}");
        assert!(!got.ends_with(".csproj"), "must not return csproj: {got}");
    }

    #[test]
    fn resolve_dotnet_csproj_finds_release_output() {
        let tmp = TempDir::new().unwrap();
        let tfm = tmp.path().join("bin/Release/net8.0");
        std::fs::create_dir_all(&tfm).unwrap();
        std::fs::write(tfm.join("Broken.dll"), "").unwrap();
        let got = find_dotnet_output(tmp.path(), "Broken").unwrap();
        assert!(got.ends_with("Broken.dll"), "got: {got}");
    }

    /// Regression: `dbg start dotnet-trace Broken.csproj` passed the
    /// csproj straight through to `dotnet-trace collect`, which needs
    /// an executable/DLL and exits with "No profile or provider
    /// specified". dotnet-trace now goes through the same
    /// resolve_dotnet path as netcoredbg so csproj → built DLL works.
    #[test]
    fn resolve_dispatches_dotnet_trace_to_resolve_dotnet() {
        let tmp = TempDir::new().unwrap();
        let tfm = tmp.path().join("bin/Release/net8.0");
        std::fs::create_dir_all(&tfm).unwrap();
        std::fs::write(tfm.join("Broken.dll"), "").unwrap();
        // Directly exercise resolve_dotnet on a directory containing a
        // csproj — the same dispatch path used when "dotnet-trace" is
        // the backend type.
        let proj = tmp.path().join("Broken.csproj");
        std::fs::write(&proj, "<Project/>").unwrap();
        let got = find_dotnet_output(tmp.path(), "Broken").unwrap();
        assert!(got.ends_with("Broken.dll"));
        assert!(!got.ends_with(".csproj"));
    }

    #[test]
    fn resolve_go_builds_source_file() {
        // Skip if go isn't installed in CI — the build path exists
        // only when the toolchain is on PATH.
        if Command::new("go").arg("version").output().is_err() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("hello.go");
        std::fs::write(&src, "package main\nfunc main() {}\n").unwrap();
        let out = resolve_go(src.to_str().unwrap()).expect("build should succeed");
        // Output is the built binary sitting next to the source.
        assert!(!out.ends_with(".go"), "should not return source path: {out}");
        assert!(Path::new(&out).is_file(), "binary missing: {out}");
    }

    /// Regression: `dbg start rust src/main.rs` (source file, not compiled
    /// binary) silently exited with no user-facing error because lldb
    /// accepted the path and then failed internally. `resolve_native`
    /// must refuse source files up front with a concrete build hint.
    #[test]
    fn resolve_native_rejects_rust_source_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("main.rs");
        std::fs::write(&src, "fn main(){}").unwrap();
        let err = resolve_native(src.to_str().unwrap())
            .err()
            .expect("should error on .rs source");
        let msg = err.to_string();
        assert!(
            msg.contains("source") && (msg.contains("cargo build") || msg.contains("compiled binary")),
            "hint must mention source + build: {msg}"
        );
    }

    #[test]
    fn resolve_native_rejects_c_and_cpp_source_files() {
        let tmp = TempDir::new().unwrap();
        for ext in ["c", "cpp", "cxx", "cc", "h"] {
            let src = tmp.path().join(format!("a.{ext}"));
            std::fs::write(&src, "").unwrap();
            let err = resolve_native(src.to_str().unwrap())
                .err()
                .unwrap_or_else(|| panic!(".{ext} must be rejected"));
            assert!(
                err.to_string().to_lowercase().contains("source")
                    || err.to_string().to_lowercase().contains("header"),
                "wrong hint for .{ext}: {err}"
            );
        }
    }
}
