# `FlatLongIntMap.Slot` — where does it actually live?

`FlatLongIntMap.Slot(key)` is a tiny helper (`(int)(key & _mask)`) on the hot path. Every `TryGetValue` / `Put` calls it. We want instruction-level proof that:

1. `Slot`'s three operations (`mov` key into a reg, `and` with `_mask`, use as index) really do get emitted, and are not being skipped.
2. The `_keys[i]` access that follows does **not** emit a bounds check (`CORINFO_HELP_RNGCHKFAIL`). The `& _mask` with a power-of-two capacity should let the JIT prove the index is in range.

Your task: produce that evidence from the actual JIT disassembly.

```
dotnet build -c Release
dbg start jitdasm Broken.csproj --args 'Broken.*' --capture-duration 20s
```

Then inside the session:

```
dbg disasm FlatLongIntMap:Slot
```

You will notice something immediately. Work through it, then produce the evidence.

Constraints:
- Timing numbers don't count. We want the actual instructions.
- If you conclude "the method isn't there, can't check", you haven't finished.
- When you're done, `dbg kill` before moving on — don't leak the session.

`solution.md` is a spoiler.
