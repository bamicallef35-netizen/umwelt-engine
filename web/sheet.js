// Draw the Hyde Park replay as a sheet:  node web/sheet.js
// Runs the trace through docs/umwelt.wasm (the no_std core, unchanged) and
// draws every emission over the four channels — icons on their lanes,
// indices as ties between lanes, symbols as squares — with a scale bar and
// a title block (programme, area, procedure, record). Writes docs/replay.svg.
import { readFileSync, writeFileSync } from "node:fs";
const dir = new URL("../docs/", import.meta.url);
const bytes = readFileSync(new URL("umwelt.wasm", dir));
const { instance } = await WebAssembly.instantiate(bytes);
const w = instance.exports;
const lines = readFileSync(new URL("terrain_trace.ndjson", dir), "utf8").split("\n").map((l) => l.trim()).filter(Boolean);
if (lines[0] !== "umwelt.sensor_log.v1") throw new Error("log header mismatch");
const samples = lines.slice(1).map((l) => JSON.parse(l));
w.init();
const series = [[], [], [], []], events = [];
for (const s of samples) {
  const r = w.observe(s.channel, s.q15, Number(BigInt(s.tick_us) & 0xffffffffn), Number(BigInt(s.tick_us) >> 32n));
  if (r === 0xffffffff) throw new Error("pipeline refused a sample");
  series[s.channel].push([s.tick_us, s.q15]);
  const n = w.events_len(), v = new Uint32Array(w.memory.buffer, w.events_ptr(), n * 8);
  for (let i = 0; i < n; i++) events.push({ kind: v[i * 8], a: v[i * 8 + 1], b: v[i * 8 + 2], t: v[i * 8 + 3] + v[i * 8 + 4] * 2 ** 32, q: v[i * 8 + 5] / 32767 });
  w.events_clear();
}
const INK = "#111111", INK2 = "#6b6b6b", RULE = "#d4d4d4", ACCENT = "#0000ee", FONT = "IBM Plex Mono, ui-monospace, Menlo, monospace";
const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;");
const text = (x, y, s, size = 11, fill = INK, anchor = "start") => `<text x="${x}" y="${y}" font-family="${FONT}" font-size="${size}" fill="${fill}" text-anchor="${anchor}">${esc(s)}</text>`;
const t0 = samples[0].tick_us, t1 = samples[samples.length - 1].tick_us, span = t1 - t0;
const pad = 48, labelW = 40, laneH = 64, lanes = 4, pxPerS = 40, gw = (span / 1e6) * pxPerS, gh = lanes * laneH, block = 130;
const W = gw + pad * 2 + labelW, H = gh + pad * 2 + 30 + block, x0 = pad + labelW;
const X = (t) => x0 + ((t - t0) / span) * gw, Y = (lane, q) => pad + laneH * (lane + 0.5) - (q / 32767) * laneH * 0.42, yc = (lane) => pad + laneH * (lane + 0.5);
let body = `<rect x="${x0 + 0.5}" y="${pad + 0.5}" width="${gw.toFixed(1)}" height="${gh}" fill="none" stroke="${RULE}"/>`;
body += text(pad, pad - 14, "replay / the Hyde Park terrain trace through umwelt-core, compiled to WebAssembly", 10, INK2);
body += text(x0 + gw, pad - 14, "benmicallef / umwelt-engine / drawn by the module's own emissions", 10, INK2, "end");
series.forEach((s, l) => {
  body += text(pad, yc(l) + 3, "ch" + l, 9, INK2);
  body += `<line x1="${x0}" y1="${yc(l)}" x2="${(x0 + gw).toFixed(1)}" y2="${yc(l)}" stroke="${RULE}"/>`;
  body += `<path d="${s.map(([t, q], i) => `${i ? "L" : "M"}${X(t).toFixed(1)} ${Y(l, q).toFixed(1)}`).join("")}" fill="none" stroke="${INK}" stroke-width="0.8"/>`;
});
const labelled = {};
for (const e of events) {
  const x = X(e.t).toFixed(1);
  if (e.kind === 0) body += `<circle cx="${x}" cy="${yc(e.a)}" r="3" fill="${INK}"/>`;
  else if (e.kind === 1) body += `<line x1="${x}" y1="${yc(e.a)}" x2="${x}" y2="${yc(e.b)}" stroke="${ACCENT}" stroke-width="${(1 + e.q * 1.5).toFixed(1)}"/>`;
  else {
    const y = pad + laneH * ((e.a + e.b) / 2 + 0.5), key = e.a + "·" + e.b;
    body += `<rect x="${(Number(x) - 4).toFixed(1)}" y="${y - 4}" width="8" height="8" fill="${ACCENT}"/>`;
    if (!(key in labelled) || e.t - labelled[key] > 500000) { labelled[key] = e.t; body += text(Number(x) + 7, y - 7, "symbol " + key, 9, INK); }
  }
}
const sbY = pad + gh + 22;
body += `<line x1="${x0}" y1="${sbY}" x2="${x0 + pxPerS}" y2="${sbY}" stroke="${INK}" stroke-width="2"/>`;
body += text(x0 + pxPerS + 8, sbY + 4, "1 s", 10, INK2);
const counts = [0, 1, 2].map((k) => events.filter((e) => e.kind === k).length), first = events[0];
const rows = [
  ["project", "umwelt-engine — one replay, drawn from the module's emissions"],
  ["programme", `${samples.length} samples on ${lanes} channels over ${(span / 1e6).toFixed(0)} s, fed one at a time to the no_std core compiled to WebAssembly (${bytes.byteLength.toLocaleString("en-GB")} bytes), profile <4, 64, 8>`],
  ["area", `${(span / 1e6).toFixed(0)} s; a 64-sample icon window with 8 histogram bins per channel`],
  ["procedure", "a sample that passes the activity gate of its channel's window forms an icon; two icons close in time form an index with the coherence of the pair; a pair whose indices recur enters the vocabulary as a symbol"],
  ["record", `icons ${counts[0]} / indices ${counts[1]} / symbols ${counts[2]} / first emission at ${(first.t / 1e6).toFixed(3)} s on ch${first.a} / the Rust example reports the same counts`],
];
const ty = pad + gh + 54, maxChars = Math.floor((gw + labelW - 90) / 6);
body += `<line x1="${pad}" y1="${ty - 14}" x2="${(x0 + gw).toFixed(1)}" y2="${ty - 14}" stroke="${RULE}"/>`;
let y = ty;
for (const [k, v] of rows) {
  const ls = []; let cur = "";
  for (const word of v.split(" ")) { if (cur && (cur + " " + word).length > maxChars) { ls.push(cur); cur = word; } else cur = cur ? cur + " " + word : word; }
  if (cur) ls.push(cur);
  body += text(pad, y, k, 10, INK2); ls.forEach((line, i) => { body += text(pad + 90, y + i * 13, line, 10, INK); }); y += ls.length * 13 + 5;
}
const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W.toFixed(0)} ${H}" width="${W.toFixed(0)}" height="${H}" role="img" aria-label="the Hyde Park replay: icons, indices and symbols over four channels">
<rect width="${W.toFixed(0)}" height="${H}" fill="#ffffff"/>
<rect x="0.5" y="0.5" width="${(W - 1).toFixed(0)}" height="${H - 1}" fill="none" stroke="${RULE}"/>
${body}
</svg>
`;
writeFileSync(new URL("replay.svg", dir), svg);
console.error(`wrote docs/replay.svg — ${samples.length} samples, ${events.length} events, ${W.toFixed(0)}×${H}`);
