nsys timeline shows 100k back-to-back kernel launches, each
~5-10 µs, separated by queue-latency gaps; the GPU is idle between
them. ncu (if run) confirms each kernel has ~1 thread active and
abysmal occupancy.

Fix: launch the kernel once with a grid that covers the full
array: `add_one_all<<< (N+255)/256, 256 >>>(d_data, N)` where the
kernel body is `int i = blockIdx.x * blockDim.x + threadIdx.x;
if (i < N) data[i] += 1.0f;`. Wall time drops by ~1000x.
