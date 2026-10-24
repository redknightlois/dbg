# "Pure refactor" of a pipelined kernel: is the H200 code really the same?

`candidate.py` is a PR against `reference.py`. The PR title is "refactor: rename and restructure staged_scale, no functional change". The kernel is CUDA compiled through `torch.utils.cpp_extension.load_inline`, and production runs it on H200 (sm_90).

The author's evidence:

- the numerics test passes
- `cuobjdump -sass` shows the same instruction count for `staged_scale` before and after

We don't consider equal counts enough. This box has an RTX 3080, not an H200, so a local benchmark tells us nothing about production.

Please tell us whether the sm_90 machine code of the candidate's `staged_scale` kernel is equivalent to the reference. If it isn't, list every difference in the generated code, point each one at the source change that causes it, and say which differences are cosmetic and which can change behavior or performance.

Constraints:

- Do not run `reference.py` or `candidate.py`. They compile and run on import, and the candidate is unreviewed code.
- Static evidence from the generated PTX/SASS only. Timing numbers don't count.
- Evidence must come from an sm_90 build, not from what this machine's GPU would run.
- "Same instruction count, so equivalent" is not an answer.

`solution.md` is a spoiler.
