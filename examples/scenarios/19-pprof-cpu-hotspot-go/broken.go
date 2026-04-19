// A JSON-lines log processor that extracts the `user_id` field from
// each line. The user-facing symptom: ingest throughput is 10x
// slower than `wc -l` on the same file. The team assumes parsing
// JSON is the cost. A CPU profile (pprof) should tell them otherwise —
// the real hotspot is `regexp.MatchString` recompiling the pattern
// on every call.

package main

import (
	"fmt"
	"regexp"
	"time"
)

// sample lines — real input would be a file; this keeps the scenario
// self-contained.
func lines(n int) []string {
	out := make([]string, n)
	for i := 0; i < n; i++ {
		out[i] = fmt.Sprintf(`{"ts":%d,"user_id":"u%d","event":"click"}`, i, i%1000)
	}
	return out
}

func extractUserID(line string) string {
	// BUG: compiles the pattern on every call.
	matched, _ := regexp.MatchString(`"user_id":"([^"]+)"`, line)
	if !matched {
		return ""
	}
	re := regexp.MustCompile(`"user_id":"([^"]+)"`)
	m := re.FindStringSubmatch(line)
	return m[1]
}

func main() {
	ls := lines(200_000)
	t0 := time.Now()
	uniq := make(map[string]int)
	for _, l := range ls {
		uniq[extractUserID(l)]++
	}
	fmt.Printf("%d unique user_ids in %s\n", len(uniq), time.Since(t0))
}
