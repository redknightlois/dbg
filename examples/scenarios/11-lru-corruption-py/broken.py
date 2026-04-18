"""LRU cache backed by a doubly-linked list + dict.

The cache works for small workloads. Under the workload below
(1000 ops, capacity 50, mixed get/put with realistic key reuse)
it starts returning stale values after a few hundred operations,
and the final consistency check fails.

The bug is in *one* of the four pointer-manipulation sites in
LruCache: _detach, _push_front, _evict_lru, or the call inside
get(). Each site, read in isolation, looks correct. The
corruption only becomes visible when you watch the list state
across many operations.
"""

from __future__ import annotations
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class Node:
    key: int
    val: int
    prev: Optional["Node"] = None
    next: Optional["Node"] = None


class LruCache:
    def __init__(self, capacity: int) -> None:
        self.capacity = capacity
        self.map: dict[int, Node] = {}
        # Sentinel head/tail to avoid None checks at the boundaries.
        self.head = Node(key=-1, val=-1)
        self.tail = Node(key=-1, val=-1)
        self.head.next = self.tail
        self.tail.prev = self.head

    def _detach(self, n: Node) -> None:
        # Splice n out of the list.
        assert n.prev is not None and n.next is not None
        n.prev.next = n.next
        n.next.prev = n.prev
        n.prev = None
        n.next = None

    def _push_front(self, n: Node) -> None:
        # Insert n right after head.
        n.prev = self.head
        n.next = self.head.next
        self.head.next.prev = n
        self.head.next = n

    def _evict_lru(self) -> Node:
        # Remove the node just before tail (oldest).
        victim = self.tail.prev
        assert victim is not None and victim is not self.head
        # Bug candidate region — looks fine.
        victim.prev.next = self.tail
        self.tail.prev = victim.prev
        victim.prev = None
        # NOTE: we leave victim.next pointing at tail. Callers shouldn't touch it.
        return victim

    def get(self, key: int) -> Optional[int]:
        n = self.map.get(key)
        if n is None:
            return None
        # Promote to front: detach + push.
        self._detach(n)
        self._push_front(n)
        return n.val

    def put(self, key: int, val: int) -> None:
        n = self.map.get(key)
        if n is not None:
            n.val = val
            self._detach(n)
            self._push_front(n)
            return
        if len(self.map) >= self.capacity:
            victim = self._evict_lru()
            del self.map[victim.key]
        n = Node(key=key, val=val)
        self.map[key] = n
        self._push_front(n)

    # Diagnostic helpers — useful when stopped in the debugger.
    def list_keys(self) -> list[int]:
        out = []
        cur = self.head.next
        while cur is not self.tail:
            assert cur is not None
            out.append(cur.key)
            cur = cur.next
        return out

    def list_keys_reverse(self) -> list[int]:
        out = []
        cur = self.tail.prev
        while cur is not self.head:
            assert cur is not None
            out.append(cur.key)
            cur = cur.prev
        return out


def workload() -> list[tuple[str, int, int]]:
    # Deterministic, reproducible mix of put + get with key reuse
    # in a Zipfian-ish pattern. ~1000 ops over a 200-key universe,
    # capacity 50.
    import random
    random.seed(7)
    ops: list[tuple[str, int, int]] = []
    universe = 200
    for _ in range(1000):
        # 60% puts, 40% gets.
        if random.random() < 0.6:
            k = random.randint(1, universe)
            v = random.randint(0, 1_000_000)
            ops.append(("put", k, v))
        else:
            k = random.randint(1, universe)
            ops.append(("get", k, 0))
    return ops


def reference_result(ops: list[tuple[str, int, int]], cap: int) -> dict[int, int]:
    """A trusted reference using OrderedDict — what we expect the
    cache contents to be after running the workload."""
    from collections import OrderedDict
    od: OrderedDict[int, int] = OrderedDict()
    for op, k, v in ops:
        if op == "put":
            if k in od:
                od.move_to_end(k, last=False)
                od[k] = v
            else:
                if len(od) >= cap:
                    od.popitem(last=True)  # evict LRU = back of OrderedDict
                od[k] = v
                od.move_to_end(k, last=False)
        else:  # get
            if k in od:
                od.move_to_end(k, last=False)
    return dict(od)


def main() -> None:
    ops = workload()
    cap = 50
    cache = LruCache(cap)
    expected = reference_result(ops, cap)

    for i, (op, k, v) in enumerate(ops):
        if op == "put":
            cache.put(k, v)
        else:
            cache.get(k)

        # Cheap structural invariant: forward and reverse traversals
        # of the linked list must yield the same multiset of keys.
        # If they diverge, the list is corrupt.
        fwd = cache.list_keys()
        rev = cache.list_keys_reverse()
        if sorted(fwd) != sorted(rev):
            raise AssertionError(
                f"after op #{i} ({op} {k}): forward and reverse keys disagree\n"
                f"  forward: {fwd}\n"
                f"  reverse: {rev}\n"
                f"  map size: {len(cache.map)}"
            )

    # Final content check against reference.
    actual = {n.key: n.val for n in cache.map.values()}
    if actual != expected:
        only_in_actual = sorted(set(actual) - set(expected))
        only_in_expected = sorted(set(expected) - set(actual))
        raise AssertionError(
            f"final cache contents diverge from reference:\n"
            f"  in actual but not expected: {only_in_actual}\n"
            f"  in expected but not actual: {only_in_expected}"
        )
    print(f"OK: {len(ops)} ops processed, final size {len(actual)}")


if __name__ == "__main__":
    main()
