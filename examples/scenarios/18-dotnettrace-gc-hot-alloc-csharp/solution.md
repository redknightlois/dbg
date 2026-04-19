`dotnet-trace`'s GC events show gen0 collections roughly every
few thousand iterations; `hotspots --by alloc` (or equivalent)
pins `BuildKey` and especially `string.Join` / `int.ToString` /
`string.Replace` as the allocators.

Fix: rewrite `BuildKey` with a stack-allocated span + `TryFormat`
for the integers, or use `string.Create` with a length computed
up-front. For the `/` → `_` transformation, either pre-sanitize
the path once outside the loop or use a `Span<char>` overwrite
instead of `string.Replace`. `method.ToUpper()` → precompute once.
