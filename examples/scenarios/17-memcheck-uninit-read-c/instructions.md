# Intermittent "debug logging" turns on in prod without a flag change

We're seeing sporadic "debug logging enabled" messages in service
logs with no matching config change — it flips on and off across
restarts on the same box. The config file doesn't set any debug
flag anywhere.

Reproduce:

```
cd examples/scenarios/17-memcheck-uninit-read-c
make
./broken       # runs clean most of the time
```

The binary exits 0 and the output looks fine — but we know
something is reading non-deterministic state. Run it under a
memory checker (memcheck) and report what it flags. I'd like the
exact line numbers of any uninitialized-value reads so I can patch
them deterministically.
