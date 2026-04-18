// Compute summary stats for a sliding window over a time series.
// We compute mean, min, max for every window of size W. The mean
// is correct, min looks fine, but max is sometimes wrong — it
// returns a value smaller than other values in the window.
package main

import (
	"fmt"
	"math/rand"
	"os"
)

type Window struct {
	mean float64
	min  float64
	max  float64
}

func summarize(data []float64, w int) []Window {
	out := make([]Window, 0, len(data)-w+1)
	for i := 0; i+w <= len(data); i++ {
		sum := 0.0
		min := data[i]
		max := data[i]
		for j := i; j < i+w; j++ {
			sum += data[j]
			if data[j] < min {
				min = data[j]
			}
			if data[j] > max {
				max = data[j]
			}
		}
		out = append(out, Window{
			mean: sum / float64(w),
			min:  min,
			max:  max,
		})
	}
	return out
}

func main() {
	rand.Seed(42)
	data := make([]float64, 200)
	for i := range data {
		data[i] = rand.Float64() * 100
	}

	windows := summarize(data, 10)

	// Sanity: max must be >= every value in its window.
	failures := 0
	for i, w := range windows {
		for j := i; j < i+10; j++ {
			if data[j] > w.max {
				failures++
				if failures <= 3 {
					fmt.Fprintf(os.Stderr,
						"window %d: data[%d]=%.3f exceeds reported max=%.3f\n",
						i, j, data[j], w.max)
				}
			}
		}
	}
	if failures > 0 {
		fmt.Fprintf(os.Stderr, "BUG: %d invariant violations across %d windows\n",
			failures, len(windows))
		os.Exit(1)
	}
	fmt.Printf("OK: %d windows, all invariants hold\n", len(windows))
}
