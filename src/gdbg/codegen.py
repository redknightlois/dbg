"""gdbg codegen: offline CUDA builds, loaded-module capture, structured diffs and a reference archive.

Embedded in the gdbg binary and run as `python3 -I -c`. Stdlib only. Python inputs are read with ast;
candidate code is never imported or executed.
"""
import argparse
import ast
import collections
import datetime
import difflib
import hashlib
import json
import os
import re
import shlex
import shutil
import sqlite3
import struct
import subprocess
import sys
import textwrap

SCHEMA = "gdbg.codegen/1"
INVENTORY_SCHEMA = "gdbg.codegen.inventory/1"
LOAD_INLINE = "torch.utils.cpp_extension.load_inline"
# load_inline arguments that change the CUDA translation unit; they must resolve statically.
TU_FIELDS = ("name", "cuda_sources", "extra_cuda_cflags", "extra_include_paths", "no_implicit_headers")
TORCH_IMPLICIT_HEADERS = ["#include <torch/types.h>", "#include <cuda.h>", "#include <cuda_runtime.h>"]
CAUTION = ("Equal instruction counts or resources do not establish equivalent code or performance; "
           "static differences are hypotheses to measure.")
NO_CAPTURED_PTX = "the driver hands CUPTI the loaded cubin only; no PTX was captured and none is reconstructed"
ROLES = ("original", "accepted", "experimental")
ARCH_FLAGS = ("-arch", "--gpu-architecture", "-gencode", "--generate-code", "-code", "--gpu-code")
CONTROLLED_FLAGS = {"-o", "--output-file", "-c", "--compile", "-ptx", "--ptx", "-cubin", "--cubin", "-fatbin",
                    "--fatbin", "-dc", "--device-c", "-dw", "--device-w", "-E", "--preprocess", "-lib", "--lib",
                    "-link", "--link", "-shared", "--shared", "-M", "-MD", "-MF", "-keep", "--keep", "-keep-dir",
                    "--keep-dir"}


class Failure(Exception):
    pass


class Unsupported(Exception):
    pass


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(argv, **kwargs):
    return subprocess.run([str(a) for a in argv], capture_output=True, text=True, errors="replace", **kwargs)


def read_text(path):
    with open(path, errors="replace") as f:
        return f.read()


def write_text(path, text):
    with open(path, "w") as f:
        f.write(text)


def write_json(path, value):
    write_text(path, json.dumps(value, indent=2, sort_keys=True) + "\n")


def read_json(path):
    with open(path) as f:
        return json.load(f)


def new_dir(path):
    try:
        os.makedirs(path)
    except FileExistsError:
        raise Failure(f"refusing to overwrite existing output directory {path}")
    return os.path.abspath(path)


def artifact_table(out, names):
    return {n: {"sha256": sha256_file(os.path.join(out, n)), "bytes": os.path.getsize(os.path.join(out, n))}
            for n in names if os.path.isfile(os.path.join(out, n))}


# ---------------------------------------------------------------------------
# Tools and provenance
# ---------------------------------------------------------------------------

def find_tools(nvcc=None):
    home = os.environ.get("CUDA_HOME") or os.environ.get("CUDA_PATH")
    path = shutil.which(nvcc or (home and os.path.join(home, "bin", "nvcc")) or "nvcc")
    if path is None:
        raise Failure("nvcc not found; pass --nvcc or set CUDA_HOME")
    tools = {"nvcc": os.path.abspath(path)}
    for name in ("ptxas", "nvdisasm", "cuobjdump", "cu++filt"):
        sibling = os.path.join(os.path.dirname(tools["nvcc"]), name)
        tools[name] = sibling if os.access(sibling, os.X_OK) else shutil.which(name)
    missing = [n for n in ("ptxas", "nvdisasm", "cuobjdump") if tools[n] is None]
    if missing:
        raise Failure(f"missing CUDA tools next to {tools['nvcc']} or on PATH: {', '.join(missing)}")
    return tools


def tool_versions(tools):
    versions = {}
    for name, path in tools.items():
        if path and name != "cu++filt":
            lines = run([path, "--version"]).stdout.strip().splitlines()
            versions[name] = {"path": path, "version": lines[-1] if lines else None}
    return versions


def git_state(paths):
    repos = collections.defaultdict(list)
    for path in paths:
        directory = path if os.path.isdir(path) else os.path.dirname(path)
        top = run(["git", "-C", directory, "rev-parse", "--show-toplevel"])
        if top.returncode == 0:
            repos[top.stdout.strip()].append(path)
    state = []
    for top, members in sorted(repos.items()):
        head = run(["git", "-C", top, "rev-parse", "HEAD"]).stdout.strip() or None
        status = run(["git", "-C", top, "status", "--porcelain", "--", *members]).stdout
        state.append({"repo": top, "head": head, "dirty": sorted(line[3:] for line in status.splitlines())})
    return state


def cubin_arch(tools, cubin):
    match = re.search(r"\.(sm_\d+[a-z]?)\.cubin", run([tools["cuobjdump"], "-lelf", cubin]).stdout)
    return match.group(1) if match else None


