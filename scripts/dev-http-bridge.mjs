#!/usr/bin/env node
// F-RCLONE UI 验证桥（dev tooling，不进生产路径）：
//   Vite dev 页面（真实前端，ESM 直出，绕开 dev-host 8MB 单资产限制）
//     └─ window.dbxPlugin（本进程注入的宿主页） ─HTTP→ 本桥 ─stdio 帧协议→ 真实 sidecar
// 用法：node scripts/dev-http-bridge.mjs [sidecarBin] [port] [viteOrigin]
// 环境变量 DBX_FILES_ENGINE 透传给 sidecar（默认 rclone）。
import { spawn } from "node:child_process";
import http from "node:http";
import crypto from "node:crypto";

const BIN = process.argv[2] ?? "../backend/target/debug/dbx-plugin-files";
const PORT = Number(process.argv[3] ?? 5199);
const VITE = process.argv[4] ?? "http://127.0.0.1:5173";
const ENGINE = process.env.DBX_FILES_ENGINE ?? "rclone";

const FRAME = 5;
let buf = Buffer.alloc(0);
const pending = new Map(); // id → {resolve, reject}
const sseClients = new Set();
const binaryInbox = []; // sidecar → host 二进制帧

function readFrames(chunk, onJson, onBinary) {
  buf = Buffer.concat([buf, chunk]);
  while (buf.length >= FRAME) {
    const kind = buf[0];
    const len = buf.readUInt32BE(1);
    if (buf.length < FRAME + len) break;
    const payload = buf.subarray(FRAME, FRAME + len);
    buf = buf.subarray(FRAME + len);
    if (kind === 0) onJson(JSON.parse(payload.toString("utf8")));
    else if (kind === 1) {
      const n = payload.readUInt16BE(0);
      const channel = payload.subarray(2, 2 + n).toString("utf8");
      onBinary(channel, payload.subarray(2 + n));
    }
  }
}

const sidecar = spawn(BIN, [], {
  env: { ...process.env, DBX_FILES_ENGINE: ENGINE },
  stdio: ["pipe", "pipe", "pipe"],
});
sidecar.stderr.on("data", (c) => process.stderr.write("[sidecar] " + c));
sidecar.on("exit", (code) => console.error("[bridge] sidecar exited", code));

function writeJson(obj) {
  const payload = Buffer.from(JSON.stringify(obj), "utf8");
  const head = Buffer.alloc(FRAME);
  head[0] = 0;
  head.writeUInt32BE(payload.length, 1);
  sidecar.stdin.write(Buffer.concat([head, payload]));
}

let nextId = 1;
function invoke(method, params, timeoutMs = 30000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error(`invoke ${method} timed out`));
      }
    }, timeoutMs);
    writeJson({ jsonrpc: "2.0", id, method, params: params ?? {} });
  });
}

sidecar.stdout.on("data", (chunk) =>
  readFrames(
    chunk,
    (msg) => {
      if (msg.method && !msg.id) {
        const event = { method: msg.method, params: msg.params ?? {} };
        for (const client of sseClients) client(event);
        return;
      }
      const p = pending.get(msg.id);
      if (p) {
        pending.delete(msg.id);
        if (msg.error) p.reject(Object.assign(new Error(msg.error.message ?? "sidecar error"), { payload: msg.error }));
        else p.resolve(msg.result);
      }
    },
    (channel, data) => {
      binaryInbox.push({ channel, base64: data.toString("base64") });
      if (binaryInbox.length > 64) binaryInbox.shift();
    },
  ),
);

// 握手 + 预连接：主连接（fixtures）与 __local__（rclone 引擎尚不自动合成，桥补偿）
await new Promise((resolve) => {
  const t = setTimeout(resolve, 800);
  const orig = pending.get.bind(pending);
  invoke("plugin/initialize", { host: { protocolVersions: [1] } }).then(() => {
    clearTimeout(t);
    resolve();
  });
});
for (const conn of [
  { id: "ui-conn", root: "/tmp/dbx-ui-fixtures" },
  { id: "__local__", root: "/" },
]) {
  try {
    await invoke("connection/connect", {
      provider: "io.dbx.files.connection",
      connection: { id: conn.id, name: conn.id, external_config: { protocol: "fs", root: conn.root } },
    });
    console.log(`[bridge] connected ${conn.id} -> ${conn.root}`);
  } catch (e) {
    console.error(`[bridge] connect ${conn.id} failed:`, e.message);
  }
}

