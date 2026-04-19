/* A classifier that labels samples as "in-range" or "out-of-range"
 * before a downstream stage accumulates them. The input is
 * deliberately unsorted, and the if-inside-loop is the bottleneck —
 * every sample is a branch-mispredict, so IPC tanks.
 *
 * Profile with callgrind to see Dr/Dw rates and branch-mispredict
 * counters; the `classify` function dominates instruction count. */

#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#define N (1 << 22)  /* 4M samples */

static long classify(const int *data, int n, int lo, int hi) {
    long sum = 0;
    for (int i = 0; i < n; i++) {
        /* Unpredictable branch on random data — the hotspot. */
        if (data[i] >= lo && data[i] <= hi) {
            sum += data[i];
        }
    }
    return sum;
}

int main(void) {
    int *data = malloc(sizeof(int) * N);
    srand(42);
    for (int i = 0; i < N; i++) {
        data[i] = rand() % 1000;
    }
    long total = classify(data, N, 250, 750);
    printf("sum in [250,750] = %ld\n", total);
    free(data);
    return 0;
}
