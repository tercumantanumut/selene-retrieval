// M0-D: an MCP client that launches sr-bridge exactly like node/Electron do
// (child_process.spawn, not detached => libuv puts the child in its global Job Object).
// Runs initialize -> tools/list -> tools/call ping -> tools/call progress (with progressToken).
// usage: node node-client.js <sr-bridge.exe> [SR_LAUNCH_ORDER]   env HOLD_MS=<ms before exit>
const { spawn } = require('child_process');
const bridge = process.argv[2];
const env = { ...process.env };
if (process.argv[3]) env.SR_LAUNCH_ORDER = process.argv[3];
const t0 = process.hrtime.bigint();
const ms = () => Number(process.hrtime.bigint() - t0) / 1e6;
const ch = spawn(bridge, ['--client', 'node'], { env, stdio: ['pipe', 'pipe', 'inherit'] });
let buf = '', id = 0;
const pending = new Map();
const notes = [];
ch.stdout.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const m = JSON.parse(buf.slice(0, i));
    buf = buf.slice(i + 1);
    if (m.id !== undefined && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } else notes.push(m);
  }
});
const req = (method, params) => new Promise((r) => {
  id++; pending.set(id, r);
  ch.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
});
const timer = setTimeout(() => { console.log(JSON.stringify({ error: 'timeout 15s' })); process.exit(2); }, 15000);
(async () => {
  const init = await req('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'node-libuv', version: '0' } });
  const tInit = ms();
  ch.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
  const list = await req('tools/list', {});
  const tList = ms();
  const p = await req('tools/call', { name: 'ping', arguments: {} });
  const tPing = ms();
  const pr = await req('tools/call', { name: 'progress', arguments: { steps: 3 }, _meta: { progressToken: 'n1' } });
  clearTimeout(timer);
  console.log(JSON.stringify({
    node_pid: process.pid, bridge_pid: ch.pid,
    negotiated: init.result.protocolVersion, server: init.result.serverInfo,
    tools: list.result.tools.map((t) => t.name),
    ping: JSON.parse(p.result.content[0].text),
    progress_result: pr.result.content[0].text,
    progress_notifications: notes.filter((n) => n.method === 'notifications/progress').map((n) => `${n.params.progress}/${n.params.total}`),
    ms: { initialize: +tInit.toFixed(1), tools_list: +tList.toFixed(1), ping: +tPing.toFixed(1) },
  }));
  setTimeout(() => process.exit(0), +(process.env.HOLD_MS || 0)); // exit without killing the child
})();