def elf_functions(path):
    """Symbol-table index of every STT_FUNC symbol in a 64-bit little-endian cubin (nvdisasm -fun takes indices)."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        return {}
    shoff, = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    headers = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)]
    functions = {}
    for _, kind, _, _, offset, size, link, _, _, entsize in headers:
        if kind != 2:  # SHT_SYMTAB
            continue
        names = data[headers[link][4]:headers[link][4] + headers[link][5]]
        for index in range(size // (entsize or 24)):
            name, info, _, _, _, _ = struct.unpack_from("<IBBHQQ", data, offset + index * (entsize or 24))
            if info & 0xF == 2:  # STT_FUNC
                functions[names[name:names.index(b"\0", name)].decode(errors="replace")] = index
    return functions


def demangle(tools, names):
    tool = tools.get("cu++filt") or shutil.which("c++filt")
    if not tool or not names:
        return {}
    lines = subprocess.run([tool], input="\n".join(names) + "\n", capture_output=True, text=True).stdout.splitlines()
    return dict(zip(names, lines)) if len(lines) == len(names) else {}


def parse_res_usage(text):
    usage, name = {}, None
    for line in text.splitlines():
        match = re.match(r"\s*(Function (.+)|Common):$", line)
        if match:
            name = match.group(2) or ""
        elif name is not None and ":" in line:
            usage[name] = {k.lower(): int(v) for k, v in re.findall(r"(\w+(?:\[\d+\])?):(\d+)", line)}
            name = None
    return usage


def parse_ptxas(text):
    """Per-function stack frame and spill bytes from `ptxas -v`."""
    spills, current = {}, None
    for line in text.splitlines():
        match = re.search(r"Function properties for (\S+)", line)
        if match:
            current = match.group(1)
            continue
        match = re.search(r"(\d+) bytes stack frame, (\d+) bytes spill stores, (\d+) bytes spill loads", line)
        if match and current:
            spills[current] = dict(zip(("stack_frame", "spill_stores", "spill_loads"), map(int, match.groups())))
            current = None
    return spills


def function_table(sass_text, usage, spills):
    table = {}
    for name, function in parse_sass(sass_text)["functions"].items():
        resources = dict(usage.get(name, {}))
        resources.update(spills.get(name.rsplit("$", 1)[-1], {}))
        table[name] = {"entry": function["entry"], "resources": resources}
    return table


# ---------------------------------------------------------------------------
# Static load_inline extraction
# ---------------------------------------------------------------------------

class StaticEval:
    """Evaluates the literal subset of Python used to assemble CUDA sources and flags, without running the file."""
    FUNCTIONS = {"str": str, "int": int, "textwrap.dedent": textwrap.dedent}
    METHODS = {"format", "replace", "strip", "lstrip", "rstrip", "join", "upper", "lower", "split", "splitlines"}
    LIMIT = 1 << 26

    def __init__(self, tree):
        self.parent = {child: node for node in ast.walk(tree) for child in ast.iter_child_nodes(node)}
        self.bindings = collections.defaultdict(list)
        for node in ast.walk(tree):
            self.collect(node)

    def scope_of(self, node):
        node = self.parent.get(node)
        while node is not None and not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda)):
            node = self.parent.get(node)
        return node

    def bind(self, node, name, value):
        self.bindings[(self.scope_of(node), name)].append(value)

    def collect(self, node):
        """Records every binding; anything but `name = expr` or an import is a non-literal (None) binding."""
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    self.bind(node, target.id, node.value)
                else:
                    for name in ast.walk(target):
                        if isinstance(name, ast.Name):
                            self.bind(node, name.id, None)
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name) and node.value is not None:
            self.bind(node, node.target.id, node.value)
        elif isinstance(node, (ast.AugAssign, ast.For, ast.AsyncFor, ast.comprehension, ast.withitem, ast.NamedExpr)):
            target = getattr(node, "target", None) or getattr(node, "optional_vars", None)
            for name in ast.walk(target) if target is not None else ():
                if isinstance(name, ast.Name):
                    self.bind(node, name.id, None)
        elif isinstance(node, ast.Import):
            for alias in node.names:
                if alias.asname:
                    self.bind(node, alias.asname, ("import", alias.name))
                else:
                    self.bind(node, alias.name.split(".")[0], ("import", alias.name.split(".")[0]))
        elif isinstance(node, ast.ImportFrom) and node.level == 0:
            for alias in node.names:
                self.bind(node, alias.asname or alias.name, ("import", f"{node.module}.{alias.name}"))
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            self.bind(node, node.name, None)
        elif isinstance(node, ast.arg):
            self.bindings[(self.scope_of(node), node.arg)].append(None)
        elif isinstance(node, ast.ExceptHandler) and node.name:
            self.bind(node, node.name, None)

    def lookup(self, name, node):
        scope = self.scope_of(node)
        while True:
            found = self.bindings.get((scope, name))
            if found is not None or scope is None:
                return found
            scope = self.scope_of(scope)

    def dotted(self, node):
        if isinstance(node, ast.Attribute):
            base = self.dotted(node.value)
            return base and f"{base}.{node.attr}"
        if not isinstance(node, ast.Name):
            return None
        found = self.lookup(node.id, node)
        if found is None:
            return node.id
        if len(found) != 1 or found[0] is None:
            return None
        if isinstance(found[0], tuple):
            return found[0][1]
        return self.dotted(found[0]) if isinstance(found[0], (ast.Name, ast.Attribute)) else None

    def eval(self, node):
        if isinstance(node, ast.Constant):
            return node.value
        if isinstance(node, (ast.List, ast.Tuple)):
            if any(isinstance(e, ast.Starred) for e in node.elts):
                raise Unsupported("starred element")
            return [self.eval(e) for e in node.elts]
        if isinstance(node, ast.Dict):
            if any(k is None for k in node.keys):
                raise Unsupported("dict unpacking")
            return {self.eval(k): self.eval(v) for k, v in zip(node.keys, node.values)}
        if isinstance(node, ast.JoinedStr):
            parts = []
            for value in node.values:
                if isinstance(value, ast.Constant):
                    parts.append(value.value)
                    continue
                item = {-1: lambda x: x, 115: str, 114: repr, 97: ascii}[value.conversion](self.eval(value.value))
                parts.append(format(item, self.eval(value.format_spec) if value.format_spec else ""))
            return "".join(parts)
        if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Mod, ast.Mult)):
            left, right = self.eval(node.left), self.eval(node.right)
            if isinstance(node.op, ast.Mult) and isinstance(right, int) and len(left) * right > self.LIMIT:
                raise Unsupported("expression exceeds 64 MiB")
            if isinstance(node.op, ast.Mod):
                return left % (tuple(right) if isinstance(right, list) else right)
            return left + right if isinstance(node.op, ast.Add) else left * right
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
            return -self.eval(node.operand)
        if isinstance(node, ast.Name):
            found = self.lookup(node.id, node)
            if not found:
                raise Unsupported(f"`{node.id}` is not assigned in this file")
            if len(found) > 1:
                raise Unsupported(f"`{node.id}` is bound {len(found)} times")
            if found[0] is None or isinstance(found[0], tuple):
                raise Unsupported(f"`{node.id}` is not bound to a literal expression")
            return self.eval(found[0])
        if isinstance(node, ast.Call):
            if any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords):
                raise Unsupported("argument unpacking")
            args = [self.eval(a) for a in node.args]
            kwargs = {k.arg: self.eval(k.value) for k in node.keywords}
            name = self.dotted(node.func)
            if name in self.FUNCTIONS:
                return self.FUNCTIONS[name](*args, **kwargs)
            if isinstance(node.func, ast.Attribute) and node.func.attr in self.METHODS:
                target = self.eval(node.func.value)
                if isinstance(target, str):
                    return getattr(target, node.func.attr)(*args, **kwargs)
            raise Unsupported(f"call to `{ast.unparse(node.func)}`")
        raise Unsupported(f"{type(node).__name__} expression")


def extract_inline(path, selector, params):
    """Statically resolves one load_inline call. `params` is the installed load_inline signature."""
    source = read_text(path)
    tree = ast.parse(source, path)
    evaluator = StaticEval(tree)
    calls = sorted((n for n in ast.walk(tree) if isinstance(n, ast.Call) and evaluator.dotted(n.func) == LOAD_INLINE),
                   key=lambda n: (n.lineno, n.col_offset))
    if not calls:
        raise Failure(f"{path}: no {LOAD_INLINE} call found")
    described = []
    for call in calls:
        bound, problem = {}, None
        for index, arg in enumerate(call.args):
            if isinstance(arg, ast.Starred) or index >= len(params):
                problem = "positional argument unpacking"
                break
            bound[params[index]] = arg
        for keyword in call.keywords:
            if keyword.arg is None:
                problem = "**kwargs"
            else:
                bound[keyword.arg] = keyword.value
        try:
            name = evaluator.eval(bound["name"]) if "name" in bound else None
        except (Unsupported, RecursionError):
            name = None
        described.append((call, bound, problem, name))
    if selector and selector.startswith("@"):
        chosen = [d for d in described if str(d[0].lineno) == selector[1:]]
    elif selector:
        chosen = [d for d in described if d[3] == selector]
    else:
        chosen = described
    if len(chosen) != 1:
        listing = ", ".join(f"name={d[3]!r} at line {d[0].lineno}" for d in described)
        found = f"selector {selector!r} matches {len(chosen)}" if selector else f"found {len(chosen)}"
        raise Failure(f"{path}: {found} load_inline calls; select one with PATH::NAME or PATH::@LINE ({listing})")
    call, bound, problem, _ = chosen[0]
    if problem:
        raise Failure(f"{path}:{call.lineno}: unsupported dynamic load_inline configuration: {problem}")
    values, unresolved = {}, {}
    for field, node in bound.items():
        try:
            values[field] = evaluator.eval(node)
        except (Unsupported, RecursionError, TypeError, ValueError, KeyError, IndexError) as error:
            segment = ast.get_source_segment(source, node) or ast.unparse(node)
            if field in TU_FIELDS:
                raise Failure(f"{path}:{node.lineno}: load_inline `{field}` is not statically resolvable "
                              f"(`{segment}`): {error}")
            unresolved[field] = f"`{segment}`: {error}"
    if not values.get("cuda_sources"):
        raise Failure(f"{path}:{call.lineno}: the selected load_inline call has no cuda_sources")
    return {"line": call.lineno, "values": values, "unresolved": unresolved}


TORCH_QUERY = r"""
import inspect, json, sysconfig, torch, torch.utils.cpp_extension as ext
try:
    includes = ext.include_paths(device_type="cuda")
except TypeError:
    includes = ext.include_paths(cuda=True)
print(json.dumps({"torch": torch.__version__, "torch_cuda": torch.version.cuda, "cuda_home": ext.CUDA_HOME,
    "include_paths": includes, "python_include": sysconfig.get_path("include", scheme="posix_prefix"),
    "common_nvcc_flags": ext.COMMON_NVCC_FLAGS, "load_inline_params": list(inspect.signature(ext.load_inline).parameters)}))
"""


def torch_query(python, cwd):
    """Imports torch (never the candidate) from an empty working directory."""
    proc = run([python, "-c", TORCH_QUERY], cwd=cwd)
    if proc.returncode:
        raise Failure(f"cannot query torch with {python}; pass --torch-python:\n{proc.stderr.strip()[-2000:]}")
    return json.loads(proc.stdout.strip().splitlines()[-1])


def torch_cuda_flags(info, name, extra_cuda_cflags, extra_include_paths):
    """nvcc flags torch's Linux JIT builder gives a load_inline cuda.cu, before its own arch flags."""
    flags = [f"-DTORCH_EXTENSION_NAME={name}", "-DTORCH_API_INCLUDE_EXTENSION_H"]
    flags += [f"-I{os.path.abspath(p.strip())}" for p in extra_include_paths]
    for path in info["include_paths"] + [info["python_include"]] * bool(info["python_include"]):
        flags += ["-isystem", path]
    flags += info["common_nvcc_flags"] + ["--compiler-options", "-fPIC"] + [f.strip() for f in extra_cuda_cflags]
    if not any(f.startswith("-std=") for f in flags):
        flags.append("-std=c++17")
    if os.environ.get("CC"):
        flags = ["-ccbin", os.environ["CC"]] + flags
    return flags


