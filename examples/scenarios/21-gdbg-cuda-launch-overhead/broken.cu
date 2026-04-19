// We launch a tiny "add one" kernel once per element instead of
// once per array. Each launch is ~5-10 µs, so the wall time is
// dominated by launch overhead, not the work. nsys timeline makes
// this obvious at a glance; ncu shows each kernel is nearly empty.
//
// Build: nvcc -O2 -lineinfo -o broken broken.cu
// Run:   ./broken

#include <cuda_runtime.h>
#include <cstdio>

__global__ void add_one(float *data, int idx) {
    // "Work" per launch: a single thread touching a single element.
    if (threadIdx.x == 0 && blockIdx.x == 0) {
        data[idx] += 1.0f;
    }
}

int main() {
    const int N = 100000;
    float *d_data;
    cudaMalloc(&d_data, N * sizeof(float));
    cudaMemset(d_data, 0, N * sizeof(float));

    // Anti-pattern: one kernel launch per element.
    for (int i = 0; i < N; i++) {
        add_one<<<1, 32>>>(d_data, i);
    }
    cudaDeviceSynchronize();

    float h0;
    cudaMemcpy(&h0, d_data, sizeof(float), cudaMemcpyDeviceToHost);
    std::printf("data[0] = %.1f (expected 1.0)\n", h0);
    cudaFree(d_data);
    return 0;
}
