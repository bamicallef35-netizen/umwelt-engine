// Replay the Hyde Park trace through the WebAssembly build and print the
// same counts the Rust example prints:  node web/check.js
import { readFileSync } from "node:fs";
const dir = new URL("../docs/", import.meta.url);
const { instance } = await WebAssembly.instantiate(readFileSync(new URL("umwelt.wasm", dir)));
const w = instance.exports;
const lines = readFileSync(new URL("terrain_trace.ndjson", dir), "utf8").split("\n").map((l) => l.trim()).filter(Boolean);
if (lines[0] !== "umwelt.sensor_log.v1") throw new Error("log header mismatch");
w.init();
const counts = [0, 0, 0], first = [], perChannel = new Map(), pairs = new Map();
for (const line of lines.slice(1)) {
  const s = JSON.parse(line);
  const r = w.observe(s.channel, s.q15, Number(BigInt(s.tick_us) & 0xffffffffn), Number(BigInt(s.tick_us) >> 32n));
  if (r === 0xffffffff) throw new Error("pipeline refused a sample");
  const n = w.events_len(), v = new Uint32Array(w.memory.buffer, w.events_ptr(), n * 8);
  for (let i = 0; i < n; i++) {
    const kind = v[i * 8], a = v[i * 8 + 1], b = v[i * 8 + 2], t = v[i * 8 + 3] + v[i * 8 + 4] * 2 ** 32;
    counts[kind]++;
    if (kind === 0) perChannel.set(a, (perChannel.get(a) || 0) + 1);
    if (kind === 1) { const k = a + "," + b; pairs.set(k, (pairs.get(k) || 0) + 1); }
    if (first.length < 5) first.push(`${["icon", "index", "symbol"][kind]} ch${a} ${t} us`);
  }
  w.events_clear();
}
console.log(`samples ${lines.length - 1} / icons ${counts[0]} / indices ${counts[1]} / symbols ${counts[2]} / dropped ${w.events_dropped()}`);
console.log("icons per channel:", [...perChannel].sort((x, y) => x[0] - y[0]).map(([c, n]) => `ch${c}=${n}`).join(" "));
console.log("index pairs:", [...pairs].map(([k, n]) => `(${k})=${n}`).join(" "));
console.log("first five:", first.join(" | "));