def split_flags(flags):
    """Drops architecture flags (the explicit target wins) and rejects flags that change the build gdbg drives."""
    kept, removed, index = [], [], 0
    while index < len(flags):
        flag = flags[index]
        base = flag.split("=", 1)[0]
        if base in ARCH_FLAGS:
            take = 1 if "=" in flag else 2
            removed += flags[index:index + take]
            index += take
            continue
        if base in CONTROLLED_FLAGS:
            raise Failure(f"nvcc flag {flag} conflicts with the build gdbg codegen drives")
        kept.append(flag)
        index += 1
    return kept, removed


def include_dirs(flags):
    dirs = []
    for index, flag in enumerate(flags):
        if flag == "-I" and index + 1 < len(flags):
            dirs.append(flags[index + 1])
        elif flag.startswith("-I") and len(flag) > 2:
            dirs.append(flag[2:])
    return [os.path.abspath(d) for d in dirs]


def dependencies(deps_file, user_roots, out, tu):
    """Hashes every dependency nvcc reports; copies the ones under user include roots into source/deps."""
    text = read_text(deps_file).replace("\\\n", " ")
    body = text.split(" : ", 1)[1] if " : " in text else text.split(":", 1)[-1]
    records = []
    for token in re.findall(r"(?:\\ |\S)+", body):
        path = os.path.realpath(token.replace("\\ ", " "))
        if path == os.path.realpath(tu) or not os.path.isfile(path):
            continue
        record = {"path": path, "sha256": sha256_file(path), "snapshot": None}
        if any(path.startswith(os.path.realpath(root) + os.sep) for root in user_roots):
            record["snapshot"] = os.path.join("source", "deps", path.lstrip(os.sep))
            os.makedirs(os.path.dirname(os.path.join(out, record["snapshot"])), exist_ok=True)
            shutil.copyfile(path, os.path.join(out, record["snapshot"]))
        records.append(record)
    return records


# ---------------------------------------------------------------------------
# Offline build
# ---------------------------------------------------------------------------

def build(spec, args, out):
    """Builds one input into `out` (which must not exist). The manifest and diagnostics stay on failure."""
    out = new_dir(out)
    manifest = {"schema": SCHEMA, "kind": "compile", "status": "failed", "created": now(), "input": {"spec": spec}}
    try:
        build_into(spec, args, out, manifest)
        manifest["status"] = "ok"
    except Failure as error:
        manifest["error"] = str(error)
        raise
    finally:
        write_json(os.path.join(out, "manifest.json"), manifest)
    return manifest


def build_into(spec, args, out, manifest):
    if not args.arch:
        raise Failure("--arch is required to build a source input (for example --arch sm_90)")
    if not re.fullmatch(r"sm_\d+[a-z]?", args.arch):
        raise Failure(f"--arch must name a SASS target such as sm_90, not {args.arch}")
    ptx_arch = args.ptx_arch or "compute_" + args.arch[3:]
    manifest["target"] = {"arch": args.arch, "ptx_arch": ptx_arch, "source": "explicit --arch"}
    path, _, selector = spec.partition("::")
    path = os.path.abspath(path)
    if not os.path.isfile(path):
        raise Failure(f"input not found: {path}")
    nvcc = args.nvcc
    source_dir = os.path.join(out, "source")
    os.makedirs(source_dir)
    manifest["input"].update(path=path, sha256=sha256_file(path))
    if path.endswith(".py"):
        info = torch_query(args.torch_python or sys.executable, out)
        # torch's JIT compiles with its CUDA_HOME's nvcc.
        nvcc = nvcc or (info["cuda_home"] and os.path.join(info["cuda_home"], "bin", "nvcc"))
        call = extract_inline(path, selector, info["load_inline_params"])
        values = call["values"]
        sources = values["cuda_sources"]
        sources = [sources] if isinstance(sources, str) else list(sources)
        if not values.get("no_implicit_headers"):
            sources = TORCH_IMPLICIT_HEADERS + sources
        text, tu_name = "\n".join(sources), "cuda.cu"
        includes = values.get("extra_include_paths") or []
        user_flags = torch_cuda_flags(info, values["name"], values.get("extra_cuda_cflags") or [], includes)
        user_roots = [os.path.abspath(p) for p in includes]
        manifest["input"].update(kind="inline", selector=selector or None, line=call["line"], unresolved=call["unresolved"],
                                 torch={k: info[k] for k in ("torch", "torch_cuda", "cuda_home")},
                                 load_inline={k: v for k, v in values.items() if k not in ("cuda_sources", "cpp_sources")},
                                 implicit_headers=not values.get("no_implicit_headers"))
    else:
        if selector:
            raise Failure("a ::selector applies to Python load_inline inputs only")
        text, tu_name = read_text(path), os.path.basename(path)
        # The snapshot compiles from source/, so the original directory stays first on the quote-include path.
        user_flags = [f"-I{os.path.dirname(path)}"]
        user_roots = [os.path.dirname(path)]
        manifest["input"]["kind"] = "cuda"
    tools = find_tools(nvcc)
    manifest["tools"] = tool_versions(tools)
    flags, removed = split_flags(user_flags + args.rest)
    user_roots += include_dirs(args.rest)
    if args.instantiate:
        text += "\n" + "\n".join(args.instantiate) + "\n"
    manifest["specialization"] = {"instantiate": args.instantiate, "params": dict(args.param),
                                  "defines": [f for f in flags if f.startswith("-D")]}
    tu = os.path.join(source_dir, tu_name)
    write_text(tu, text)

    keep = os.path.join(out, "build")
    os.makedirs(keep)
    argv = [tools["nvcc"], *flags, "-gencode", f"arch={ptx_arch},code={args.arch}", "-cubin", "-o",
            os.path.join(out, "module.cubin"), "--keep", "--keep-dir", keep, "-Xptxas", "-v", "-MD", "-MF",
            os.path.join(out, "deps.d"), tu]
    proc = run(argv)
    write_text(os.path.join(out, "compiler.log"), f"$ {shlex.join(argv)}\n{proc.stdout}{proc.stderr}")
    manifest["compiler"] = {"argv": argv, "returncode": proc.returncode, "removed_arch_flags": removed}
    if proc.returncode:
        hint = f"; -ccbin {os.environ['CC']} comes from $CC, as in torch's JIT" if "-ccbin" in flags else ""
        raise Failure(f"nvcc exited with {proc.returncode}; see {os.path.join(out, 'compiler.log')}{hint}")
    ptx = [f for f in os.listdir(keep) if f.endswith(".ptx")]
    if len(ptx) != 1:
        raise Failure(f"expected one PTX intermediate in {keep}, found {ptx}")
    shutil.copyfile(os.path.join(keep, ptx[0]), os.path.join(out, "module.ptx"))
    shutil.rmtree(keep)

    cubin = os.path.join(out, "module.cubin")
    sass = run([tools["nvdisasm"], "-g", cubin])
    if sass.returncode:
        raise Failure(f"nvdisasm failed: {sass.stderr.strip()}")
    write_text(os.path.join(out, "module.sass"), sass.stdout)
    usage = run([tools["cuobjdump"], "-res-usage", cubin]).stdout
    write_text(os.path.join(out, "resources.txt"), usage)
    manifest["target"]["cubin_arch"] = cubin_arch(tools, cubin)
    if manifest["target"]["cubin_arch"] != args.arch:
        raise Failure(f"nvcc produced {manifest['target']['cubin_arch']} instead of {args.arch}")
    manifest["ptx"] = {"available": True, "origin": "nvcc intermediate that ptxas compiled into module.cubin"}
    manifest["functions"] = function_table(sass.stdout, parse_res_usage(usage), parse_ptxas(proc.stdout + proc.stderr))
    manifest["common_resources"] = parse_res_usage(usage).get("", {})
    manifest["dependencies"] = dependencies(os.path.join(out, "deps.d"), user_roots, out, tu)
    manifest["git"] = git_state([path] + [d["path"] for d in manifest["dependencies"] if d["snapshot"]])
    manifest["artifacts"] = artifact_table(out, ["module.ptx", "module.cubin", "module.sass", "resources.txt",
                                                 "compiler.log", "deps.d", os.path.join("source", tu_name)])


# ---------------------------------------------------------------------------
# Normalization and structured diff
# ---------------------------------------------------------------------------

