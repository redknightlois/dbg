Line 21: `return a.priority < b.priority;` should be `return a.priority > b.priority;`.

Convention: `std::sort` calls the comparator as "is `a` strictly *before* `b` in the desired order?". For a descending priority sort, higher-priority items must come first, so `a.priority > b.priority` returns true when `a` is more urgent than `b`.

`dbg locals` at the breakpoint shows e.g. `a={priority=1}, b={priority=5}` returning `true` from `order_before` — the immediate red flag.
