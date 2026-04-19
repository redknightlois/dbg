Node CPU profile shows `fs.readFileSync` + `JSON.parse` dominating
self-time — the blocker is `resolveConfig` running synchronously
on every job.

Cheapest fix: hoist the config read out of the hot loop — read
and parse once, pass the result into `handleJob`. If the config
really does need to be re-read per job (rare), switch to async
`fs.promises.readFile` and await it, letting the event loop
interleave other work.
