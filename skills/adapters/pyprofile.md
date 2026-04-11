# Python cProfile Adapter

## CLI

Profile a script:
```bash
python3 -m cProfile -o /tmp/dbg-profile.prof script.py
dbg start pyprofile /tmp/dbg-profile.prof
```

Or open an existing profile:
```bash
dbg start pyprofile existing.prof
```

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `python3` | `which python3` | `sudo apt install python3` |

cProfile and pstats are stdlib — no pip install needed.

## Generating Profiles

```bash
# Profile a script
python3 -m cProfile -o profile.prof script.py

# Profile with sorting (direct output, no .prof)
python3 -m cProfile -s cumulative script.py

# Profile from code
python3 -c "import cProfile; cProfile.run('my_function()', 'profile.prof')"
```

## Key Commands (pstats interactive)

| Command | What it does |
|---------|-------------|
| `sort cumulative` | Sort by cumulative time (includes callees) |
| `sort tottime` | Sort by time spent in function itself |
| `sort calls` | Sort by number of calls |
| `stats 20` | Show top 20 functions |
| `stats <pattern>` | Show functions matching regex |
| `callers <func>` | Who calls this function |
| `callees <func>` | What does this function call |
| `strip` | Remove directory paths for cleaner output |
| `reverse` | Reverse sort order |

## Workflow

1. Generate profile: `python3 -m cProfile -o /tmp/dbg-profile.prof script.py`
2. Open: `dbg start pyprofile /tmp/dbg-profile.prof`
3. Overview: `sort cumulative` then `stats 20`
4. Find bottleneck: `stats <pattern>` for specific module/function
5. Understand call chain: `callers <hot_func>` and `callees <hot_func>`

## Common Failures

| Symptom | Fix |
|---------|-----|
| Everything in C extensions | cProfile can't see inside C code — use py-spy instead |
| `profile.prof` too small | Script ran too fast — profile a longer workload |
| No function names | Decorated/lambda functions show as `<lambda>` — add named functions |
