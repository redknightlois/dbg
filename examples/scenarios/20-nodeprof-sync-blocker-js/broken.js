// A tiny HTTP-like handler simulation: process N jobs in a loop,
// each one calls "resolveConfig" which synchronously reads a file
// and parses it — blocking the event loop. The caller awaits each
// job in sequence, so wall time is N * fs-read. Under node's CPU
// profiler you'll see fs.readFileSync / JSON.parse dominating
// self-time; the right fix is async fs or caching the parsed config
// above the hot loop.

'use strict';
const fs = require('fs');
const path = require('path');
const os = require('os');

// Write a small JSON "config" into a tmp file once — so the scenario
// stays self-contained and doesn't require a pre-baked file.
const cfgPath = path.join(os.tmpdir(), 'broken-cfg.json');
fs.writeFileSync(cfgPath, JSON.stringify({ a: 1, b: 2, c: [1, 2, 3, 4, 5] }));

function resolveConfig() {
  // BUG: sync fs + sync parse on the hot path.
  const raw = fs.readFileSync(cfgPath, 'utf8');
  return JSON.parse(raw);
}

function handleJob(i) {
  const cfg = resolveConfig();
  return i + cfg.a + cfg.b + cfg.c.length;
}

function run(n) {
  const t0 = Date.now();
  let acc = 0;
  for (let i = 0; i < n; i++) {
    acc += handleJob(i);
  }
  console.log(`processed ${n} jobs -> ${acc} in ${Date.now() - t0} ms`);
}

run(20000);
