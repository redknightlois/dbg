"""A training-loop inner step that looks pure-GPU on the surface —
but .item() in the progress-print path is a CUDA sync. Every
iteration stalls the host waiting for the forward+backward to
finish before the next minibatch is enqueued. torch.profiler shows
a CPU-GPU gap at each sync point; the GPU is only busy ~30% of
wall clock.

Fix: batch the loss prints, or accumulate the scalar in a tensor
and .item() it outside the hot loop."""

import time
import torch
import torch.nn as nn


def make_model():
    return nn.Sequential(
        nn.Linear(512, 512),
        nn.ReLU(),
        nn.Linear(512, 512),
        nn.ReLU(),
        nn.Linear(512, 10),
    ).cuda()


def train_step(model, opt, x, y):
    opt.zero_grad(set_to_none=True)
    logits = model(x)
    loss = nn.functional.cross_entropy(logits, y)
    loss.backward()
    opt.step()
    return loss


def main():
    torch.manual_seed(0)
    model = make_model()
    opt = torch.optim.SGD(model.parameters(), lr=1e-3)
    xs = torch.randn(64, 512, device="cuda")
    ys = torch.randint(0, 10, (64,), device="cuda")

    N = 200
    t0 = time.time()
    running = 0.0
    for _ in range(N):
        loss = train_step(model, opt, xs, ys)
        # BUG: .item() triggers a CUDA sync every iteration.
        running += loss.item()
    torch.cuda.synchronize()
    print(f"{N} steps in {time.time() - t0:.2f}s — avg loss {running / N:.4f}")


if __name__ == "__main__":
    main()
