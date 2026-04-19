"""A Triton matmul kernel with hardcoded BLOCK_M / BLOCK_N / BLOCK_K
and a single num_warps setting. Correct, but badly configured for
anything except the tile size the author happened to test with —
so on shapes slightly different from that test case, occupancy
tanks and the kernel runs 3-5x slower than torch.matmul.

gdbg (or torch profiler + ncu) will show low achieved occupancy
and few active warps. The obvious fix: wrap with @triton.autotune
over a sensible grid of block sizes + num_warps."""

import torch
import triton
import triton.language as tl


@triton.jit
def matmul_kernel(
    a_ptr, b_ptr, c_ptr,
    M, N, K,
    stride_am, stride_ak,
    stride_bk, stride_bn,
    stride_cm, stride_cn,
    BLOCK_M: tl.constexpr,
    BLOCK_N: tl.constexpr,
    BLOCK_K: tl.constexpr,
):
    pid_m = tl.program_id(0)
    pid_n = tl.program_id(1)
    offs_m = pid_m * BLOCK_M + tl.arange(0, BLOCK_M)
    offs_n = pid_n * BLOCK_N + tl.arange(0, BLOCK_N)
    offs_k = tl.arange(0, BLOCK_K)

    a_ptrs = a_ptr + offs_m[:, None] * stride_am + offs_k[None, :] * stride_ak
    b_ptrs = b_ptr + offs_k[:, None] * stride_bk + offs_n[None, :] * stride_bn

    acc = tl.zeros((BLOCK_M, BLOCK_N), dtype=tl.float32)
    for k in range(0, K, BLOCK_K):
        a = tl.load(a_ptrs, mask=(offs_m[:, None] < M) & ((offs_k[None, :] + k) < K), other=0.0)
        b = tl.load(b_ptrs, mask=((offs_k[:, None] + k) < K) & (offs_n[None, :] < N), other=0.0)
        acc += tl.dot(a, b)
        a_ptrs += BLOCK_K * stride_ak
        b_ptrs += BLOCK_K * stride_bk

    c_ptrs = c_ptr + offs_m[:, None] * stride_cm + offs_n[None, :] * stride_cn
    tl.store(c_ptrs, acc, mask=(offs_m[:, None] < M) & (offs_n[None, :] < N))


def matmul(a: torch.Tensor, b: torch.Tensor) -> torch.Tensor:
    M, K = a.shape
    K2, N = b.shape
    assert K == K2
    c = torch.empty((M, N), device=a.device, dtype=torch.float32)
    # BUG: hardcoded tile sizes, no autotune. Fine for the M=N=K=4096
    # case the author tested, terrible for the shapes we actually
    # run in production.
    BLOCK_M = 16
    BLOCK_N = 16
    BLOCK_K = 16
    grid = (triton.cdiv(M, BLOCK_M), triton.cdiv(N, BLOCK_N))
    matmul_kernel[grid](
        a, b, c,
        M, N, K,
        a.stride(0), a.stride(1),
        b.stride(0), b.stride(1),
        c.stride(0), c.stride(1),
        BLOCK_M=BLOCK_M, BLOCK_N=BLOCK_N, BLOCK_K=BLOCK_K,
    )
    return c


def main():
    torch.manual_seed(0)
    # Production shape — not what the kernel was tuned for.
    a = torch.randn((1024, 1024), device="cuda")
    b = torch.randn((1024, 1024), device="cuda")
    _ = matmul(a, b)
    torch.cuda.synchronize()
    import time
    t0 = time.time()
    for _ in range(50):
        _ = matmul(a, b)
    torch.cuda.synchronize()
    print(f"triton matmul: {(time.time() - t0) * 1000 / 50:.2f} ms/iter")

    t0 = time.time()
    for _ in range(50):
        _ = torch.matmul(a, b)
    torch.cuda.synchronize()
    print(f"torch.matmul:  {(time.time() - t0) * 1000 / 50:.2f} ms/iter")


if __name__ == "__main__":
    main()
