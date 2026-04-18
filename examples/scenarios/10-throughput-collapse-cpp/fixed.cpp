// We process 10M items in batches. Throughput collapses past
// a certain batch size — going from 10K → 100K per batch makes
// the *total* runtime ~30x worse instead of giving us any benefit.
// Reproduce with the two batch sizes below.
//
// Build: c++ -g -O2 -std=c++17 broken.cpp -o broken
// Run:   ./broken

#include <chrono>
#include <cstdint>
#include <iostream>
#include <vector>

struct Item {
    std::uint64_t id;
    double payload[8]; // 64 bytes of data per item
};

// Pretend-work: touch every payload slot, accumulate.
double process_batch(const std::vector<Item>& batch) {
    double acc = 0.0;
    for (const auto& it : batch) {
        for (int k = 0; k < 8; k++) acc += it.payload[k] * 1.0001;
    }
    return acc;
}

void run(std::size_t total, std::size_t batch_sz) {
    auto t0 = std::chrono::steady_clock::now();
    double sink = 0.0;
    std::size_t done = 0;
    // Allocate once, reuse across batches: avoids per-batch malloc/mmap +
    // value-initialization zeroing (and the page faults that come with
    // fresh mmap-sized allocations at larger batch sizes).
    std::vector<Item> batch;
    batch.reserve(batch_sz);
    while (done < total) {
        std::size_t this_batch = std::min(batch_sz, total - done);
        batch.resize(this_batch);
        for (std::size_t i = 0; i < this_batch; i++) {
            batch[i].id = done + i;
            for (int k = 0; k < 8; k++) batch[i].payload[k] = (done + i) * 0.001;
        }
        sink += process_batch(batch);
        done += this_batch;
    }
    auto t1 = std::chrono::steady_clock::now();
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(t1 - t0).count();
    std::cout << "batch=" << batch_sz << " total=" << total
              << " time=" << ms << "ms sink=" << sink << "\n";
}

int main() {
    const std::size_t TOTAL = 10'000'000;
    run(TOTAL, 10'000);
    run(TOTAL, 100'000);
    return 0;
}
