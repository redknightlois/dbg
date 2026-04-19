# Triton matmul is slower than torch.matmul on our shapes

We wrote a Triton matmul kernel for a specific inference path.
Author benchmarked it at M=N=K=4096 and it was competitive. In
production we run M=N=K=1024 most of the time — and the Triton
kernel is 3-5x slower than `torch.matmul` on that shape. We
don't understand why.

Reproduce:

```
cd examples/scenarios/24-gdbg-triton-no-autotune
python3 broken.py       # requires triton + cuda-enabled torch
```

Please profile the kernel (ncu / gdbg) on the 1024×1024×1024 shape
and tell me what's actually limiting throughput — occupancy, tile
size, memory traffic, something else. Propose a concrete change
to the kernel setup (not the inner compute) that closes the gap to
`torch.matmul` on this shape without regressing the 4096 case.
