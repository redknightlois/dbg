"""Evals for `gdbg codegen`.

    python3 src/gdbg/test_codegen.py [--gpu] [NAME_FILTER]

Needs nvcc. Inline CUDA checks need TORCH_PYTHON (an interpreter with torch). --gpu also runs loaded-module
capture on the local GPU under the shared GPU lock. GDBG_BIN runs the CLI through the gdbg binary instead of the
helper script.
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.dont_write_bytecode = True
import codegen  # noqa: E402

GPU_LOCK = "/tmp/openkernel-tracker-gpu.lock"
# Offline fixture directory, set by the offline eval and reused by the archive eval.
FIXTURE_ROOT = None
PIPE4 = "_Z4pipeILi4EEvPKfPfi"
HELPER_SASS = f"${PIPE4}$_Z6helperf"

KERNEL = r'''#include <cuda_pipeline.h>
#include "helper.h"
__constant__ float coeffs[4] = {1.0f, 2.0f, 3.0f, 4.0f};
__device__ int table[3] = {7, 8, 9};
__device__ __noinline__ float helper(float x) { return x * coeffs[1] + table[0]; }
template <int STAGES>
__global__ void pipe(const float* __restrict__ in, float* out, int n) {
  __shared__ float buf[STAGES][128];
  int t = threadIdx.x;
  for (int s = 0; s < STAGES; ++s) __pipeline_memcpy_async(&buf[s][t], &in[s * 128 + t], 4);
  __pipeline_commit();
  __pipeline_wait_prior(STAGES - 2);
  __syncthreads();
  out[t] = helper(buf[0][t]);
}
__global__ void plain(float* out) { out[threadIdx.x] = twice(coeffs[threadIdx.x & 3]); }
'''
INSTANTIATE = "template __global__ void pipe<4>(const float*, float*, int);\n"
HEADER = "__device__ inline float twice(float v) { return v * 2.0f; }\n"

APP = r'''#include <cstdio>
#include <cstdlib>
__global__ void scale(float* x, float a) { x[threadIdx.x] *= a; }
__global__ void bump(float* x) { x[threadIdx.x] += 1; }
int main(int argc, char** argv) {
  float* d; cudaMalloc(&d, 128 * sizeof(float));
  for (int i = 0; i < 3; ++i) scale<<<1, 128>>>(d, 2.0f);
  bump<<<1, 128>>>(d);
  cudaDeviceSynchronize();
  if (argc > 1) abort();
  cudaGraph_t g; cudaGraphExec_t ge; cudaStream_t s; cudaStreamCreate(&s);
  cudaStreamBeginCapture(s, cudaStreamCaptureModeGlobal); bump<<<1, 128, 0, s>>>(d); cudaStreamEndCapture(s, &g);
  cudaGraphInstantiate(&ge, g, 0); cudaGraphLaunch(ge, s); cudaGraphLaunch(ge, s);
  cudaDeviceSynchronize(); printf("ok %d\n", (int)cudaGetLastError()); return 0;
}
'''

TRITON = r'''import torch, triton, triton.language as tl
@triton.jit
def add_kernel(x, y, out, n, BLOCK: tl.constexpr):
    i = tl.program_id(0) * BLOCK + tl.arange(0, BLOCK)
    m = i < n
    tl.store(out + i, tl.load(x + i, mask=m) + tl.load(y + i, mask=m), mask=m)
x = torch.randn(4096, device="cuda"); y = torch.randn_like(x); o = torch.empty_like(x)
add_kernel[(4,)](x, y, o, 4096, BLOCK=1024)
torch.cuda.synchronize(); print("ok", bool(torch.allclose(o, x + y)))
'''

INLINE = r'''open(MARKER, "w").write("executed")
import torch.utils.cpp_extension as cx
from torch.utils.cpp_extension import load_inline as build
HDR = "#define N 4\n"
BODY = f"__global__ void k(float* x) {{ x[0] = N + {2}; }}"
first = build(name="a", cpp_sources="", cuda_sources=[HDR + BODY], extra_cuda_cflags=["-O3"] + ["-arch=sm_80"])
second = cx.load_inline("b", "", "__global__ void j(float* x) { x[1] = 1; }", no_implicit_headers=True, verbose=flag)
def f(src):
    return build(name="c", cpp_sources="", cuda_sources=src)
def g(**kw):
    return build(name="d", cpp_sources="", **kw)
twice = "__global__ void t() {}"
twice = "__global__ void u() {}"
h = build(name="e", cpp_sources="", cuda_sources=twice)
'''
# torch 2.11 load_inline signature; the CLI reads the installed one.
PARAMS = ["name", "cpp_sources", "cuda_sources", "sycl_sources", "functions", "extra_cflags", "extra_cuda_cflags",
          "extra_sycl_cflags", "extra_ldflags", "extra_include_paths", "build_directory", "verbose", "with_cuda",
          "with_sycl", "is_python_module", "with_pytorch_error_handling", "keep_intermediates", "use_pch",
          "no_implicit_headers"]

TORCH_ORACLE = r'''
import inspect, json, shlex, sys, torch.utils.cpp_extension as ext
path, name, flags, includes = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), json.loads(sys.argv[4])
kwargs = dict(path=path, name=name, sources=["cuda.cu"], extra_cflags=[], extra_cuda_cflags=flags, extra_ldflags=[],
              extra_include_paths=includes, with_cuda=True, is_standalone=False)
params = inspect.signature(ext._write_ninja_file_to_build_library).parameters
kwargs.update({k: v for k, v in (("extra_sycl_cflags", []), ("with_sycl", False)) if k in params})
ext._write_ninja_file_to_build_library(**kwargs)
line = next(l for l in open(path) if l.startswith("cuda_cflags ="))
print(json.dumps(shlex.split(line.split("=", 1)[1])))
'''


class Skip(Exception):
    pass


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    return path


def manifest(path):
    with open(os.path.join(path, "manifest.json")) as f:
        return json.load(f)


def cli_env():
    env = dict(os.environ)
    # torch passes $CC to nvcc as -ccbin; the evals use the default host compiler.
    env.pop("CC", None)
    env.pop("CUDA_INJECTION64_PATH", None)
    with open(os.path.join(HERE, "codegen_capture.c")) as f:
        env["GDBG_CODEGEN_CAPTURE_C"] = f.read()
    return env


def cli(*args, gpu=False, env=None):
    gdbg = os.environ.get("GDBG_BIN")
    argv = [gdbg, "codegen", *args] if gdbg else [sys.executable, "-I", os.path.join(HERE, "codegen.py"), *args]
    if gpu:
        argv = ["flock", GPU_LOCK, *argv]
    return subprocess.run(argv, capture_output=True, text=True, env=env or cli_env())


def ok(proc):
    assert proc.returncode == 0, f"exit {proc.returncode}\n{proc.stdout}\n{proc.stderr}"
    return proc


def failed(proc, text):
    assert proc.returncode == 1, f"exit {proc.returncode}\n{proc.stdout}\n{proc.stderr}"
    assert text in proc.stderr, proc.stderr
    return proc


def torch_python():
    python = os.environ.get("TORCH_PYTHON")
    if not python:
        raise Skip("set TORCH_PYTHON to an interpreter with torch")
    return python


class Fixture:
    """Kernel sources and one reference sm_90 build shared by the offline evals."""

    def __init__(self, root):
        self.root = root
        self.src = os.path.join(root, "src")
        write(os.path.join(self.src, "inc", "helper.h"), HEADER)
        self.ref_cu = write(os.path.join(self.src, "k.cu"), KERNEL + INSTANTIATE)
        # A doubled slash keeps a quoted `//` in every .file and //## File path.
        self.flags = ["--", "-lineinfo", "-I" + self.src + "//inc"]
        subprocess.run(["git", "init", "-q", self.src], check=True)
        subprocess.run(["git", "-C", self.src, "add", "."], check=True)
        subprocess.run(["git", "-C", self.src, "-c", "user.name=eval", "-c", "user.email=eval@localhost", "commit",
                        "-qm", "fixture"], check=True)
        self.ref = os.path.join(root, "ref")
        ok(cli("compile", "--arch", "sm_90", "--out", self.ref, self.ref_cu, *self.flags))

    def variant(self, name, old, new):
        source = KERNEL + INSTANTIATE
        assert source.count(old) == 1, old
        return write(os.path.join(self.src, name + ".cu"), source.replace(old, new))

    def compare(self, name, candidate, *extra):
        out = os.path.join(self.root, "cmp-" + name)
        proc = cli("compare", "--arch", "sm_90", "--out", out, *extra, self.ref, candidate, *self.flags)
        return proc, out


# ---------------------------------------------------------------------------
# Units
# ---------------------------------------------------------------------------

def test_strip_comments_keeps_quoted_slashes():
    assert codegen.strip_comments('.file 1 "/x//y.cu" // tail') == '.file 1 "/x//y.cu" '
    assert codegen.strip_comments('/*0010*/ IADD3 R1, R2, R3 ;') == '  IADD3 R1, R2, R3 ;'
    assert codegen.strip_comments(r'.pragma "a\"//b"; // c') == r'.pragma "a\"//b"; '
    assert codegen.strip_comments('MOV R1, R2 ; /* open') == 'MOV R1, R2 ; '


def test_masking_cancels_allocation_only():
    sass = "@!P0 IMAD.WIDE R4, R2.reuse, 0x4, UR6 ;"
    assert codegen.masked("sass", sass) == codegen.masked("sass", "@!P1 IMAD.WIDE R8, R9, 0x4, UR7 ;")
    assert codegen.masked("sass", sass) != codegen.masked("sass", "@!P0 IMAD.WIDE R4, R2, 0x8, UR6 ;")
    assert codegen.masked("sass", "LDG.E R0, desc[UR4][R2.64+0x10] ;").endswith("[#.64+0x10] ;")
    assert codegen.normalize_line(".dword _ZN34_INTERNAL_abc85a19_7_cuda_cu_kGain4cuda3std3__436_GLOBAL__N__abc85a19_7_"
                                  "cuda_cu_kGain6ignoreE // x") == (".dword _ZN34_INTERNAL_TU_7_cuda_cu_kGain4cuda3std3"
                                                                    "__436_GLOBAL__N__TU_7_cuda_cu_kGain6ignoreE")
    assert codegen.masked("ptx", "add.s32 %r5, %r3, 7;") == codegen.masked("ptx", "add.s32 %r1, %r2, 7;")
    assert codegen.masked("ptx", "ld.const.f32 %f2, [coeffs+4];") == "ld.const.f32 #, [coeffs+4];"
    assert codegen.category("sass", "UIADD3") == "integer" and codegen.category("sass", "HGMMA.64x128x16") == "tensor"


def test_split_flags():
    kept, removed = codegen.split_flags(["-O3", "-arch=sm_80", "-gencode", "arch=compute_80,code=sm_80",
                                         "--gpu-architecture", "sm_70", "-lineinfo", "-DX=1"])
    assert kept == ["-O3", "-lineinfo", "-DX=1"]
    assert removed == ["-arch=sm_80", "-gencode", "arch=compute_80,code=sm_80", "--gpu-architecture", "sm_70"]
    for flag in ("-o", "-c", "--ptx", "-MF"):
        try:
            codegen.split_flags([flag, "x"])
            raise AssertionError(flag)
        except codegen.Failure as error:
            assert flag in str(error)


def test_parsers():
    usage = codegen.parse_res_usage(" Common:\n  GLOBAL:12 CONSTANT[3]:16\n Function _Z1kv:\n  REG:10 STACK:0 "
                                    "SHARED:3072 LOCAL:0 CONSTANT[0]:548\n")
    assert usage[""] == {"global": 12, "constant[3]": 16}
    assert usage["_Z1kv"]["reg"] == 10 and usage["_Z1kv"]["shared"] == 3072 and usage["_Z1kv"]["constant[0]"] == 548
    spills = codegen.parse_ptxas("ptxas info    : Function properties for _Z1kv\n    16 bytes stack frame, 8 bytes "
                                 "spill stores, 4 bytes spill loads\n")
    assert spills == {"_Z1kv": {"stack_frame": 16, "spill_stores": 8, "spill_loads": 4}}
    ptx = codegen.parse_ptx('.visible .entry _Z1kv(\n.param .u64 p\n)\n{\n.loc 1 3 1\nret;\n}\n'
                            '.func (.param .b32 r) _Z1hf(\n.param .b32 x\n);\n.file 1 "/a//b.cu"\n'
                            '.section .debug_str\n{\n$L__info_string0:\n.b8 0\n}\n')
    assert list(ptx["functions"]) == ["_Z1kv"] and ptx["functions"]["_Z1kv"]["entry"]
    assert ("ret;", "/a//b.cu:3") in ptx["functions"]["_Z1kv"]["lines"]
    assert any(line.startswith(".func") for line in ptx["data"]), "prototypes are module-level"
    assert ".b8 0" not in ptx["normalized"] and ".file" not in ptx["normalized"]


def write_inline(root):
    marker = os.path.join(root, "executed")
    path = write(os.path.join(root, "inline.py"), INLINE.replace("MARKER", repr(marker)))
    return path, marker


def test_extractor():
    root = tempfile.mkdtemp(prefix="gdbg-codegen-")
    path, marker = write_inline(root)
    first_line = INLINE.splitlines().index(next(l for l in INLINE.splitlines() if l.startswith("first"))) + 1

    def failure(selector, text):
        try:
            codegen.extract_inline(path, selector, PARAMS)
        except codegen.Failure as error:
            assert text in str(error), str(error)
            return
        raise AssertionError(f"{selector} resolved")

    failure("", "found 5 load_inline calls")
    a = codegen.extract_inline(path, "a", PARAMS)
    assert a["values"]["cuda_sources"] == ["#define N 4\n__global__ void k(float* x) { x[0] = N + 2; }"]
    assert a["values"]["extra_cuda_cflags"] == ["-O3", "-arch=sm_80"] and a["line"] == first_line
    assert codegen.extract_inline(path, f"@{first_line}", PARAMS)["values"]["name"] == "a"
    b = codegen.extract_inline(path, "b", PARAMS)
    assert b["values"]["cuda_sources"].startswith("__global__ void j") and b["values"]["no_implicit_headers"] is True
    assert "verbose" in b["unresolved"]
    failure("c", "`cuda_sources` is not statically resolvable")
    failure("d", "**kwargs")
    failure("e", "bound 2 times")
    failure("zz", "selector 'zz' matches 0")
    assert not os.path.exists(marker)
    shutil.rmtree(root)


def test_torch_flags_match_torch():
    python = torch_python()
    root = tempfile.mkdtemp(prefix="gdbg-codegen-")
    flags, includes = ["-O3", " -lineinfo ", "-arch=sm_90"], [os.path.join(root, "inc")]
    env = cli_env()
    proc = subprocess.run([python, "-c", TORCH_ORACLE, os.path.join(root, "build.ninja"), "ext", json.dumps(flags),
                           json.dumps(includes)], capture_output=True, text=True, env=env, cwd=root)
    assert proc.returncode == 0, proc.stderr
    torch_flags = json.loads(proc.stdout)
    os.environ.pop("CC", None)
    ours = codegen.torch_cuda_flags(codegen.torch_query(python, root), "ext", flags, includes)
    assert codegen.split_flags(ours) == codegen.split_flags(torch_flags), (ours, torch_flags)
    shutil.rmtree(root)


# ---------------------------------------------------------------------------
# Offline builds and structured diff
# ---------------------------------------------------------------------------

def check_reference(fx):
    m = manifest(fx.ref)
    assert m["schema"] == codegen.SCHEMA and m["status"] == "ok"
    assert m["target"] == {"arch": "sm_90", "ptx_arch": "compute_90", "cubin_arch": "sm_90", "source": "explicit --arch"}
    functions = m["functions"]
    assert functions[PIPE4]["entry"] and functions["_Z5plainPf"]["entry"] and not functions[HELPER_SASS]["entry"]
    assert functions[PIPE4]["resources"]["shared"] >= 4 * 128 * 4 and "reg" in functions[PIPE4]["resources"]
    assert functions[HELPER_SASS]["resources"].get("stack_frame") == 0
    for name in ("module.ptx", "module.cubin", "module.sass", "resources.txt", "compiler.log", "deps.d", "source/k.cu"):
        assert m["artifacts"][name]["sha256"] and os.path.isfile(os.path.join(fx.ref, name)), name
    assert not os.path.exists(os.path.join(fx.ref, "build"))
    header = [d for d in m["dependencies"] if d["path"].endswith("/inc/helper.h")]
    assert header and header[0]["snapshot"] and os.path.isfile(os.path.join(fx.ref, header[0]["snapshot"]))
    assert any(d["path"].endswith("cuda_pipeline.h") and d["sha256"] for d in m["dependencies"])
    assert m["git"][0]["head"] and m["git"][0]["dirty"] == []
    assert {"nvcc", "ptxas", "nvdisasm", "cuobjdump"} <= set(m["tools"])
    sass = codegen.parse_sass(open(os.path.join(fx.ref, "module.sass")).read())
    assert any("//inc/helper.h:" in (loc or "") for _, loc in sass["functions"]["_Z5plainPf"]["lines"])


def check_refusals(fx):
    write(os.path.join(fx.root, "taken", "keep.txt"), "kept")
    failed(cli("compile", "--arch", "sm_90", "--out", os.path.join(fx.root, "taken"), fx.ref_cu), "refusing to overwrite")
    assert open(os.path.join(fx.root, "taken", "keep.txt")).read() == "kept"
    failed(cli("compile", "--out", os.path.join(fx.root, "noarch"), fx.ref_cu), "--arch is required")
    failed(cli("compile", "--arch", "compute_90", "--out", os.path.join(fx.root, "virt"), fx.ref_cu), "SASS target")
    failed(cli("compile", "--arch", "sm_90", "--out", os.path.join(fx.root, "o"), fx.ref_cu, "--", "-o", "x"), "conflicts")


def check_dirty_and_arch_flags(fx):
    write(fx.ref_cu, KERNEL + INSTANTIATE + "// edit\n")
    out = os.path.join(fx.root, "dirty")
    ok(cli("compile", "--arch", "sm_90", "--out", out, fx.ref_cu, *fx.flags, "-arch=sm_80"))
    m = manifest(out)
    assert m["git"][0]["dirty"] == ["k.cu"] and m["compiler"]["removed_arch_flags"] == ["-arch=sm_80"]
    assert m["target"]["cubin_arch"] == "sm_90"
    write(fx.ref_cu, KERNEL + INSTANTIATE)


def check_debug_path_only(fx):
    proc, out = fx.compare("same", fx.ref_cu)
    ok(proc)
    report = manifest(out)["report"]
    for kind in ("ptx", "sass"):
        assert not report[kind]["identical_raw"], f"{kind} raw output should carry the build path"
        assert report[kind]["identical_normalized"], open(os.path.join(out, "diff", f"{kind}.normalized.diff")).read()
        assert all(not f["changed_lines"] and not f["opcodes"] and not f["operand_changes"]
                   for f in report[kind]["functions"].values())
    assert not report["cubin_identical"]
    assert codegen.CAUTION in report["notes"]


def check_instruction_change(fx):
    proc, out = fx.compare("instr", fx.variant("instr", "x * coeffs[1] + table[0]", "x * coeffs[1] * x + table[0]"))
    ok(proc)
    report = manifest(out)["report"]
    assert report["ptx"]["functions"]["_Z6helperf"]["opcodes"]
    helper = report["sass"]["functions"][HELPER_SASS]
    assert helper["opcodes"] and helper["changed_lines"] and helper["source_locations"]["candidate"]
    assert not report["sass"]["functions"]["_Z5plainPf"]["changed_lines"]
    assert "instructions" in open(os.path.join(out, "summary.txt")).read()


def check_wait_distance(fx):
    proc, out = fx.compare("wait", fx.variant("wait", "STAGES - 2", "STAGES - 1"))
    ok(proc)
    report = manifest(out)["report"]
    ptx = report["ptx"]["functions"][PIPE4]
    assert not ptx["opcodes"], ptx["opcodes"]
    assert ptx["operand_changes"]["cp.async.wait_group"] == {
        "removed": {"cp.async.wait_group 2;": 1}, "added": {"cp.async.wait_group 3;": 1}, "operand_only": True}
    sass = report["sass"]["functions"][PIPE4]
    assert not sass["opcodes"] and sass["instructions"][0] == sass["instructions"][1]
    assert [k for k, c in sass["operand_changes"].items() if c["operand_only"]] == ["DEPBAR.LE"]
    assert not report["sass"]["data"]["changed"], report["sass"]["data"]
    assert "operand-only cp.async.wait_group" in open(os.path.join(out, "summary.txt")).read()


def check_constant_data(fx):
    proc, out = fx.compare("const", fx.variant("const", "3.0f, 4.0f}", "3.0f, 5.0f}"))
    ok(proc)
    report = manifest(out)["report"]
    assert any(line.startswith(".const") and "coeffs" in line for line in report["ptx"]["data"]["added"])
    assert any("coeffs" in line for line in report["ptx"]["data"]["removed"])
    assert report["sass"]["data"]["changed"] == [".nv.constant3"]
    for kind in ("ptx", "sass"):
        assert all(not f["changed_lines"] for f in report[kind]["functions"].values())


def check_entries_added_removed(fx):
    proc, out = fx.compare("entries", fx.variant("entries", "void plain(float* out)", "void extra(float* out)"))
    ok(proc)
    report = manifest(out)["report"]
    for kind in ("ptx", "sass"):
        assert report[kind]["entries"] == {"added": ["_Z5extraPf"], "removed": ["_Z5plainPf"]}, report[kind]["entries"]
    assert "entries added: _Z5extraPf" in open(os.path.join(out, "summary.txt")).read()


def check_specialization(fx):
    cu = write(os.path.join(fx.src, "generic.cu"), KERNEL)
    out = os.path.join(fx.root, "pipe3")
    ok(cli("compile", "--arch", "sm_90", "--out", out, "--param", "STAGES=3",
           "--instantiate", "template __global__ void pipe<3>(const float*, float*, int);", cu, *fx.flags, "-DTAG=1"))
    m = manifest(out)
    assert m["specialization"] == {"instantiate": ["template __global__ void pipe<3>(const float*, float*, int);"],
                                   "params": {"STAGES": "3"}, "defines": ["-DTAG=1"]}
    assert "_Z4pipeILi3EEvPKfPfi" in m["functions"] and PIPE4 not in m["functions"]
    report_out = os.path.join(fx.root, "cmp-spec")
    ok(cli("compare", "--out", report_out, fx.ref, out))
    report = manifest(report_out)["report"]
    assert report["sass"]["entries"] == {"added": ["_Z4pipeILi3EEvPKfPfi"], "removed": [PIPE4]}
    assert report["sass"]["helpers"]["added"] and report["sass"]["helpers"]["removed"]


def check_failed_second_build(fx):
    proc, out = fx.compare("broken", fx.variant("broken", "out[t] = helper(buf[0][t]);", "out[t] = helper(;"))
    failed(proc, "candidate: nvcc exited")
    m = manifest(out)
    assert m["status"] == "failed" and "report" not in m and m["inputs"]["reference"] == fx.ref
    candidate = manifest(os.path.join(out, "candidate"))
    assert candidate["status"] == "failed" and candidate["compiler"]["returncode"] != 0
    assert "error" in open(os.path.join(out, "candidate", "compiler.log")).read()
    assert os.path.isdir(os.path.join(out, "candidate", "build"))
    both = os.path.join(fx.root, "cmp-broken-both")
    failed(cli("compare", "--arch", "sm_90", "--out", both, fx.ref_cu, os.path.join(fx.src, "broken.cu"), *fx.flags),
           "candidate: nvcc exited")
    assert manifest(os.path.join(both, "reference"))["status"] == "ok"
    assert os.path.isfile(os.path.join(both, "reference", "module.sass"))


def test_offline():
    global FIXTURE_ROOT
    root = FIXTURE_ROOT = tempfile.mkdtemp(prefix="gdbg-codegen-")
    fx = Fixture(root)
    for check in (check_reference, check_refusals, check_dirty_and_arch_flags, check_debug_path_only,
                  check_instruction_change, check_wait_distance, check_constant_data, check_entries_added_removed,
                  check_specialization, check_failed_second_build):
        check(fx)


def test_inline_cuda():
    python = torch_python()
    root = tempfile.mkdtemp(prefix="gdbg-codegen-")
    path, marker = write_inline(root)
    failed(cli("compile", "--arch", "sm_90", "--torch-python", python, "--out", os.path.join(root, "all"), path),
           "select one with PATH::NAME")
    out = os.path.join(root, "a")
    ok(cli("compile", "--arch", "sm_90", "--torch-python", python, "--out", out, path + "::a"))
    m = manifest(out)
    assert m["input"]["kind"] == "inline" and m["input"]["implicit_headers"] and m["input"]["torch"]["torch"]
    assert m["compiler"]["removed_arch_flags"] == ["-arch=sm_80"] and m["target"]["cubin_arch"] == "sm_90"
    argv = m["compiler"]["argv"]
    assert "-DTORCH_EXTENSION_NAME=a" in argv and "--expt-relaxed-constexpr" in argv and "-O3" in argv
    source = open(os.path.join(out, "source", "cuda.cu")).read()
    assert source.startswith("\n".join(codegen.TORCH_IMPLICIT_HEADERS) + "\n#define N 4")
    assert "_Z1kPf" in m["functions"]
    assert any(d["path"].endswith("torch/types.h") for d in m["dependencies"])
    out = os.path.join(root, "b")
    ok(cli("compile", "--arch", "sm_90", "--torch-python", python, "--out", out, path + "::b"))
    m = manifest(out)
    assert not m["input"]["implicit_headers"] and "verbose" in m["input"]["unresolved"]
    assert open(os.path.join(out, "source", "cuda.cu")).read().startswith("__global__ void j")
    same = os.path.join(root, "same")
    ok(cli("compare", "--arch", "sm_90", "--torch-python", python, "--out", same, os.path.join(root, "a"), path + "::a"))
    report = manifest(same)["report"]
    assert "_INTERNAL_" in open(os.path.join(root, "a", "module.ptx")).read()
    for kind in ("ptx", "sass"):
        assert not report[kind]["identical_raw"] and report[kind]["identical_normalized"], kind
    failed(cli("compile", "--arch", "sm_90", "--torch-python", python, "--out", os.path.join(root, "c"), path + "::c"),
           "not statically resolvable")
    assert manifest(os.path.join(root, "c"))["status"] == "failed"
    assert not os.path.exists(marker), "candidate Python must never run"
    shutil.rmtree(root)


# ---------------------------------------------------------------------------
# Archive
# ---------------------------------------------------------------------------

def test_archive():
    root = FIXTURE_ROOT
    if root is None:
        raise Skip("needs the offline fixture")
    archive = os.path.join(root, "archive")
    failed(cli("archive", "status", archive), "does not exist")
    os.makedirs(archive)
    failed(cli("archive", "status", archive), "")
    assert "inventory.json is missing" in json.load(open(os.path.join(archive, "index.json")))["gaps"][0]
    ref, wait = os.path.join(root, "ref"), os.path.join(root, "cmp-wait", "candidate")
    evidence = write(os.path.join(root, "bench.txt"), "candidate accepted\n")
    inventory = os.path.join(archive, "inventory.json")
    write(inventory, json.dumps({"schema": codegen.INVENTORY_SCHEMA, "required": [
        {"kernel": "pipe", "arch": "sm_90", "source": "offline", "symbol": "^_Z4pipeILi4E"},
        {"kernel": "pipe", "arch": "sm_86", "source": "capture", "symbol": "pipe"},
        {"kernel": "plain", "arch": "sm_90", "specialization": "default"}]}))

    status = failed(cli("archive", "status", archive), "")
    index = json.load(open(os.path.join(archive, "index.json")))
    assert not index["coverage_complete"] and len(index["gaps"]) == 3, status.stdout

    ok(cli("archive", "add", archive, ref, "--kernel", "pipe", "--role", "original"))
    failed(cli("archive", "add", archive, ref, "--kernel", "pipe", "--role", "original"), "pinned original")
    failed(cli("archive", "add", archive, wait, "--kernel", "pipe", "--role", "accepted"), "requires --evidence")
    failed(cli("archive", "add", archive, wait, "--kernel", "pipe", "--role", "accepted", "--evidence", evidence,
               "--evidence", evidence), "distinct names")
    assert os.listdir(os.path.join(archive, "pipe")) == ["original"]
    failed(cli("archive", "add", archive, ref, "--kernel", "../x", "--role", "experimental"), "plain identifier")
    failed(cli("archive", "add", archive, os.path.join(root, "cmp-broken", "candidate"), "--kernel", "pipe",
               "--role", "experimental"), "status is failed")
    ok(cli("archive", "add", archive, ref, "--kernel", "pipe", "--role", "experimental"))
    ok(cli("archive", "add", archive, wait, "--kernel", "pipe", "--role", "experimental"))
    ok(cli("archive", "add", archive, wait, "--kernel", "pipe", "--role", "accepted", "--evidence", evidence))
    ok(cli("archive", "add", archive, ref, "--kernel", "plain", "--role", "accepted", "--evidence", evidence,
           "--specialization", "default"))
    entry = json.load(open(os.path.join(archive, "pipe", "accepted", "v001", "entry.json")))
    assert entry["evidence"][0]["sha256"] == codegen.sha256_file(evidence) and entry["snapshot_kind"] == "compile"
    assert os.path.isfile(os.path.join(archive, "pipe", "accepted", "v001", "snapshot", "module.sass"))

    failed(cli("archive", "status", archive), "")
    index = json.load(open(os.path.join(archive, "index.json")))
    assert index["kernels"]["pipe"] == {"original": ["v001"], "accepted": ["v001"], "experimental": ["v001", "v002"]}
    gaps = [r for r in index["required"] if r["gap"]]
    assert len(gaps) == 1 and gaps[0]["requirement"]["arch"] == "sm_86" and "needs capture" in gaps[0]["gap"]
    assert not index["coverage_complete"]

    write(inventory, json.dumps({"schema": codegen.INVENTORY_SCHEMA, "required": [
        {"kernel": "pipe", "arch": "sm_90", "symbol": "^_Z4pipeILi4E"}, {"kernel": "plain", "arch": "sm_90"}]}))
    ok(cli("archive", "status", archive))
    assert json.load(open(os.path.join(archive, "index.json")))["coverage_complete"]
    write(inventory, json.dumps({"schema": codegen.INVENTORY_SCHEMA, "required": [
        {"kernel": "pipe", "arch": "sm_80"}]}))
    failed(cli("archive", "status", archive), "")
    assert "offline target is sm_90" in json.load(open(os.path.join(archive, "index.json")))["gaps"][0]
    write(inventory, json.dumps({"schema": codegen.INVENTORY_SCHEMA, "required": []}))
    failed(cli("archive", "status", archive), "")
    assert json.load(open(os.path.join(archive, "index.json")))["gaps"] == ["the inventory lists no required kernels"]


# ---------------------------------------------------------------------------
# Loaded-module capture (--gpu)
# ---------------------------------------------------------------------------

def local_arch():
    if "--gpu" not in sys.argv:
        raise Skip("pass --gpu to run capture on the local GPU")
    proc = subprocess.run(["nvidia-smi", "--query-gpu=compute_cap", "--format=csv,noheader"], capture_output=True,
                          text=True)
    if proc.returncode:
        raise Skip("no local GPU")
    return "sm_" + proc.stdout.split()[0].replace(".", "")


def test_capture_cuda():
    arch = local_arch()
    root = tempfile.mkdtemp(prefix="gdbg-codegen-")
    app = os.path.join(root, "app")
    subprocess.run(["nvcc", "-arch=native", "-o", app, write(os.path.join(root, "app.cu"), APP)], check=True)
    session = os.path.join(root, "app.gpu.db")
    with sqlite3.connect(session) as db:
        db.execute("CREATE TABLE launches (id INTEGER PRIMARY KEY, kernel_name TEXT NOT NULL, duration_us REAL NOT NULL)")
        db.executemany("INSERT INTO launches (kernel_name, duration_us) VALUES (?, 1)",
                       [("scale(float *, float)",), ("_Z4bumpPf",), ("other_kernel",)])
    out = os.path.join(root, "cap")
    ok(cli("capture", "--out", out, "--session", session, "--", app, gpu=True))
    m = manifest(out)
    assert m["status"] == "ok" and m["architectures"] == [arch] and m["launch_counts_complete"]
    assert {r["kernel"]: r["status"] for r in m["session_correlation"]} == {
        "scale(float *, float)": "unique", "_Z4bumpPf": "unique", "other_kernel": "unmatched"}
    launches = {l["symbol"]: l for l in m["launches"]}
    assert launches["_Z5scalePff"]["count"] == 3 and launches["_Z5scalePff"]["attribution"] == "unique"
    assert launches["_Z5scalePff"]["demangled"] == "scale(float *, float)"
    assert launches["_Z4bumpPf"]["count"] == 2, "the stream-captured launch is counted once at capture time"
    assert m["graph_launches"] == 2 and any("graph launches" in g for g in m["gaps"])
    module = next(mod for mod in m["modules"] if "_Z5scalePff" in mod["functions"])
    assert module["ptx"] == {"available": False, "reason": codegen.NO_CAPTURED_PTX}
    assert module["functions"]["_Z5scalePff"]["entry"] and module["sass_scope"] == "launched kernels"
    assert os.path.isfile(os.path.join(out, module["cubin"])) and codegen.sha256_file(
        os.path.join(out, module["cubin"])) == module["sha256"]

    offline = os.path.join(root, "offline")
    ok(cli("compile", "--arch", "sm_90", "--out", offline, os.path.join(root, "app.cu")))
    compare = os.path.join(root, "cmp")
    ok(cli("compare", "--out", compare, offline, out + "::scale(float *, float)"))
    report = manifest(compare)["report"]
    assert report["candidate"]["kind"] == "captured" and report["candidate"]["arch"] == arch
    assert report["ptx"] == {"available": False, "reason": codegen.NO_CAPTURED_PTX}
    if arch != "sm_90":
        assert any(n.startswith("architectures differ (sm_90 vs " + arch) for n in report["notes"])
    assert any("distinct artifacts" in n for n in report["notes"])

    crashed = os.path.join(root, "crash")
    failed(cli("capture", "--out", crashed, "--all-sass", "--", app, "abort", gpu=True), "workload exited")
    m = manifest(crashed)
    assert m["status"] == "failed" and m["modules"] and not m["launch_counts_complete"]
    assert m["modules"][0]["sass_scope"] == "module" and os.path.isfile(os.path.join(crashed, m["modules"][0]["sass"]))
    assert any("did not exit normally" in g for g in m["gaps"])
    assert {l["symbol"] for l in m["launches"]} == {"_Z5scalePff", "_Z4bumpPf"}

    env = dict(cli_env(), CUDA_INJECTION64_PATH="/nonexistent.so")
    failed(cli("capture", "--out", os.path.join(root, "nested"), "--", app, env=env), "already set")
    assert not os.path.exists(os.path.join(root, "nested"))
    shutil.rmtree(root)


def test_capture_triton():
    local_arch()
    python = torch_python()
    if subprocess.run([python, "-c", "import triton"], capture_output=True).returncode:
        raise Skip("TORCH_PYTHON has no triton")
    root = tempfile.mkdtemp(prefix="gdbg-codegen-")
    script = write(os.path.join(root, "workload", "tri.py"), TRITON)
    env = dict(cli_env(), TRITON_CACHE_DIR=os.path.join(root, "triton-cache"))
    out = os.path.join(root, "cap")
    ok(cli("capture", "--out", out, "--", python, "-I", script, gpu=True, env=env))
    m = manifest(out)
    launch = next(l for l in m["launches"] if l["symbol"] == "add_kernel")
    assert launch["apis"] == ["cuLaunchKernelEx"] and launch["attribution"] == "unique"
    module = next(mod for mod in m["modules"] if mod["sha256"] in launch["modules"])
    assert module["ptx"]["available"] and module["ptx"]["origin"] == "provenance"
    assert "add_kernel" in open(os.path.join(out, module["ptx"]["path"])).read()
    assert any(f.endswith(".json") for f in module["provenance"]["files"])
    library = [mod for mod in m["modules"] if mod is not module]
    assert library and all(mod["ptx"]["available"] is False for mod in library)
    shutil.rmtree(root)


if __name__ == "__main__":
    selected = [a for a in sys.argv[1:] if not a.startswith("--")]
    tests = [(n, f) for n, f in list(globals().items())
             if n.startswith("test_") and callable(f) and (not selected or any(s in n for s in selected))]
    failures = 0
    for name, function in tests:
        try:
            function()
            print(f"pass {name}")
        except Skip as reason:
            print(f"skip {name}: {reason}")
        except Exception as error:
            failures += 1
            print(f"FAIL {name}: {type(error).__name__}: {error}")
    root = FIXTURE_ROOT
    if root and not failures:
        shutil.rmtree(root)
    elif root:
        print(f"kept {root}")
    sys.exit(1 if failures else 0)