const page = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"/><title>DBX Files (rclone dev bridge)</title>
<link rel="icon" href="data:," /></head><body><div id="app"></div>
<script>
const listeners = { event: [], binary: [] };
window.__ctx = { connectionId: "ui-conn", connection: { name: "Rclone UI Test", host: "local", readOnly: false, protocol: "fs" } };
async function __invoke(method, params) {
  const r = await fetch("/invoke", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ method, params }) });
  const j = await r.json();
  if (!j.ok) { const err = new Error((j.error && j.error.message) || "invoke failed"); err.payload = j.error; throw err; }
  return j.result;
}
const es = new EventSource("/events");
es.onmessage = (e) => { try { const d = JSON.parse(e.data); listeners.event.forEach((f) => f({ type: "event", method: d.method, params: d.params || {} })); } catch {} };
(async function poll() {
  try {
    const r = await fetch("/binary-poll");
    const j = await r.json();
    for (const f of j.frames || []) listeners.binary.forEach((cb) => cb({ channel: f.channel, dataBase64: f.base64 }));
  } catch {}
  setTimeout(poll, 400);
})();
function b64dec(v) { const bin = atob(v); const u = new Uint8Array(bin.length); for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i); return u; }
function b64enc(u) { let bin = ""; for (const c of u) bin += String.fromCharCode(c); return btoa(bin); }
window.dbxPlugin = {
  ready: Promise.resolve(window.__ctx),
  get context() { return window.__ctx; },
  locale: "zh-CN",
  request: async (method, params) => {
    if (method === "host.getContext") return window.__ctx;
    if (method === "host.listConnections") return [{ id: "ui-conn", name: "Rclone UI Test" }, { id: "__local__", name: "本地文件" }];
    return __invoke(method, params);
  },
  invoke: (method, params) => __invoke(method, params),
  notify: async (method, params) => { __invoke(method, params).catch(() => {}); },
  sendBinary: async (channel, data) => {
    const u = data instanceof Uint8Array ? data : new Uint8Array(data);
    await fetch("/binary", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ channel, base64: b64enc(u) }) });
  },
  onEvent: (f) => { listeners.event.push(f); return () => { const i = listeners.event.indexOf(f); if (i >= 0) listeners.event.splice(i, 1); }; },
  onBinary: (f) => { listeners.binary.push(f); return () => { const i = listeners.binary.indexOf(f); if (i >= 0) listeners.binary.splice(i, 1); }; },
  decodeBase64: b64dec,
  encodeBase64: b64enc,
};
</script>
<script type="module" src="${VITE}/src/main.ts"></script>
</body></html>`;

const server = http.createServer(async (req, res) => {
  const cors = {
    "Access-Control-Allow-Origin": "*",
    "Access-Control-Allow-Methods": "GET,POST,OPTIONS",
    "Access-Control-Allow-Headers": "content-type",
  };
  if (req.method === "OPTIONS") { res.writeHead(204, cors); return res.end(); }
  const url = new URL(req.url, "http://x");
  const body = await new Promise((resolve) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => resolve(data));
  });
  const send = (code, json) => { res.writeHead(code, { "content-type": "application/json", ...cors }); res.end(JSON.stringify(json)); };
  try {
    if (url.pathname === "/") { res.writeHead(200, { "content-type": "text/html; charset=utf-8", ...cors }); return res.end(page); }
    if (url.pathname === "/invoke") {
      const { method, params } = JSON.parse(body || "{}");
      const result = await invoke(method, params);
      return send(200, { ok: true, result });
    }
    if (url.pathname === "/binary") {
      const { channel, base64 } = JSON.parse(body || "{}");
      const data = Buffer.from(base64, "base64");
      const chan = Buffer.from(channel, "utf8");
      const payload = Buffer.concat([Buffer.from([chan.length >> 8, chan.length & 255]), chan, data]);
      const head = Buffer.alloc(FRAME);
      head[0] = 1;
      head.writeUInt32BE(payload.length, 1);
      sidecar.stdin.write(Buffer.concat([head, payload]));
      return send(200, { ok: true });
    }
    if (url.pathname === "/binary-poll") return send(200, { frames: binaryInbox.splice(0) });
    if (url.pathname === "/events") {
      res.writeHead(200, { "content-type": "text/event-stream", ...cors });
      const client = (msg) => res.write("data: " + JSON.stringify(msg) + "\n\n");
      sseClients.add(client);
      req.on("close", () => sseClients.delete(client));
      return;
    }
    send(404, { error: { message: "not found" } });
  } catch (error) {
    send(200, { ok: false, error: { code: -32000, message: String(error.message || error) } });
  }
});

server.listen(PORT, () => console.log(`[bridge] http://127.0.0.1:${PORT} (sidecar=${BIN}, engine=${ENGINE}, vite=${VITE})`));