def strip_comments(line):
    """Removes // and /* */ comments outside double-quoted strings."""
    out, index, quoted = [], 0, False
    while index < len(line):
        char = line[index]
        if quoted:
            out.append(char)
            if char == "\\" and index + 1 < len(line):
                index += 1
                out.append(line[index])
            elif char == '"':
                quoted = False
        elif char == '"':
            quoted = True
            out.append(char)
        elif line.startswith("//", index):
            break
        elif line.startswith("/*", index):
            end = line.find("*/", index + 2)
            if end < 0:
                break
            out.append(" ")
            index = end + 2
            continue
        else:
            out.append(char)
        index += 1
    return "".join(out)


# nvcc's per-translation-unit hash in internal-linkage symbol names; it changes with the build path.
TU_HASH = re.compile(r"(?<=_INTERNAL_)[0-9a-f]{8}(?=_)|(?<=_GLOBAL__N__)[0-9a-f]{8}(?=_)")


def normalize_line(raw):
    return TU_HASH.sub("TU", " ".join(strip_comments(raw).split()))


PTX_FUNCTION = re.compile(r"\.(entry|func)\s+(?:\([^)]*\)\s*)?([A-Za-z_$][\w$]*)")


def parse_ptx(text):
    """Normalized PTX without comments, .loc/.file and debug sections; lines keep their source location."""
    files = dict(re.findall(r'^\s*\.file\s+(\d+)\s+"((?:[^"\\]|\\.)*)"', text, re.M))
    kept, module, functions = [], [], {}
    loc = current = section_depth = None
    depth, opened = 0, False
    for raw in text.splitlines():
        line = normalize_line(raw)
        if not line:
            continue
        if section_depth is None and line.split()[0] == ".section":
            section_depth = 0
        if section_depth is not None:
            section_depth += line.count("{") - line.count("}")
            if "}" in line and section_depth == 0:
                section_depth = None
            continue
        if line.startswith(".loc"):
            parts = line.split()
            loc = f"{files.get(parts[1], parts[1])}:{parts[2]}"
            continue
        if line.startswith((".file", "@@DWARF")):
            continue
        kept.append(line)
        if current is None and depth == 0:
            match = PTX_FUNCTION.search(line)
            if match:
                current, opened, loc = match.group(2), False, None
                functions[current] = {"entry": match.group(1) == "entry", "lines": []}
        depth += line.count("{") - line.count("}")
        if current is None:
            module.append(line)
            continue
        functions[current]["lines"].append((line, loc))
        opened = opened or "{" in line
        if opened and depth == 0:
            current = None
        elif not opened and line.endswith(";"):
            module.extend(l for l, _ in functions.pop(current)["lines"])
            current = None
    return {"normalized": "\n".join(kept) + "\n", "functions": functions, "data": module}


SASS_LOCATION = re.compile(r'//## File "((?:[^"\\]|\\.)*)", line (\d+)')


def parse_sass(text):
    """Normalized nvdisasm output without comments, offsets and debug sections; per-function lines keep locations."""
    kept, functions, sections = [], {}, {}
    section = current = loc = None
    for raw in text.splitlines():
        match = SASS_LOCATION.search(raw)
        if match:
            loc = f"{match.group(1)}:{match.group(2)}"
            continue
        line = normalize_line(raw)
        if not line:
            continue
        if line.split()[0] == ".section":
            section, current, loc = line.split()[1].split(",")[0], None, None
        if section and section.startswith((".debug", ".nv_debug")):
            continue
        kept.append(line)
        sections.setdefault(section or "header", []).append(line)
        match = re.match(r"\.type ([^,\s]+),@function", line)
        if match:
            current = match.group(1)
            functions[current] = {"entry": False, "lines": []}
        match = re.match(r'\.other ([^,\s]+),@"([^"]*)"', line)
        if match and "STO_CUDA_ENTRY" in match.group(2) and match.group(1) in functions:
            functions[match.group(1)]["entry"] = True
        if current:
            functions[current]["lines"].append((line, loc))
    data = {name: lines for name, lines in sections.items() if not name.startswith(".text.")}
    return {"normalized": "\n".join(kept) + "\n", "functions": functions, "data": data}


# Allocation-dependent names: registers, predicates, convergence barriers and compiler labels.
NAMES = {"sass": re.compile(r"\b(?:U?R\d+|U?P\d+|B\d+)\b|\.L_x_\d+|\.reuse\b"),
         "ptx": re.compile(r"%[a-z]+\d+\b|\$L__\w+")}

SASS_CATEGORIES = {
    "tensor": "HMMA IMMA DMMA BMMA HGMMA IGMMA QGMMA BGMMA",
    "global/local memory": "LD ST LDG STG LDL STL LDGSTS LDGDEPBAR ATOM ATOMG RED CCTL UBLKCP UTMALDG UTMASTG UTMAPF",
    "shared memory": "LDS STS LDSM STSM ATOMS",
    "constant": "LDC ULDC",
    "synchronization": "BAR DEPBAR MEMBAR SYNCS WARPSYNC ERRBAR ARRIVES FENCE",
    "control": "BRA BRX JMP JMX CALL RET EXIT BSSY BSYNC BREAK BPT KILL YIELD",
    "float": "FADD FMUL FFMA FMNMX FSETP FSET FSEL FCHK MUFU HADD2 HMUL2 HFMA2 HMNMX2 HSETP2 HSET2 DADD DMUL DFMA DSETP",
    "integer": "IADD IADD3 IMAD IMUL ISETP IMNMX IABS LOP3 SHF SHL SHR LEA POPC FLO BREV PRMT SEL BMSK SGXT IDP VABSDIFF",
    "conversion": "F2F F2I I2F I2I F2FP I2FP FRND",
    "data movement": "MOV MOVM SHFL S2R S2UR CS2R R2UR P2R R2P VOTE VOTEU REDUX MATCH",
}
SASS_CATEGORY = {op: category for category, ops in SASS_CATEGORIES.items() for op in ops.split()}


def masked(kind, line):
    return NAMES[kind].sub(lambda m: "" if m.group() == ".reuse" else "#", line)


def opcode(line):
    tokens = line.rstrip(";").split()
    if tokens and tokens[0].startswith("@"):
        tokens = tokens[1:]
    return tokens[0] if tokens else ""


def is_instruction(line):
    return line.endswith(";") and not line.startswith(".")


def category(kind, op):
    base = op.split(".")[0]
    if kind == "ptx":
        return base
    return SASS_CATEGORY.get(base) or SASS_CATEGORY.get(base[1:] if base.startswith("U") else "") or "other"


def counter_delta(a, b):
    return {k: [a.get(k, 0), b.get(k, 0)] for k in sorted(set(a) | set(b)) if a.get(k, 0) != b.get(k, 0)}


def compare_function(kind, a, b):
    am = [masked(kind, line) for line, _ in a]
    bm = [masked(kind, line) for line, _ in b]
    ops_a = collections.Counter(opcode(l) for l in am if is_instruction(l))
    ops_b = collections.Counter(opcode(l) for l in bm if is_instruction(l))
    categories_a, categories_b = collections.Counter(), collections.Counter()
    for op, count in ops_a.items():
        categories_a[category(kind, op)] += count
    for op, count in ops_b.items():
        categories_b[category(kind, op)] += count
    # Masked lines keyed by opcode or directive: register renaming cancels, operand and modifier changes remain.
    forms_a = collections.Counter((opcode(l), l) for l in am if not l.endswith(":") and l not in "{}")
    forms_b = collections.Counter((opcode(l), l) for l in bm if not l.endswith(":") and l not in "{}")
    operand_changes = {}
    for key, form in sorted((forms_a - forms_b) + (forms_b - forms_a)):
        change = operand_changes.setdefault(key, {"removed": {}, "added": {}})
        if forms_a[(key, form)] > forms_b[(key, form)]:
            change["removed"][form] = forms_a[(key, form)] - forms_b[(key, form)]
        else:
            change["added"][form] = forms_b[(key, form)] - forms_a[(key, form)]
    for key, change in operand_changes.items():
        change["operand_only"] = bool(change["removed"] and change["added"]) and ops_a[key] == ops_b[key]
    locations_a, locations_b, changed = set(), set(), 0
    # SequenceMatcher is quadratic in the worst case; very large captured modules compare slowly.
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, am, bm).get_opcodes():
        if tag != "equal":
            changed += max(i2 - i1, j2 - j1)
            locations_a.update(loc for _, loc in a[i1:i2] if loc)
            locations_b.update(loc for _, loc in b[j1:j2] if loc)
    return {"instructions": [sum(ops_a.values()), sum(ops_b.values())], "changed_lines": changed,
            "opcodes": counter_delta(ops_a, ops_b), "categories": counter_delta(categories_a, categories_b),
            "operand_changes": operand_changes,
            "source_locations": {"reference": sorted(locations_a), "candidate": sorted(locations_b)}}


