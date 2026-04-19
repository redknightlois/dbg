ncu on the 1024×1024×1024 run shows achieved occupancy ~10-15 %,
grid size 64×64 = 4096 blocks each running a tiny 16×16 tile.
The kernel is launch-heavy and under-occupies SMs; tensor-core
paths aren't being hit at all because the tiles are too small.

Fix: wrap `matmul_kernel` with `@triton.autotune` over a grid of
(BLOCK_M, BLOCK_N, BLOCK_K, num_warps) including at minimum
(128, 128, 32, 4) and (64, 64, 32, 2), keyed on `[M, N, K]`.
Triton picks the winning config per shape. On the 1024 case this
closes the gap to torch.matmul; the 4096 case keeps its current
winning config.
