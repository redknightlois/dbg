pprof's top / flame view pins `regexp.Compile` (and
`regexp.MustCompile` called from `extractUserID`) at the top of
self-time — we're recompiling the same pattern 200k times. JSON
parsing isn't in the picture.

Fix: hoist `regexp.MustCompile` to a package-level `var` so the
pattern compiles once. Or replace with `strings.Index` + slicing —
for a fixed key like `"user_id":"…"` it's usually 3-5x faster than
a regex even once compilation is amortized.
