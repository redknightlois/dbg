ncu flags this as memory-bound: DRAM throughput near peak, SM
utilization low, FP32 utilization a few percent. Arithmetic
intensity (FLOPs / bytes) is ~2/16 = 0.125 — well below the
machine-balance ridgeline of the roofline.

FMA won't help. Real win: fuse `normalize` with the upstream
producer of `x` so the read of `x` is served from registers / L1
instead of global memory. Or swap the schedule so mean/scale are
broadcast (one value per block) instead of per-element.
