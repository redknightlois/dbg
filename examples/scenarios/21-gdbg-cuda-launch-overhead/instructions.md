# GPU utilization near zero but wall time is bad

Port of a CPU routine to CUDA. Kernel itself looks tiny — it
should finish in microseconds — but the whole program takes close
to a second for 100k elements. `nvidia-smi` shows GPU utilization
hovering near zero during the run.

Reproduce:

```
cd examples/scenarios/21-gdbg-cuda-launch-overhead
make
time ./broken
```

Please collect an nsys timeline (gdbg) and tell me what the GPU is
actually doing. I suspect kernel launches aren't even overlapping
with each other. Propose a rewrite that hits real GPU utilization —
the work per element is trivial, so the batching should be obvious
once the profile is in front of you.
