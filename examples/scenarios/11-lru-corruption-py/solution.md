`_detach` has its lines in the wrong order:

```python
def _detach(self, n: Node) -> None:
    assert n.prev is not None and n.next is not None
    n.prev.next = n.next
    n.prev = None                     # ← clears n.prev *before* it's needed
    n.next.prev = n.prev              # ← reads n.prev, gets None
    n.next = None
```

`n.next.prev = n.prev` runs *after* `n.prev = None`, so it sets the successor's back-pointer to `None` instead of to the predecessor. Forward traversal still skips `n` correctly (because `n.prev.next = n.next` ran first), so the corruption is invisible from the head side — that's why every individual line "looks correct" and why the unit tests on small inputs pass. The reverse traversal hits the orphaned `None` link and crashes.

Fix: swap the two middle lines.

```python
n.prev.next = n.next
n.next.prev = n.prev    # use n.prev *before* clearing it
n.prev = None
n.next = None
```

Why this is interesting from a tooling standpoint: every line of the buggy `_detach` is independently correct. The bug is in the *order*. Pure source-reading tends to scan each line and tick it off — `n.prev.next = n.next` ✓, `n.prev = None` ✓, `n.next.prev = n.prev` ✓ — and miss the cross-line dependency. Setting a breakpoint at the end of `_detach` and inspecting `n.next.prev` shows the bug in one observation.
