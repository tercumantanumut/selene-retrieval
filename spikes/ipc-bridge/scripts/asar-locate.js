// Read-only: maps byte offsets of marker strings in an app.asar to the archived file path.
// usage: node asar-locate.js <app.asar> <marker> [marker..]
const fs = require('fs');
const [, , asar, ...markers] = process.argv;
const buf = fs.readFileSync(asar);
const hsize = buf.readUInt32LE(12);
const header = JSON.parse(buf.slice(16, 16 + hsize).toString());
const base = 8 + buf.readUInt32LE(4);
const files = [];
(function walk(node, p) {
  for (const [k, v] of Object.entries(node.files || {})) {
    if (v.files) walk(v, p + '/' + k);
    else if (v.offset !== undefined) files.push({ path: p + '/' + k, start: base + +v.offset, end: base + +v.offset + v.size });
  }
})(header, '');
for (const m of markers) {
  let i = -1; const hits = {};
  while ((i = buf.indexOf(m, i + 1)) >= 0) {
    const f = files.find((f) => i >= f.start && i < f.end);
    const k = f ? f.path : '<unpacked/header>';
    hits[k] = (hits[k] || 0) + 1;
  }
  console.log(m, JSON.stringify(hits));
}
