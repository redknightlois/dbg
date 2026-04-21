# Solution

## The trap

`dbg disasm FlatLongIntMap:Slot` returns no standalone body — `Slot` is a three-instruction private helper marked `[MethodImpl(MethodImplOptions.AggressiveInlining)]`, so the JIT inlined it at every call site. It has no `; Assembly listing for method …` header of its own. An agent that reads the no-standalone-body message and concludes "the method is missing, I can't inspect it" has misread the situation; the code absolutely runs, just not as its own listing.

## What dbg does for you

The jitdasm backend builds a call graph of the capture at parse time. When `disasm <inlined>` misses, it falls back to printing the **callers'** full disasm — the inlined body is embedded inside each caller. The broad capture filter (`Broken.*`) is what makes this possible: it includes the standalone listings of the *callers* (`Put`, `TryGetValue`, `LookupBatch`, `LookupOne`), so the call graph has parents to show. A narrow filter that names only the inlined target captures nothing useful — neither the target (inlined away) nor its parents (excluded by filter).

Output looks roughly like:

```
── `FlatLongIntMap:Slot` has no standalone body — inlined at every call site.
   Showing N caller listing(s); the inlined body is embedded in each. ──

════════ parent: Broken.FlatLongIntMap:TryGetValue(...) ════════
...
════════ parent: Broken.FlatLongIntMap:Put(...) ════════
...
```

## The evidence you need to produce

Inside any caller's listing, the inlined `Slot` appears as two instructions right where the call site would have been:

```
mov      <reg>, <key-reg>          ; copy key
and      <reg>, <mask-reg>         ; i = key & _mask
```

Followed immediately by the `_keys[i]` access (inside `TryGetValue`'s probe loop):

```
mov      <reg>, qword ptr [<keys-ptr> + <i>*8]   ; k = _keys[i]
```

with **no `call CORINFO_HELP_RNGCHKFAIL`** anywhere near it — that is the bounds-check elision you were asked to confirm. The `& _mask` produces an index the JIT can prove is `< _keys.Length`, so it skips the check.

The two things the task asks for:

1. **No `call CORINFO_HELP_RNGCHKFAIL` in the probe sequence.** The `& _mask` with a power-of-two `_keys.Length` lets the JIT prove `i < _keys.Length`, so the bounds check is elided. If you see a `RNGCHKFAIL` call here, the optimization failed — investigate.
2. **The loop is a mask-and-linear-probe.** A `jmp` back up to the load, `add`/`and` incrementing the index — no fancy unrolling, no vectorization (correct: the probe is data-dependent).

## Close the session

```
dbg kill
```

Leaving the daemon running holds the capture file and subprocess open. Always close before moving on.
