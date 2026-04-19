torch.profiler (or gdbg's PyTorch timeline) shows a CPU-GPU gap at
the end of every step — the GPU stalls while the host waits on
`loss.item()`, which is a CUDA sync.

Fix: accumulate on-device and sync once at the end:

```python
running = torch.zeros(1, device="cuda")
for _ in range(N):
    loss = train_step(model, opt, xs, ys)
    running += loss.detach()
print(f"avg loss {running.item() / N:.4f}")
```

Now the host enqueues the next iteration immediately and the GPU
stays busy. Wall time drops ~2-3x on a modest GPU.
