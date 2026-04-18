`formatReport` builds the result with `out += ...` in a loop. Strings in Go are immutable, so each `+=` allocates a new string of length `len(out) + len(piece)` and copies the old contents — O(N²) over the loop.

Fix: use `strings.Builder`:

```go
import "strings"

func formatReport(rows []Row) string {
    var b strings.Builder
    b.Grow(len(rows) * 32)   // optional but cuts another ~2x
    for _, r := range rows {
        b.WriteString(formatRow(r))
        b.WriteByte('\n')
    }
    return b.String()
}
```

Profile after the fix should drop the runtime to well under a second; `dbg hotspots` will move off `runtime.concatstrings` / `runtime.mallocgc` to `runtime.memmove` inside Builder writes.
