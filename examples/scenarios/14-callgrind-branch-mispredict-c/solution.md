Callgrind shows `classify` dominating instruction count; the Bc/Bcm
ratio on the inner `if` is close to 50 %, i.e. every other sample is
a mispredict because the data is random.

Two fixes:
- Branchless accumulation: `sum += data[i] * (data[i] >= lo && data[i] <= hi)` — compiler turns the mask into a cmov.
- Sort `data` before classifying — branch predictor hits ~100 % after sort; costs one O(n log n) pass but wins if classify runs many times.
