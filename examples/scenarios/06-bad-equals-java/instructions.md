# Inventory dedupe undercount

We merge SKU streams from two upstream feeds into a `HashMap<Sku, Integer>` of occurrence counts. SKU `A-1001` appears once in feed1 and twice in feed2, so the count should be 3. We're getting 1.

```
$ javac -g Broken.java && java Broken
counts:
  A-1001 -> 1
  A-1001 -> 1
  A-1001 -> 1
BUG: expected 3 for A-1001, got null
```

Notice the output: there are *three* entries with the same string representation, and the lookup with a freshly-constructed probe key returns `null`. That's a clue but it's also a symptom — the underlying bug is upstream of the lookup.

Your task:
- Identify the bug. The fix is mechanical and well-known once you see what's happening.
- Convince yourself by *observing* what the HashMap is actually storing — in particular, what makes two `Sku` instances "the same" or "different" from the map's perspective.
- Don't change the test; fix the production class.

`solution.md` is a spoiler.
