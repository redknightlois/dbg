// A kernel that "fuses" normalization + element-wise transform +
// reduction. Looks compute-heavy but is actually memory-bandwidth
// bound — each output pixel reads 3 floats from global memory and
// writes 1; the arithmetic is 2 ops. ncu will flag a very low
// achieved compute throughput against a high achieved DRAM
// throughput, and the memory workload analyzer will show L1/L2
// hit rate near zero on the read pattern.
//
// The "fix" for scenario purposes is recognizing the kernel is
// bandwidth-bound (expected ~DRAM peak), not buggy compute. The
// actual improvement would be to fuse upstream passes so these
// reads are reused, cutting the global traffic.

#include <cuda_runtime.h>
#include <cstdio>

__global__ void normalize(float *out, const float *mean, const float *scale,
                          const float *x, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    // 3 loads + 1 store per element; 2 FLOPs. Ratio is byte-bound.
    out[i] = (x[i] - mean[i]) * scale[i];
}

int main() {
    const int N = 64 * 1024 * 1024;  // 256 MB of floats per buffer
    float *d_x, *d_mean, *d_scale, *d_out;
    cudaMalloc(&d_x, N * sizeof(float));
    cudaMalloc(&d_mean, N * sizeof(float));
    cudaMalloc(&d_scale, N * sizeof(float));
    cudaMalloc(&d_out, N * sizeof(float));
    cudaMemset(d_x, 0, N * sizeof(float));
    cudaMemset(d_mean, 0, N * sizeof(float));
    cudaMemset(d_scale, 0, N * sizeof(float));

    int threads = 256;
    int blocks = (N + threads - 1) / threads;
    for (int i = 0; i < 50; i++) {
        normalize<<<blocks, threads>>>(d_out, d_mean, d_scale, d_x, N);
    }
    cudaDeviceSynchronize();

    std::printf("ran normalize 50x over %d elements\n", N);
    cudaFree(d_x);
    cudaFree(d_mean);
    cudaFree(d_scale);
    cudaFree(d_out);
    return 0;
}
