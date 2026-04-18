// Builds a CSV report from a slice of rows. Performance complaint:
// "the report endpoint takes ~30s for 50k rows; we expected
// hundreds of milliseconds." Reproduce with N=50000 below.
//
// The hot path is `formatReport` — but the actual culprit may be
// elsewhere. Profile, don't guess.
package main

import (
	"fmt"
	"strings"
	"strconv"
	"time"
)

type Row struct {
	ID    int
	Name  string
	Value float64
}

func makeRows(n int) []Row {
	rows := make([]Row, n)
	for i := range rows {
		rows[i] = Row{ID: i, Name: "row-" + strconv.Itoa(i), Value: float64(i) * 1.5}
	}
	return rows
}

func formatRow(r Row) string {
	return strconv.Itoa(r.ID) + "," + r.Name + "," + strconv.FormatFloat(r.Value, 'f', 2, 64)
}

func formatReport(rows []Row) string {
	var b strings.Builder
	b.Grow(len(rows) * 32)
	for _, r := range rows {
		b.WriteString(formatRow(r))
		b.WriteByte('\n')
	}
	return b.String()
}

func main() {
	const N = 50_000
	rows := makeRows(N)
	start := time.Now()
	report := formatReport(rows)
	elapsed := time.Since(start)
	fmt.Printf("formatted %d rows, %d bytes, in %s\n", N, len(report), elapsed)
}