def unified(a, b, name_a, name_b):
    return "".join(difflib.unified_diff(a.splitlines(True), b.splitlines(True), name_a, name_b))


def diff_view(kind, text_a, text_b, diff_dir):
    parse = parse_ptx if kind == "ptx" else parse_sass
    pa, pb = parse(text_a), parse(text_b)
    write_text(os.path.join(diff_dir, f"{kind}.raw.diff"), unified(text_a, text_b, "reference", "candidate"))
    write_text(os.path.join(diff_dir, f"{kind}.normalized.diff"),
               unified(pa["normalized"], pb["normalized"], "reference", "candidate"))
    fa, fb = pa["functions"], pb["functions"]

    def names(functions, entry):
        return {n for n, f in functions.items() if f["entry"] == entry}

    if kind == "ptx":
        da, db = collections.Counter(pa["data"]), collections.Counter(pb["data"])
        data = {"removed": sorted((da - db).elements()), "added": sorted((db - da).elements())}
    else:
        da, db = pa["data"], pb["data"]
        data = {"changed": sorted(n for n in set(da) & set(db) if da[n] != db[n]),
                "removed": sorted(set(da) - set(db)), "added": sorted(set(db) - set(da))}
    common = sorted(set(fa) & set(fb))
    return {
        "identical_raw": text_a == text_b,
        "identical_normalized": pa["normalized"] == pb["normalized"],
        "entries": {"added": sorted(names(fb, True) - names(fa, True)), "removed": sorted(names(fa, True) - names(fb, True))},
        "helpers": {"added": sorted(names(fb, False) - names(fa, False)),
                    "removed": sorted(names(fa, False) - names(fb, False))},
        "functions": {n: compare_function(kind, fa[n]["lines"], fb[n]["lines"]) for n in common},
        "data": data,
        "source_locations_available": any(loc for f in list(fa.values()) + list(fb.values()) for _, loc in f["lines"]),
        "diffs": {"raw": f"diff/{kind}.raw.diff", "normalized": f"diff/{kind}.normalized.diff"},
    }


def load_view(spec):
    """One comparison side from a compile output, or from a capture output with DIR::SYMBOL or DIR::SHA256-PREFIX."""
    path, _, select = spec.partition("::")
    try:
        manifest = read_json(os.path.join(path, "manifest.json"))
    except (OSError, ValueError) as error:
        raise Failure(f"{path} is not a gdbg codegen output: {error}")
    if manifest.get("schema") != SCHEMA or manifest.get("status") != "ok":
        raise Failure(f"{path}: expected an ok {SCHEMA} manifest, found {manifest.get('schema')} {manifest.get('status')}")
    if manifest["kind"] == "compile":
        return {"label": spec, "kind": "offline", "arch": manifest["target"]["arch"],
                "ptx": read_text(os.path.join(path, "module.ptx")), "ptx_reason": None,
                "sass": read_text(os.path.join(path, "module.sass")), "sass_scope": "module",
                "functions": manifest["functions"], "cubin_sha256": manifest["artifacts"]["module.cubin"]["sha256"]}
    if manifest["kind"] != "capture":
        raise Failure(f"{path}: cannot compare a {manifest['kind']} output")
    modules = [m for m in manifest["modules"]
               if select and (m["sha256"].startswith(select) or select in m["functions"]
                              or select in m.get("demangled", {}).values())]
    if len(modules) != 1:
        raise Failure(f"{path}: select exactly one captured module with ::SYMBOL or ::SHA256-PREFIX "
                      f"({len(modules)} match {select!r})")
    module = modules[0]
    if not module.get("sass"):
        raise Failure(f"{path}: module {module['sha256'][:16]} was not disassembled; recapture with --all-sass")
    ptx = module["ptx"]
    return {"label": spec, "kind": "captured", "arch": module["arch"],
            "ptx": read_text(os.path.join(path, ptx["path"])) if ptx["available"] else None,
            "ptx_reason": ptx.get("reason"), "sass": read_text(os.path.join(path, module["sass"])),
            "sass_scope": module["sass_scope"], "functions": module["functions"], "cubin_sha256": module["sha256"]}


def compare_views(ref, cand, diff_dir):
    os.makedirs(diff_dir)
    report = {"reference": {k: ref[k] for k in ("label", "kind", "arch", "sass_scope", "cubin_sha256")},
              "candidate": {k: cand[k] for k in ("label", "kind", "arch", "sass_scope", "cubin_sha256")},
              "cubin_identical": ref["cubin_sha256"] == cand["cubin_sha256"], "notes": [CAUTION]}
    if ref["arch"] != cand["arch"]:
        report["notes"].append(f"architectures differ ({ref['arch']} vs {cand['arch']}); instruction differences "
                               "follow from the target, not only from the source")
    if ref["kind"] != cand["kind"]:
        report["notes"].append("an offline target build and a loaded-module capture are distinct artifacts")
    if "launched kernels" in (ref["sass_scope"], cand["sass_scope"]):
        report["notes"].append("a captured side holds SASS for launched kernels only; other functions show as removed")
    if ref["ptx"] is None or cand["ptx"] is None:
        report["ptx"] = {"available": False, "reason": ref["ptx_reason"] or cand["ptx_reason"]}
    else:
        report["ptx"] = diff_view("ptx", ref["ptx"], cand["ptx"], diff_dir)
    report["sass"] = diff_view("sass", ref["sass"], cand["sass"], diff_dir)
    resources = {}
    for name in sorted(set(ref["functions"]) & set(cand["functions"])):
        a, b = ref["functions"][name]["resources"], cand["functions"][name]["resources"]
        resources[name] = {"reference": a, "candidate": b,
                           "changed": {k: [a[k], b[k]] for k in sorted(set(a) & set(b)) if a[k] != b[k]}}
    report["resources"] = resources
    return report


def render_report(report):
    lines = [f"reference: {report['reference']['label']} ({report['reference']['kind']} {report['reference']['arch']})",
             f"candidate: {report['candidate']['label']} ({report['candidate']['kind']} {report['candidate']['arch']})",
             f"cubin identical: {'yes' if report['cubin_identical'] else 'no'}"]
    for kind in ("ptx", "sass"):
        view = report[kind]
        if "identical_raw" not in view:
            lines.append(f"{kind.upper()}: unavailable: {view['reason']}")
            continue
        lines.append(f"{kind.upper()}: raw {'identical' if view['identical_raw'] else 'differs'}, normalized "
                     f"{'identical' if view['identical_normalized'] else 'differs'} ({view['diffs']['normalized']})")
        for group in ("entries", "helpers"):
            for change in ("added", "removed"):
                if view[group][change]:
                    lines.append(f"  {group} {change}: {', '.join(view[group][change])}")
        for change, items in view["data"].items():
            if items:
                lines.append(f"  data {change}: " + "; ".join(items[:8]) + (" ..." if len(items) > 8 else ""))
        for name, function in view["functions"].items():
            if not (function["changed_lines"] or function["opcodes"] or function["operand_changes"]):
                continue
            a, b = function["instructions"]
            lines.append(f"  {name}: instructions {a} -> {b}, changed lines {function['changed_lines']}")
            if function["categories"]:
                lines.append("    categories: " + ", ".join(f"{k} {x}->{y}" for k, (x, y) in function["categories"].items()))
            if function["opcodes"]:
                lines.append("    opcodes: " + ", ".join(f"{k} {x}->{y}" for k, (x, y) in function["opcodes"].items()))
            for key, change in function["operand_changes"].items():
                label = "operand-only" if change["operand_only"] else "forms"
                lines.append(f"    {label} {key}: -{sorted(change['removed'])} +{sorted(change['added'])}")
            for side in ("reference", "candidate"):
                if function["source_locations"][side]:
                    lines.append(f"    {side} source: " + ", ".join(function["source_locations"][side][:6]))
        if not view["source_locations_available"]:
            lines.append("  source locations: unavailable (build with -lineinfo to keep them)")
    for name, resources in report["resources"].items():
        if resources["changed"]:
            lines.append(f"resources {name}: " + ", ".join(f"{k} {a}->{b}" for k, (a, b) in resources["changed"].items()))
    lines += [f"note: {note}" for note in report["notes"]]
    return "\n".join(lines) + "\n"


