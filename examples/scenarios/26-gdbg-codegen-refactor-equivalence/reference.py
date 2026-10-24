"""Tile-pipelined scale kernel, compiled at import with torch's load_inline."""

import torch
from torch.utils.cpp_extension import load_inline

STAGES = 4
TILE = 128

CUDA_SRC = f"""
#include <cuda_pipeline.h>

__constant__ float kGain[4] = {{1.0f, 0.5f, 0.25f, 0.125f}};

template <int STAGES>
__global__ void staged_scale(const float* __restrict__ in, float* __restrict__ out, int tiles) {{
  __shared__ float buf[STAGES][{TILE}];
  const int t = threadIdx.x;
  for (int s = 0; s < STAGES; ++s) {{
    if (s < tiles) __pipeline_memcpy_async(&buf[s][t], &in[s * {TILE} + t], sizeof(float));
    __pipeline_commit();
  }}
  for (int tile = 0; tile < tiles; ++tile) {{
    __pipeline_wait_prior(STAGES - 1);
    __syncthreads();
    out[tile * {TILE} + t] = buf[tile % STAGES][t] * kGain[tile & 3];
    __syncthreads();
    const int next = tile + STAGES;
    if (next < tiles) __pipeline_memcpy_async(&buf[tile % STAGES][t], &in[next * {TILE} + t], sizeof(float));
    __pipeline_commit();
  }}
}}

torch::Tensor staged_scale_cuda(torch::Tensor x) {{
  auto out = torch::empty_like(x);
  const int tiles = x.numel() / {TILE};
  staged_scale<{STAGES}><<<1, {TILE}>>>(x.data_ptr<float>(), out.data_ptr<float>(), tiles);
  return out;
}}
"""

ext = load_inline(
    name="staged_scale",
    cpp_sources="torch::Tensor staged_scale_cuda(torch::Tensor x);",
    cuda_sources=CUDA_SRC,
    functions=["staged_scale_cuda"],
    extra_cuda_cflags=["-O3", "-lineinfo"],
)

if __name__ == "__main__":
    x = torch.randn(64 * TILE, device="cuda")
    gain = torch.tensor([1.0, 0.5, 0.25, 0.125], device="cuda").repeat_interleave(TILE).repeat(16)
    print("max error", (ext.staged_scale_cuda(x) - x * gain).abs().max().item())
