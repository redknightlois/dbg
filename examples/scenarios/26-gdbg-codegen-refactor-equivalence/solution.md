# Solution

## Command

```
gdbg codegen compare --arch sm_90 --torch-python <python with torch> --out /tmp/staged-cmp \
  reference.py candidate.py::staged_scale
```

`candidate.py` has two `load_inline` calls (`staged_scale` and `staged_scale_probe`), so the candidate needs the `::staged_scale` selector. The scripts are read statically; neither is executed.

## Traps

- Equal instruction counts. `staged_scale<4>` has the same PTX and SASS instruction counts on both sides.
- Raw noise. The raw PTX and SASS diffs differ in build paths, `.loc` line numbers and nvcc's per-file hash in internal symbol names. The normalized diff removes all of that.
- Wrong target. The local GPU is sm_86. A capture or a default build on this box shows sm_86 code, not what the H200 runs.
- The candidate source reads like a rename. Both real changes are easy to miss in the Python diff: one hides behind a `constexpr`, the other behind a flags variable.

## The differences

1. Pipeline wait distance. `constexpr int kLookahead = STAGES - 2;` replaced `STAGES - 1` in `__pipeline_wait_prior`. PTX `cp.async.wait_group 3` becomes `cp.async.wait_group 2`; SASS `DEPBAR.LE SB0, 0x3` becomes `DEPBAR.LE SB0, 0x2`. Same opcode, different operand. Each iteration now waits for one more copy group than the tile it consumes needs, so one fewer tile stays in flight. Results are unchanged, which is why the numerics test passes; less copy latency is hidden, a performance regression to confirm on H200. Fix: `STAGES - 1`.
2. Fast math. `extra_cuda_cflags=TUNED_FLAGS` adds `--use_fast_math`. The five `mul.f32` become `mul.ftz.f32` in PTX and `FMUL` becomes `FMUL.FTZ` in SASS: denormal inputs and results flush to zero. The count is unchanged. It changes numerics only for denormals, which the test data never produces. Fix: drop the flag or justify it in the PR.

Cosmetic:

- The `__shared__` array was renamed from `buf` to `stage`. Its PTX symbol changes (`.shared` declaration and `mov.u32` of its address); the SASS is unaffected.
- Local variable renames and moved lines only change `.loc` line numbers.

## Passing answer

States that the code is not equivalent, gives both real differences with the PTX or SASS instructions from an sm_90 build and the source change behind each, classifies the rename as cosmetic, and runs neither script.