def describe_functions(functions):
    lines = []
    for name, function in sorted(functions.items()):
        r = function["resources"]
        kind = "entry" if function["entry"] else "helper"
        fields = " ".join(f"{k}={r[k]}" for k in ("reg", "shared", "stack", "local", "spill_stores", "spill_loads") if k in r)
        lines.append(f"  {kind} {name} {fields}")
    return "\n".join(lines)


def cmd_compile(args):
    out = os.path.abspath(args.out)
    manifest = build(args.input, args, out)
    print(f"built {manifest['target']['arch']} (PTX {manifest['target']['ptx_arch']}) into {out}")
    print(describe_functions(manifest["functions"]))
    return 0


def cmd_compare(args):
    out = new_dir(args.out)
    manifest = {"schema": SCHEMA, "kind": "compare", "created": now(), "inputs": {}, "errors": []}
    views = {}
    for role, spec in (("reference", args.reference), ("candidate", args.candidate)):
        manifest["inputs"][role] = spec
        try:
            if os.path.isdir(spec.partition("::")[0]):
                views[role] = load_view(spec)
            else:
                build(spec, args, os.path.join(out, role))
                views[role] = load_view(os.path.join(out, role))
        except Failure as error:
            manifest["errors"].append(f"{role}: {error}")
    manifest["status"] = "failed" if manifest["errors"] else "ok"
    if views.keys() == {"reference", "candidate"}:
        manifest["report"] = compare_views(views["reference"], views["candidate"], os.path.join(out, "diff"))
        summary = render_report(manifest["report"])
        write_text(os.path.join(out, "summary.txt"), summary)
        print(summary, end="")
    write_json(os.path.join(out, "manifest.json"), manifest)
    for error in manifest["errors"]:
        print(f"gdbg codegen: {error}", file=sys.stderr)
    return 1 if manifest["errors"] else 0


# ---------------------------------------------------------------------------
# Loaded-module capture
# ---------------------------------------------------------------------------

def build_capture_library(tools, out):
    source = os.environ.get("GDBG_CODEGEN_CAPTURE_C")
    if not source:
        raise Failure("the capture library source is missing; run this through `gdbg codegen`")
    lib_dir = os.path.join(out, "capture-lib")
    os.makedirs(lib_dir)
    c_file, library = os.path.join(lib_dir, "capture.c"), os.path.join(lib_dir, "libgdbg_capture.so")
    write_text(c_file, source)
    root = os.path.dirname(os.path.dirname(tools["nvcc"]))
    extra = []
    if os.path.isdir(os.path.join(root, "extras", "CUPTI", "include")):
        extra.append("-I" + os.path.join(root, "extras", "CUPTI", "include"))
    for lib in (os.path.join(root, "extras", "CUPTI", "lib64"), os.path.join(root, "lib64")):
        if os.path.isdir(lib):
            extra += ["-L" + lib, "-Xlinker", "-rpath=" + lib]
    argv = [tools["nvcc"], "-shared", "-Xcompiler", "-fPIC", "-o", library, c_file, *extra, "-lcupti", "-lcuda"]
    proc = run(argv)
    write_text(os.path.join(lib_dir, "build.log"), f"$ {shlex.join(argv)}\n{proc.stdout}{proc.stderr}")
    if proc.returncode:
        raise Failure(f"cannot build the CUPTI capture library (needs cupti.h and libcupti); see {lib_dir}/build.log")
    linked = [line.split()[2] for line in run(["ldd", library]).stdout.splitlines() if "cupti" in line and "=>" in line]
    return library, {"library": library, "build_argv": argv, "libcupti": linked[0] if linked else None}


def load_records(raw):
    events, torn = [], 0
    for name in sorted(os.listdir(raw)):
        if name.startswith("records-") and name.endswith(".jsonl"):
            for line in read_text(os.path.join(raw, name)).splitlines():
                try:
                    events.append(json.loads(line))
                except ValueError:
                    torn += 1
    return events, torn


def provenance_index(dirs):
    index = {}
    for directory in dirs:
        for root, _, files in os.walk(directory):
            for name in files:
                if name.endswith(".cubin"):
                    index.setdefault(sha256_file(os.path.join(root, name)), os.path.join(root, name))
    return index


def cmd_capture(args):
    if not args.rest:
        raise Failure("usage: gdbg codegen capture --out DIR -- COMMAND [ARGS...]")
    if os.environ.get("CUDA_INJECTION64_PATH"):
        raise Failure("CUDA_INJECTION64_PATH is already set (another CUDA injection such as Nsight is active); "
                      "run capture as its own phase")
    tools = find_tools(args.nvcc)
    out = new_dir(args.out)
    raw, modules_dir = os.path.join(out, "raw"), os.path.join(out, "modules")
    os.makedirs(raw)
    os.makedirs(modules_dir)
    manifest = {"schema": SCHEMA, "kind": "capture", "status": "failed", "created": now(), "command": args.rest,
                "cwd": os.getcwd(), "tools": tool_versions(tools), "git": git_state([os.getcwd()]),
                "env": {k: v for k, v in sorted(os.environ.items())
                        if k.startswith(("CUDA_", "TRITON_", "TORCH_", "PYTORCH_", "NVIDIA_"))},
                "notes": ["The captured architecture is that of each loaded cubin; offline builds for an explicit "
                          "target are separate artifacts.",
                          "PTX is listed only when a provenance directory holds a byte-identical cubin; it is never "
                          "reconstructed."],
                "gaps": [], "errors": []}
    try:
        library, manifest["capture_library"] = build_capture_library(tools, out)
        env = dict(os.environ, CUDA_INJECTION64_PATH=library, GDBG_CAPTURE_DIR=raw)
        with open(os.path.join(out, "workload.stdout"), "w") as stdout, \
                open(os.path.join(out, "workload.stderr"), "w") as stderr:
            try:
                manifest["returncode"] = subprocess.run(args.rest, env=env, stdout=stdout, stderr=stderr).returncode
            except OSError as error:
                raise Failure(f"cannot run {args.rest[0]}: {error}")
        summarize_capture(args, tools, out, raw, manifest)
        if manifest["returncode"]:
            manifest["errors"].append(f"workload exited with {manifest['returncode']}; see workload.stderr")
        manifest["status"] = "failed" if manifest["errors"] else "ok"
    except Failure as error:
        manifest["errors"].append(str(error))
    finally:
        write_json(os.path.join(out, "manifest.json"), manifest)
    print(render_capture(manifest, out), end="")
    for error in manifest["errors"]:
        print(f"gdbg codegen: {error}", file=sys.stderr)
    return 1 if manifest["errors"] else 0


