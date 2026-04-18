The odd branch makes **two** recursive calls and adds them:

```python
return collatz_steps(nxt, depth + 1) + collatz_steps(nxt // 2, depth + 1)
```

That's a binary recursion — every odd `n` doubles the number of calls. For Collatz, odd values appear roughly half the time, so the call count grows exponentially even though each individual call descends quickly. That's why `dbg hits` shows orders of magnitude more invocations than actual steps.

Fix: drop the second recursive call. The "pre-collapsing one step" comment is wrong — `3n+1` is always even (since `n` is odd), so the next step is always to halve, but you should let the *next* recursive call do that, not branch into both:

```python
else:
    return collatz_steps(3 * n + 1, depth + 1)
```

Or, if you want to keep the even-step inline as an optimization, replace `+` with single-call substitution:

```python
else:
    nxt = (3 * n + 1) // 2     # one combined odd+even step
    return collatz_steps(nxt, depth + 2)
```

After the fix, `dbg hits` for `n=15` should show 18 calls (17 steps + initial), not several thousand.
