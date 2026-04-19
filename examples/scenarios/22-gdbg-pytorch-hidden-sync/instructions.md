# Training loop is CPU-bound for no obvious reason

The inner training step is small MLP on GPU — forward, backward,
opt.step, nothing exotic. Yet `nvidia-smi` shows GPU utilization
around 30 %. We expected to be GPU-bound; instead the CPU is the
one working hard and the GPU is idle more than half the time.

Reproduce:

```
cd examples/scenarios/22-gdbg-pytorch-hidden-sync
python3 broken.py
```

Something in the host code is forcing a sync per step. Please
profile with torch.profiler / gdbg, show me the CPU-GPU timeline,
and identify the culprit line. The fix should be a one-liner that
preserves the loss-reporting semantics — we still want to see the
average loss at the end.
