"""Tile-pipelined scale kernel, compiled at import with torch's load_inline."""

import torch
from torch.utils.cpp_extension import load_inline

STAGES = 4
TILE = 128
BASE_FLAGS = ["-O3", "-lineinfo"]
TUNED_FLAGS = BASE_FLAGS + ["--use_fast_math"]

HEADER = """
#include <cuda_pipeline.h>

__constant__ float kGain[4] = {1.0f, 0.5f, 0.25f, 0.125f};
"""

KERNEL = f"""
template <int STAGES>
__global__ void staged_scale(const float* __restrict__ src, float* __restrict__ dst, int num_tiles) {{
  constexpr int kLookahead = STAGES - 2;  // groups left in flight while a tile is consumed
  __shared__ float stage[STAGES][{TILE}];
  const int lane = threadIdx.x;

  // Prologue: fill every stage before the first tile is consumed.
  for (int s = 0; s < STAGES; ++s) {{
    if (s < num_tiles) __pipeline_memcpy_async(&stage[s][lane], &src[s * {TILE} + lane], sizeof(float));
    __pipeline_commit();
  }}

  for (int tile = 0; tile < num_tiles; ++tile) {{
    const int slot = tile % STAGES;
    __pipeline_wait_prior(kLookahead);
    __syncthreads();
    dst[tile * {TILE} + lane] = stage[slot][lane] * kGain[tile & 3];
    __syncthreads();

    // Refill the slot just consumed with the tile STAGES ahead.
    const int ahead = tile + STAGES;
    if (ahead < num_tiles) __pipeline_memcpy_async(&stage[slot][lane], &src[ahead * {TILE} + lane], sizeof(float));
    __pipeline_commit();
  }}
}}
"""

LAUNCHER = f"""
torch::Tensor staged_scale_cuda(torch::Tensor x) {{
  auto out = torch::empty_like(x);
  const int num_tiles = x.numel() / {TILE};
  staged_scale<{STAGES}><<<1, {TILE}>>>(x.data_ptr<float>(), out.data_ptr<float>(), num_tiles);
  return out;
}}
"""

ext = load_inline(
    name="staged_scale",
    cpp_sources="torch::Tensor staged_scale_cuda(torch::Tensor x);",
    cuda_sources=HEADER + KERNEL + LAUNCHER,
    functions=["staged_scale_cuda"],
    extra_cuda_cflags=TUNED_FLAGS,
)

# Tiny probe used by the integration test to confirm the toolchain links a second extension.
probe = load_inline(
    name="staged_scale_probe",
    cpp_sources="int probe_sm();",
    cuda_sources="int probe_sm() { int d = 0; cudaGetDevice(&d); return d; }",
    functions=["probe_sm"],
)

if __name__ == "__main__":
    x = torch.randn(64 * TILE, device="cuda")
    gain = torch.tensor([1.0, 0.5, 0.25, 0.125], device="cuda").repeat_interleave(TILE).repeat(16)
    print("max error", (ext.staged_scale_cuda(x) - x * gain).abs().max().item())