def summarize_capture(args, tools, out, raw, manifest):
    events, torn = load_records(raw)
    by_kind = collections.defaultdict(list)
    for event in events:
        by_kind[event["event"]].append(event)
    if torn:
        manifest["gaps"].append(f"{torn} truncated record lines (a process ended mid-write)")
    manifest["errors"] += [f"pid {e['pid']}: {e['call']} failed: {e['message']}" for e in by_kind["error"]]
    pids = sorted({e["pid"] for e in by_kind["init"]})
    if not pids:
        manifest["errors"].append("the CUPTI injection library never initialized: the workload did not initialize "
                                  "CUDA, or another tool owns CUDA injection")
    exits = {e["pid"]: e for e in by_kind["exit"]}
    manifest["processes"] = [{"pid": e["pid"], "cupti_api_version": e["cupti_api_version"], "exited": e["pid"] in exits}
                             for e in by_kind["init"]]

    modules = {}
    for event in by_kind["module"]:
        src = os.path.join(raw, event["file"])
        if event["written"] != event["size"] or not os.path.isfile(src):
            manifest["gaps"].append(f"module {event['file']} was not fully written")
            continue
        digest = sha256_file(src)
        if digest in modules:
            os.remove(src)
        else:
            os.replace(src, os.path.join(out, "modules", digest + ".cubin"))
            modules[digest] = {"sha256": digest, "cubin": f"modules/{digest}.cubin", "bytes": event["size"], "loads": []}
        modules[digest]["loads"].append({k: event[k] for k in ("pid", "module_id", "device", "device_cc", "image")})

    provenance_dirs = list(args.provenance)
    triton_cache = os.environ.get("TRITON_CACHE_DIR") or os.path.expanduser("~/.triton/cache")
    if os.path.isdir(triton_cache):
        provenance_dirs.append(triton_cache)
    manifest["provenance_dirs"] = provenance_dirs
    provenance = provenance_index(provenance_dirs)

    symbol_index = {}
    for digest, module in modules.items():
        cubin = os.path.join(out, module["cubin"])
        module["arch"] = cubin_arch(tools, cubin)
        usage_text = run([tools["cuobjdump"], "-res-usage", cubin]).stdout
        write_text(cubin[:-len(".cubin")] + ".resources.txt", usage_text)
        usage = parse_res_usage(usage_text)
        symbol_index[digest] = elf_functions(cubin)
        module["functions"] = {name: {"entry": name in usage, "resources": usage.get(name, {})}
                               for name in symbol_index[digest]}
        module["ptx"] = {"available": False, "reason": NO_CAPTURED_PTX}
        match = provenance.get(module["sha256"])
        if match:
            stem = os.path.basename(match)[:-len(".cubin")]
            copy_dir = os.path.join(out, "modules", module["sha256"] + ".provenance")
            os.makedirs(copy_dir)
            for name in os.listdir(os.path.dirname(match)):
                if name.startswith(stem + "."):
                    shutil.copyfile(os.path.join(os.path.dirname(match), name), os.path.join(copy_dir, name))
            module["provenance"] = {"matched_cubin": match, "files": sorted(os.listdir(copy_dir))}
            if os.path.isfile(os.path.join(copy_dir, stem + ".ptx")):
                module["ptx"] = {"available": True, "origin": "provenance",
                                 "path": f"modules/{module['sha256']}.provenance/{stem}.ptx",
                                 "note": f"sibling of byte-identical cubin {match}; not captured from the driver"}

    launches = {}
    for event in by_kind["launched"] + by_kind["launch_count"]:
        launch = launches.setdefault(event["symbol"], {"symbol": event["symbol"], "apis": [], "count": 0})
        if event.get("api") and event["api"] not in launch["apis"]:
            launch["apis"].append(event["api"])
        launch["count"] += event.get("count", 0)
    names = sorted({n for m in modules.values() for n in m["functions"]} | set(launches))
    demangled = demangle(tools, names)
    for launch in launches.values():
        launch["demangled"] = demangled.get(launch["symbol"])
        launch["modules"] = [d for d, m in modules.items() if launch["symbol"] in m["functions"]]
        launch["attribution"] = ("unnamed" if not launch["symbol"] else "unmatched" if not launch["modules"]
                                 else "unique" if len(launch["modules"]) == 1 else "ambiguous")
        if launch["attribution"] != "unique":
            manifest["gaps"].append(f"launch of {launch['symbol'] or '<unnamed>'} is {launch['attribution']} "
                                    f"across captured modules")

    for digest, module in modules.items():
        module["demangled"] = {n: demangled[n] for n in module["functions"] if demangled.get(n) not in (None, n)}
        wanted = [l["symbol"] for l in launches.values() if digest in l["modules"]]
        if args.all_sass:
            argv, module["sass_scope"] = [tools["nvdisasm"], "-g", module["cubin"]], "module"
        elif wanted:
            indices = ",".join(str(symbol_index[digest][n]) for n in wanted)
            argv = [tools["nvdisasm"], "-g", "-fun", indices, module["cubin"]]
            module["sass_scope"] = "launched kernels"
        else:
            module["sass"] = module["sass_scope"] = None
            continue
        proc = run(argv, cwd=out)
        if proc.returncode:
            module["sass"], module["sass_error"] = None, proc.stderr.strip()
            manifest["gaps"].append(f"nvdisasm failed for module {digest[:16]}: {proc.stderr.strip()}")
        else:
            module["sass"] = f"modules/{digest}.sass"
            write_text(os.path.join(out, module["sass"]), proc.stdout)

    graph_launches = sum(e["graph_launches"] for e in exits.values())
    dropped = sum(e["dropped_launches"] for e in exits.values())
    if graph_launches:
        manifest["gaps"].append(f"{graph_launches} CUDA graph launches: kernels replayed from graphs are neither "
                                "counted nor attributed, and launches recorded during stream capture are counted "
                                "although they run only on replay")
    if dropped:
        manifest["gaps"].append(f"{dropped} launches exceeded the capture library's symbol table")
    for pid in pids:
        if pid not in exits:
            manifest["gaps"].append(f"process {pid} did not exit normally; its launch counts are missing "
                                    "(first launches are still recorded)")
    manifest["launch_counts_complete"] = bool(pids) and all(p in exits for p in pids)
    manifest["graph_launches"] = graph_launches
    manifest["modules"] = list(modules.values())
    manifest["launches"] = sorted(launches.values(), key=lambda l: -l["count"])
    manifest["architectures"] = sorted({m["arch"] for m in modules.values() if m["arch"]})
    manifest["devices"] = sorted({tuple(l["device_cc"]) for m in modules.values() for l in m["loads"]})
    if args.session:
        manifest["session_correlation"] = correlate_session(args.session, modules, demangled)


def correlate_session(path, modules, demangled):
    """Matches a gdbg session's kernel names (nsys records) to captured module functions by exact name."""
    try:
        connection = sqlite3.connect(f"file:{os.path.abspath(path)}?mode=ro", uri=True)
        kernels = [row[0] for row in connection.execute("SELECT DISTINCT kernel_name FROM launches ORDER BY 1")]
    except sqlite3.Error as error:
        raise Failure(f"cannot read gdbg session {path}: {error}")
    rows = []
    for kernel in kernels:
        hits = [d for d, m in modules.items() if any(kernel in (n, demangled.get(n)) for n in m["functions"])]
        rows.append({"kernel": kernel, "modules": hits,
                     "status": "unmatched" if not hits else "unique" if len(hits) == 1 else "ambiguous"})
    return rows


def render_capture(manifest, out):
    lines = [f"capture {manifest['status']}: {out}"]
    for module in manifest.get("modules", []):
        launched = [l["symbol"] for l in manifest["launches"] if module["sha256"] in l["modules"]]
        ptx = "ptx from provenance" if module["ptx"]["available"] else "no ptx"
        lines.append(f"  module {module['sha256'][:16]} {module['arch']} {len(module['functions'])} functions, "
                     f"{len(launched)} launched, {ptx}, sass {module['sass_scope'] or 'not disassembled'}")
    for launch in manifest.get("launches", []):
        lines.append(f"  launch {launch['count']:>8} {launch['demangled'] or launch['symbol'] or '<unnamed>'} "
                     f"[{launch['attribution']}]")
    lines += [f"  gap: {gap}" for gap in manifest["gaps"]]
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Reference archive
# ---------------------------------------------------------------------------

def versions(directory):
    if not os.path.isdir(directory):
        return []
    return sorted((v for v in os.listdir(directory) if re.fullmatch(r"v\d+", v)), key=lambda v: int(v[1:]))


def cmd_archive_add(args):
    if not re.fullmatch(r"[A-Za-z0-9][\w.-]*", args.kernel):
        raise Failure("--kernel must be a plain identifier (letters, digits, '.', '_', '-')")
    snapshot = os.path.abspath(args.snapshot)
    try:
        manifest = read_json(os.path.join(snapshot, "manifest.json"))
    except (OSError, ValueError) as error:
        raise Failure(f"{snapshot} is not a gdbg codegen output: {error}")
    if manifest.get("schema") != SCHEMA or manifest.get("kind") not in ("compile", "capture"):
        raise Failure(f"{snapshot}: only {SCHEMA} compile or capture outputs can be archived")
    if manifest.get("status") != "ok":
        raise Failure(f"{snapshot}: status is {manifest.get('status')}; only successful outputs are archived")
    if args.role == "accepted" and not args.evidence:
        raise Failure("accepting a snapshot requires --evidence (the acceptance record)")
    role_dir = os.path.join(os.path.abspath(args.archive), args.kernel, args.role)
    existing = versions(role_dir)
    if args.role == "original" and existing:
        raise Failure(f"{args.kernel} already has a pinned original ({existing[0]})")
    names = [os.path.basename(path) for path in args.evidence]
    missing = [path for path in args.evidence if not os.path.isfile(path)]
    if missing or len(set(names)) != len(names):
        raise Failure(f"evidence must be existing files with distinct names: {missing or names}")
    version = f"v{int(existing[-1][1:]) + 1 if existing else 1:03d}"
    staging = os.path.join(role_dir, f".staging-{os.getpid()}")
    try:
        shutil.copytree(snapshot, os.path.join(staging, "snapshot"))
        os.makedirs(os.path.join(staging, "evidence"))
        evidence = []
        for path, name in zip(args.evidence, names):
            shutil.copyfile(path, os.path.join(staging, "evidence", name))
            evidence.append({"file": name, "sha256": sha256_file(path)})
        write_json(os.path.join(staging, "entry.json"), {
            "schema": SCHEMA, "kind": "archive-entry", "kernel": args.kernel, "role": args.role, "version": version,
            "specialization": args.specialization, "note": args.note, "added": now(), "snapshot_source": snapshot,
            "snapshot_kind": manifest["kind"],
            "snapshot_manifest_sha256": sha256_file(os.path.join(snapshot, "manifest.json")), "evidence": evidence})
        # A version directory appears only when complete; a concurrent add of the same version fails here.
        os.rename(staging, os.path.join(role_dir, version))
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    print(f"archived {args.kernel} {args.role} {version}")
    index = archive_index(args.archive)
    print(render_index(index), end="")
    return 0


def coverage_gap(requirement, entry, manifest):
    arch, source = requirement["arch"], requirement.get("source", "offline")
    symbol = requirement.get("symbol")
    wanted = {"offline": "compile", "capture": "capture"}[source]
    if manifest["kind"] != wanted:
        return f"current accepted snapshot is a {manifest['kind']}, the requirement needs {source}", None
    if requirement.get("specialization") not in (None, entry.get("specialization")):
        return f"current accepted specialization is {entry.get('specialization')!r}", None
    if source == "offline":
        if manifest["target"]["arch"] != arch:
            return f"current accepted offline target is {manifest['target']['arch']}", None
        names, ptx = list(manifest["functions"]), "available"
    else:
        modules = [m for m in manifest["modules"] if m["arch"] == arch]
        if not modules:
            return f"no captured module for {arch} (captured {manifest['architectures']})", None
        shas = {m["sha256"] for m in modules}
        names = [l["symbol"] for l in manifest["launches"] if shas & set(l["modules"])]
        matched = [m for m in modules if not symbol or any(re.search(symbol, n) for n in m["functions"])]
        ptx = "; ".join(sorted({"available" if m["ptx"]["available"] else f"unavailable: {m['ptx']['reason']}"
                                for m in matched})) or None
    if symbol and not any(re.search(symbol, n) for n in names):
        return f"no {'launched ' if source == 'capture' else ''}function matches {symbol!r}", ptx
    return None, ptx


def archive_index(archive):
    archive = os.path.abspath(archive)
    if not os.path.isdir(archive):
        raise Failure(f"archive {archive} does not exist")
    path = os.path.join(archive, "inventory.json")
    inventory = {"schema": INVENTORY_SCHEMA, "required": []}
    if os.path.exists(path):
        try:
            inventory = read_json(path)
        except (OSError, ValueError) as error:
            raise Failure(f"cannot read {path}: {error}")
    if inventory.get("schema") != INVENTORY_SCHEMA:
        raise Failure(f"{path} must declare schema {INVENTORY_SCHEMA}")
    kernels = {}
    for kernel in sorted(os.listdir(archive)):
        if os.path.isdir(os.path.join(archive, kernel)) and not kernel.startswith("."):
            kernels[kernel] = {role: versions(os.path.join(archive, kernel, role)) for role in ROLES}
    required, gaps = [], []
    for requirement in inventory.get("required", []):
        if not {"kernel", "arch"} <= set(requirement) or requirement.get("source", "offline") not in ("offline", "capture"):
            raise Failure(f"invalid inventory requirement {requirement}")
        accepted = kernels.get(requirement["kernel"], {}).get("accepted", [])
        status = {"requirement": requirement, "current_accepted": None, "ptx": None}
        if not accepted:
            gap = "no accepted snapshot"
        else:
            version_dir = os.path.join(archive, requirement["kernel"], "accepted", accepted[-1])
            status["current_accepted"] = os.path.relpath(version_dir, archive)
            gap, status["ptx"] = coverage_gap(requirement, read_json(os.path.join(version_dir, "entry.json")),
                                              read_json(os.path.join(version_dir, "snapshot", "manifest.json")))
        status["gap"] = gap
        required.append(status)
        if gap:
            label = " ".join(str(requirement[k]) for k in ("kernel", "arch", "source", "specialization") if requirement.get(k))
            gaps.append(f"{label}: {gap}")
    if not required:
        gaps.append("the inventory lists no required kernels" if os.path.exists(path) else
                    f"{path} is missing; it lists the required kernels ({INVENTORY_SCHEMA}, required: "
                    "[{kernel, arch, source: offline|capture, specialization?, symbol?}])")
    index = {"schema": SCHEMA, "kind": "archive-index", "generated": now(), "required": required, "gaps": gaps,
             "coverage_complete": not gaps, "kernels": kernels}
    write_json(os.path.join(archive, "index.json"), index)
    return index


def render_index(index):
    lines = [f"coverage {'complete' if index['coverage_complete'] else 'incomplete'}: "
             f"{sum(1 for r in index['required'] if not r['gap'])}/{len(index['required'])} requirements covered"]
    for kernel, roles in index["kernels"].items():
        lines.append(f"  {kernel}: " + ", ".join(f"{role} {' '.join(v) or '-'}" for role, v in roles.items()))
    lines += [f"  gap: {gap}" for gap in index["gaps"]]
    return "\n".join(lines) + "\n"


def cmd_archive_status(args):
    index = archive_index(args.archive)
    print(render_index(index), end="")
    return 0 if index["coverage_complete"] else 1


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def key_value(text):
    key, sep, value = text.partition("=")
    if not sep or not key:
        raise argparse.ArgumentTypeError("expected KEY=VALUE")
    return key, value


def parser():
    top = argparse.ArgumentParser(prog="gdbg codegen", description="Inspect CUDA code generation: offline builds for "
                                  "an explicit target, loaded-module capture, structured diffs and a reference archive.")
    sub = top.add_subparsers(dest="action", required=True)

    def build_options(p):
        p.add_argument("--arch", help="SASS target such as sm_90; never inferred from the local GPU")
        p.add_argument("--ptx-arch", help="virtual architecture for the PTX (default compute_<arch>)")
        p.add_argument("--out", required=True, help="output directory; must not exist")
        p.add_argument("--nvcc", help="nvcc to use (default $CUDA_HOME/bin/nvcc, then PATH)")
        p.add_argument("--torch-python", help="interpreter whose torch defines load_inline headers and flags")
        p.add_argument("--instantiate", action="append", default=[], metavar="TEXT",
                       help="source appended to the translation unit, such as an explicit template instantiation")
        p.add_argument("--param", action="append", default=[], type=key_value, metavar="KEY=VALUE",
                       help="specialization or launch parameter recorded in the manifest")

    p = sub.add_parser("compile", help="build one input; nvcc flags follow --")
    build_options(p)
    p.add_argument("input", help="file.cu, or file.py[::NAME|::@LINE] for a load_inline call")
    p = sub.add_parser("compare", help="build or load two inputs and diff them; nvcc flags follow --")
    build_options(p)
    p.add_argument("reference", help="source input, compile output dir, or capture dir::SYMBOL|SHA256-PREFIX")
    p.add_argument("candidate")
    p = sub.add_parser("capture", help="run COMMAND (after --) and keep every cubin it loads")
    p.add_argument("--out", required=True, help="output directory; must not exist")
    p.add_argument("--nvcc", help="nvcc used to build the CUPTI injection library and locate CUDA tools")
    p.add_argument("--provenance", action="append", default=[], metavar="DIR",
                   help="directory searched for byte-identical cubins with sibling PTX (the Triton cache is added)")
    p.add_argument("--session", help="gdbg session database whose kernel names are matched to captured modules")
    p.add_argument("--all-sass", action="store_true", help="disassemble every module, not only launched kernels")
    p = sub.add_parser("archive", help="versioned reference snapshots and required-kernel coverage")
    archive = p.add_subparsers(dest="archive_action", required=True)
    p = archive.add_parser("add", help="add a compile or capture output as a new version")
    p.add_argument("archive")
    p.add_argument("snapshot")
    p.add_argument("--kernel", required=True)
    p.add_argument("--role", required=True, choices=ROLES)
    p.add_argument("--specialization")
    p.add_argument("--evidence", action="append", default=[], metavar="FILE", help="acceptance evidence to keep")
    p.add_argument("--note")
    p = archive.add_parser("status", help="write index.json and exit nonzero unless coverage is complete")
    p.add_argument("archive")
    return top


def main(argv):
    rest = []
    if "--" in argv:
        argv, rest = argv[:argv.index("--")], argv[argv.index("--") + 1:]
    args = parser().parse_args(argv)
    args.rest = rest
    handler = {"compile": cmd_compile, "compare": cmd_compare, "capture": cmd_capture}.get(args.action)
    if handler is None:
        handler = cmd_archive_add if args.archive_action == "add" else cmd_archive_status
    try:
        return handler(args)
    except Failure as error:
        print(f"gdbg codegen: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
