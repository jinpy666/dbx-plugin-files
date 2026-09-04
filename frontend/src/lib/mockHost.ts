// 开发/验证专用 mock 宿主桥（P-FILES ④）：URL 带 ?mock=1 且宿主桥缺失时启用。
// 用途：浏览器内验证 UI 三态（加载/空/错误）、大目录虚拟滚动、transport=job
// 的 copy/move/rename 终态刷新与传输面板——不进生产路径（动态 import 单独分包）。
// 行为开关（query 参数）：
//   ?mock=1          启用
//   &locale=zh-CN    宿主 locale（默认 zh-CN）
//   &theme=light     宿主 1.1 theme 通道方案（默认 dark，与真实宿主一致）
//   &delay=300       files/list 人为延迟 ms（便于观察加载态）
//   &job=1           copy/move 一律走降级 job（默认仅目录/`mockDir`）
// 任何包含 "error" 的路径都会返回业务错误（便于验证错误横幅与重试）。
// __local__ 连接（双栏左栏本地面）：list/listPaged/stat/quickPaths/read 路由到
// 独立本地树（$HOME 家族 quickPaths）；写路径（mkdir/delete/copy/move 等）仍落
// 远端 mock 树——浏览器验证以「左=本地、右=远端」浏览/预览为主，写面由 sidecar
// 真实实现（cargo 单测 + smoke）覆盖。

type MockEntry = { kind: "file" | "dir"; size: number; modifiedAt: string };

const CHUNK = 256 * 1024;

function b64encode(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

function b64decode(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function concatBytes(chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

export function installMockHost(): void {
  if (window.dbxPlugin) return;
  const params = new URLSearchParams(window.location.search);
  const delayMs = Number(params.get("delay") ?? "250");
  const forceJob = params.get("job") === "1";

  // ---- 虚拟文件树 ---------------------------------------------------------
  const tree = new Map<string, MockEntry>();
  // A-FILES ②：文件内容（files/read / files/write 用）
  const contents = new Map<string, Uint8Array>();
  const now = Date.now();
  const stamp = (offsetDays: number) => new Date(now - offsetDays * 86_400_000).toISOString();
  const put = (path: string, kind: "file" | "dir", size = 0) =>
    tree.set(path.replace(/\/+$/, "") || "/", { kind, size, modifiedAt: stamp(tree.size % 30) });
  const putText = (path: string, text: string) => {
    const bytes = new TextEncoder().encode(text);
    contents.set(path.replace(/\/+$/, ""), bytes);
    put(path, "file", bytes.byteLength);
  };

  put("/", "dir");
  put("/docs", "dir");
  put("/empty", "dir");
  put("/10k", "dir");
  // 快速目录样例（files/quickPaths mock 段按存在才透出，与真实 fs 侧行为对齐）
  put("/desktop", "dir");
  put("/downloads", "dir");
  put("/documents", "dir");
  put("/pictures", "dir");
  putText("/docs/readme.md", "# DBX Files\n\n双栏文件浏览（A-FILES）验证样例。\n\n- 左栏：源（当前连接）\n- 右栏：目标面板 / 预览\n\n编辑此文件并保存会走 files/write。\n");
  putText("/docs/notes.txt", "line 1\nline 2\nline 3\n");
  {
    // 1x1 PNG（图片预览 / data URI）
    const png = window.atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    const bytes = new Uint8Array(png.length);
    for (let i = 0; i < png.length; i += 1) bytes[i] = png.charCodeAt(i);
    contents.set("/docs/logo.png", bytes);
    put("/docs/logo.png", "file", bytes.byteLength);
  }
  {
    // 二进制样例（hex dump 预览）
    const bytes = new Uint8Array(1024);
    for (let i = 0; i < bytes.length; i += 1) bytes[i] = (i * 31 + 7) % 256;
    contents.set("/docs/data.bin", bytes);
    put("/docs/data.bin", "file", bytes.byteLength);
  }
  put("/docs/report.pdf", "file", 5 * 1024 * 1024);
  // 压缩包样例（占位预览 / 解压入口）
  put("/backup.zip", "file", 128 * 1024);
  put("/docs/site.tar.gz", "file", 64 * 1024);
  put("/docs/dump.tar", "file", 96 * 1024);
  for (let i = 0; i < 10_000; i += 1) put(`/10k/file-${String(i).padStart(5, "0")}.txt`, "file", 1024 + i);

  // ---- 本地树（内置 __local__ 连接，模拟真实 sidecar 的本地文件系统）----------
  // 双栏左栏默认面：独立路径空间 + $HOME 家族 quickPaths，供浏览器验证
  // 「左=本地、右=远端」而不与远端 mock 树混淆。
  const localTree = new Map<string, MockEntry>();
  const localContents = new Map<string, Uint8Array>();
  const putLocal = (path: string, kind: "file" | "dir", size = 0) =>
    localTree.set(path.replace(/\/+$/, "") || "/", { kind, size, modifiedAt: stamp(localTree.size % 30) });
  const LOCAL_HOME = "/Users/demo";
  putLocal("/", "dir");
  putLocal(LOCAL_HOME, "dir");
  for (const dir of ["Desktop", "Downloads", "Documents", "Pictures", "Movies"]) putLocal(`${LOCAL_HOME}/${dir}`, "dir");
  putLocal("/Applications", "dir");
  putLocal("/tmp", "dir");
  {
    const localNote = new TextEncoder().encode("本地文件（__local__）\n\n这是 sidecar 本地文件系统的 mock 样例。\n");
    localContents.set(`${LOCAL_HOME}/Documents/notes-local.txt`, localNote);
    putLocal(`${LOCAL_HOME}/Documents/notes-local.txt`, "file", localNote.byteLength);
  }
  {
    const png = window.atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    const bytes = new Uint8Array(png.length);
    for (let i = 0; i < png.length; i += 1) bytes[i] = png.charCodeAt(i);
    localContents.set(`${LOCAL_HOME}/Desktop/logo-local.png`, bytes);
    putLocal(`${LOCAL_HOME}/Desktop/logo-local.png`, "file", bytes.byteLength);
  }
  putLocal(`${LOCAL_HOME}/Downloads/installer.dmg`, "file", 32 * 1024 * 1024);

  /** __local__ 连接 → 本地树；其余（含未带 connectionId 的当前连接）→ 远端 mock 树。 */
  const treeFor = (connectionId: unknown) => (connectionId === "__local__" ? localTree : tree);
  const contentsFor = (connectionId: unknown) => (connectionId === "__local__" ? localContents : contents);

  const children = (source: Map<string, MockEntry>, dir: string): Array<Record<string, unknown>> => {
    const base = dir.replace(/\/+$/, "");
    const prefix = base === "" || base === "/" ? "/" : `${base}/`;
    return [...source.entries()]
      .filter(([path, entry]) => path !== "/" && path.startsWith(prefix) && !path.slice(prefix.length).includes("/"))
      .map(([path, entry]) => ({ name: path.slice(prefix.length), path, kind: entry.kind, size: entry.size, modifiedAt: entry.modifiedAt }))
      .sort((a, b) => String(a.name).localeCompare(String(b.name)));
  };
  const isDir = (path: string, source: Map<string, MockEntry> = tree) => source.get(path.replace(/\/+$/, ""))?.kind === "dir";
  const exists = (path: string, source: Map<string, MockEntry> = tree) => source.has(path.replace(/\/+$/, ""));

  const assertOk = (path: string) => {
    if (path.includes("error")) throw new Error(`mock backend failure for ${path}`);
  };

  // ---- 异步 job 表（降级 copy/move/rename）--------------------------------
  let jobSeq = 0;
  const jobs = new Map<string, Record<string, unknown>>();
  const timers = new Map<string, number[]>();

  function emit(method: string, payload: Record<string, unknown>) {
    for (const listener of eventListeners) listener({ method, params: payload });
  }

  function runJob(jobId: string, kind: string, source: string, target: string, apply: () => void, cancel: { flag: boolean }) {
    jobs.set(jobId, { jobId, kind, status: "queued", sourcePath: source, targetPath: target, filesDone: 0, filesTotal: 2, bytesDone: 0, bytesTotal: 4096 });
    emit("files/transfer/progress", { jobId, kind, state: "queued", remotePath: `${source} → ${target}` });
    const schedule = (ms: number, fn: () => void) => {
      const timer = window.setTimeout(() => {
        if (cancel.flag) return;
        fn();
      }, ms);
      timers.set(jobId, [...(timers.get(jobId) ?? []), timer]);
    };
    schedule(50, () => {
      jobs.set(jobId, { ...jobs.get(jobId)!, status: "running", bytesDone: 2048 });
      emit("files/transfer/progress", { jobId, kind, state: "running", filesDone: 1, filesTotal: 2, bytesDone: 2048, bytesTotal: 4096, remotePath: `${source} → ${target}` });
    });
    schedule(400, () => {
      if (cancel.flag) return;
      // 对齐 sidecar 失败契约：job 内异常 → failed progress 事件（带 error），
      // 供 TransferPanel 失败态/重试按钮的全流程 UI 验证。源路径含 "error"
      // 在提交期就会被 assertOk 拒绝，因此 job 级失败用目标路径注入
      // （目标含 "fail" → 执行期失败；assertOk 只拒 "error"，可过提交）。
      try {
        if (target.includes("fail")) throw new Error(`mock job failure for ${target}`);
        apply();
      } catch (cause) {
        const message = cause instanceof Error ? cause.message : String(cause);
        jobs.set(jobId, { ...jobs.get(jobId)!, status: "failed", error: message });
        emit("files/transfer/progress", { jobId, kind, state: "failed", error: message, remotePath: `${source} → ${target}` });
        return;
      }
      jobs.set(jobId, { ...jobs.get(jobId)!, status: "completed", filesDone: 2, bytesDone: 4096 });
      emit("files/transfer/progress", { jobId, kind, state: "completed", filesDone: 2, filesTotal: 2, bytesDone: 4096, bytesTotal: 4096, remotePath: `${source} → ${target}` });
    });
  }

  function copyEntry(source: string, target: string) {
    const src = tree.get(source.replace(/\/+$/, ""));
    if (!src) return;
    if (src.kind === "dir") {
      const prefix = `${source.replace(/\/+$/, "")}/`;
      for (const [path, entry] of [...tree.entries()]) {
        if (path.startsWith(prefix)) tree.set(`${target.replace(/\/+$/, "")}/${path.slice(prefix.length)}`, entry);
      }
    }
    put(target, src.kind, src.size);
  }

  function deleteEntry(path: string) {
    const base = path.replace(/\/+$/, "");
    tree.delete(base);
    for (const key of [...tree.keys()]) if (key.startsWith(`${base}/`)) tree.delete(key);
  }

  // ---- 上传槽 ---------------------------------------------------------------
  const uploads = new Map<string, { path: string; size: number; received: number; bytes: Uint8Array }>();

  // ---- 监听器 ---------------------------------------------------------------
  const eventListeners: Array<(event: { method: string; params: Record<string, unknown> }) => void> = [];
  // mock 镜像当前宿主桥的二进制事件形状（零拷贝 data 字段），与真实宿主一致。
  const binaryListeners: Array<(event: { channel: string; data?: Uint8Array }) => void> = [];

  // ---- 审计（files/audit/list 对齐后端 store 审计语义）-----------------------
  // 与 backend/src/main.rs::audit_list_response 同形：{entries:[{at,action,
  // connectionId,path,result}]}，最新在前；limit clamp 1..=1000（缺省 100）。
  const auditLog: Array<{ at: string; action: string; connectionId: string; path: string; result: string }> = [];
  function recordAudit(action: string, path: string, result = "ok") {
    auditLog.push({ at: new Date().toISOString(), action, connectionId: "mock-conn", path, result });
  }

  async function invoke(method: string, payload: Record<string, unknown>): Promise<unknown> {
    const p = payload as Record<string, string | number | undefined>;
    const str = (key: string) => String(p[key] ?? "");
    switch (method) {
      case "files/audit/list": {
        const limitRaw = p.limit;
        const limit = limitRaw === undefined || limitRaw === null
          ? 100
          : Number(limitRaw);
        if (!Number.isInteger(limit) || limit < 0) throw new Error("Invalid request parameters: limit must be a non-negative integer");
        const connection = typeof p.connectionId === "string" && p.connectionId ? p.connectionId : undefined;
        const filtered = auditLog.filter((entry) => !connection || entry.connectionId === connection);
        return { entries: filtered.slice().reverse().slice(0, Math.min(Math.max(limit, 1), 1000)) };
      }
      case "files/list": {
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        assertOk(str("path"));
        return { entries: children(treeFor(p.connectionId), str("path")) };
      }
      case "files/listPaged": {
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        const all = children(treeFor(p.connectionId), str("path"));
        const page = Number(p.page ?? 1);
        const size = Number(p.pageSize ?? 200);
        return { entries: all.slice((page - 1) * size, page * size), total: all.length };
      }
      case "files/capabilities":
        return { scheme: "mock", list: true, write: true, read: true, stat: true, delete: true, createDir: true, copy: true, rename: true, presign: false };
      case "files/quickPaths": {
        // mock 无真实 $HOME：远端树返回根目录 + 实际存在的样例目录 chips；
        // __local__ 连接返回本地 home 家族（对齐真实 sidecar 的 fs 行为）。
        if (p.connectionId === "__local__") {
          return {
            paths: [
              { key: "root", path: "/" },
              { key: "home", path: LOCAL_HOME },
              ...["Desktop", "Downloads", "Documents", "Pictures"]
                .map((dir) => ({ key: dir.toLowerCase(), path: `${LOCAL_HOME}/${dir}` }))
                .filter((item) => localTree.has(item.path)),
            ],
          };
        }
        return {
          paths: [
            { key: "root", path: "/" },
            ...["desktop", "downloads", "documents", "pictures"]
              .filter((key) => tree.has(`/${key}`))
              .map((key) => ({ key, path: `/${key}` })),
          ],
        };
      }
      case "files/stat": {
        const path = str("path");
        assertOk(path);
        const entry = treeFor(p.connectionId).get(path.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${path}`);
        return { entry: { name: path.split("/").filter(Boolean).pop() ?? "/", path, kind: entry.kind, size: entry.size, modifiedAt: entry.modifiedAt } };
      }
      case "files/mkdir": {
        const path = str("path");
        assertOk(path);
        // 按连接路由（P-FILES）：双栏左栏 __local__ 的新建文件夹落本地树。
        treeFor(p.connectionId).set(path.replace(/\/+$/, "") || "/", { kind: "dir", size: 0, modifiedAt: new Date().toISOString() });
        recordAudit(method, path);
        return { success: true };
      }
      case "files/delete": {
        const path = str("path");
        assertOk(path);
        tree.delete(path.replace(/\/+$/, ""));
        recordAudit(method, path);
        return { success: true };
      }
      case "files/purge": {
        const path = str("path");
        assertOk(path);
        if (path.replace(/\/+$/, "") === "" || path.replace(/\/+$/, "") === "/") throw new Error("Purge of the connection root '/' is refused");
        deleteEntry(path);
        recordAudit(method, path);
        return { success: true };
      }
      case "files/rename": {
        const source = str("path");
        const target = str("newPath");
        assertOk(source);
        if (isDir(source) || forceJob) {
          const jobId = `mock-job-${++jobSeq}`;
          const cancel = { flag: false };
          jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
          runJob(jobId, "rename", source, target, () => {
            copyEntry(source, target);
            deleteEntry(source);
          }, cancel);
          return { success: true, transport: "job", jobId };
        }
        const entry = tree.get(source.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${source}`);
        tree.delete(source.replace(/\/+$/, ""));
        put(target, entry.kind, entry.size);
        recordAudit(method, source);
        return { success: true, transport: "native", jobId: null };
      }
      case "files/copy":
      case "files/move":
      case "files/syncDir":
      case "files/copyDir": {
        const source = str("sourcePath");
        const target = str("targetPath");
        assertOk(source);
        if (target.replace(/\/+$/, "") === source.replace(/\/+$/, "")) throw new Error("Destination must differ from the source");
        if (isDir(source) || forceJob) {
          const jobId = `mock-job-${++jobSeq}`;
          const cancel = { flag: false };
          jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
          const move = method === "files/move";
          const sync = method === "files/syncDir";
          runJob(jobId, sync ? "syncDir" : method === "files/copyDir" ? "copyDir" : move ? "move" : "copy", source, target, () => {
            copyEntry(source, target);
            if (move) deleteEntry(source);
          }, cancel);
          return { success: true, transport: "job", jobId };
        }
        const entry = tree.get(source.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${source}`);
        copyEntry(source, target);
        if (method === "files/move") tree.delete(source.replace(/\/+$/, ""));
        recordAudit(method, source);
        return { success: true, transport: "native", jobId: null };
      }
      case "files/transfers/list":
        return { jobs: [...jobs.entries()].filter(([key]) => !key.startsWith("__")).map(([, job]) => job) };
      case "files/transfers/clear": {
        // 与 sidecar 语义一致：仅清完成态，queued/running 不动。
        let cleared = 0;
        for (const [key, job] of [...jobs.entries()]) {
          if (key.startsWith("__")) continue;
          const status = String((job as Record<string, unknown>).status ?? "");
          if (status === "completed" || status === "failed" || status === "canceled") {
            jobs.delete(key);
            cleared += 1;
          }
        }
        return { cleared };
      }
      case "files/transfer/status": {
        const job = jobs.get(str("jobId"));
        if (!job) throw new Error(`Unknown jobId '${str("jobId")}'`);
        return { job, kind: "dirJob" };
      }
      case "files/transfer/cancel": {
        const jobId = str("taskId");
        const cancel = jobs.get(`__cancel_${jobId}`) as unknown as { flag: boolean } | undefined;
        if (cancel) {
          cancel.flag = true;
          for (const timer of timers.get(jobId) ?? []) window.clearTimeout(timer);
          jobs.set(jobId, { ...jobs.get(jobId)!, status: "canceled" });
          emit("files/transfer/progress", { jobId, state: "canceled" });
        }
        return { success: true };
      }
      case "files/upload/start": {
        const taskId = `mock-upload-${++jobSeq}`;
        uploads.set(taskId, { path: str("remotePath"), size: Number(p.size ?? 0), received: 0, bytes: new Uint8Array(Number(p.size ?? 0)) });
        return { taskId };
      }
      case "files/read": {
        // A-FILES ②：预览读取（≤2MiB base64 + truncated）；按连接路由树。
        const path = str("path");
        assertOk(path);
        const source = treeFor(p.connectionId);
        const entry = source.get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "file") throw new Error(`NotFound: ${path}`);
        const all = contentsFor(p.connectionId).get(path.replace(/\/+$/, "")) ?? new Uint8Array(Math.min(entry.size, 64));
        const max = Number(p.maxBytes ?? 2 * 1024 * 1024);
        const data = all.length > max ? all.subarray(0, max) : all;
        return { dataBase64: b64encode(data), truncated: all.length > data.length, size: all.length };
      }
      case "files/write": {
        // A-FILES ②：小文件写回（≤4MiB）
        const path = str("path");
        assertOk(path);
        const bytes = b64decode(String(p.dataBase64 ?? ""));
        if (bytes.byteLength > 4 * 1024 * 1024) throw new Error("payload exceeds 4MiB; use the upload slot instead");
        // 按连接路由（P-FILES）：新建文件走本方法，需与 list 的树一致。
        contentsFor(p.connectionId).set(path.replace(/\/+$/, ""), bytes);
        treeFor(p.connectionId).set(path.replace(/\/+$/, ""), { kind: "file", size: bytes.byteLength, modifiedAt: new Date().toISOString() });
        recordAudit(method, path);
        return { success: true };
      }
      case "files/compress": {
        // P-FILES：压缩（tar/tar.gz 语义模拟）——收集 paths（文件/目录递归）
        // 的伪归档字节；≤10 文件同步返回，否则降级 mock job。
        const paths = Array.isArray(p.paths) ? (p.paths as unknown[]).map(String) : [];
        const target = str("targetPath");
        assertOk(target);
        if (!paths.length) throw new Error("paths must not be empty");
        if (!/\.(tar|tar\.gz|tgz)$/i.test(target)) throw new Error("Archive target must end with .tar, .tar.gz or .tgz");
        if (treeFor(p.connectionId).has(target.replace(/\/+$/, ""))) throw new Error(`Archive target '${target}' already exists`);
        const source = treeFor(p.connectionId);
        const collected: Uint8Array[] = [];
        for (const raw of paths) {
          const path = raw.replace(/\/+$/, "");
          const entry = source.get(path);
          if (!entry) throw new Error(`NotFound: ${path}`);
          if (entry.kind === "file") {
            collected.push(contentsFor(p.connectionId).get(path) ?? new Uint8Array(0));
            continue;
          }
          for (const [key, value] of source) {
            if (key.startsWith(`${path}/`) && value.kind === "file") {
              collected.push(contentsFor(p.connectionId).get(key) ?? new Uint8Array(0));
            }
          }
        }
        if (!collected.length) throw new Error("Nothing to compress: the sources hold no files");
        // 伪归档字节（gzip 魔数前缀区分格式；mock 不消费真实 tar 结构）。
        const gzip = /\.(tar\.gz|tgz)$/i.test(target);
        const bytes = gzip ? concatBytes([new Uint8Array([0x1f, 0x8b]), concatBytes(collected)]) : concatBytes(collected);
        const apply = () => {
          const routed = treeFor(p.connectionId);
          contentsFor(p.connectionId).set(target.replace(/\/+$/, ""), bytes);
          routed.set(target.replace(/\/+$/, ""), { kind: "file", size: bytes.byteLength, modifiedAt: new Date().toISOString() });
          recordAudit(method, target);
        };
        if (collected.length <= 10 && bytes.byteLength <= 8 * 1024 * 1024) {
          apply();
          return { success: true, transport: "native", jobId: null };
        }
        const jobId = `mock-job-${++jobSeq}`;
        const cancel = { flag: false };
        jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
        runJob(jobId, "compress", paths[0], target, apply, cancel);
        return { success: true, transport: "job", jobId };
      }
      case "files/upload/finish": {
        const slot = uploads.get(str("taskId"));
        if (!slot) throw new Error("Upload task was not found");
        if (slot.received !== slot.size) throw new Error(`Upload is incomplete: ${slot.received}/${slot.size}`);
        assertOk(slot.path);
        tree.set(slot.path.replace(/\/+$/, ""), { kind: "file", size: slot.size, modifiedAt: new Date().toISOString() });
        uploads.delete(str("taskId"));
        return { success: true };
      }
      case "files/download/start": {
        const path = str("remotePath");
        assertOk(path);
        const entry = tree.get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "file") throw new Error(`NotFound: ${path}`);
        const taskId = `mock-download-${++jobSeq}`;
        const size = entry.size;
        // 模拟 sidecar 下载泵：start 返回后异步按 offset 推帧。
        const push = (offset: number) => {
          if (offset >= size) return;
          const length = Math.min(CHUNK, size - offset);
          const frame = new Uint8Array(8 + length);
          new DataView(frame.buffer).setBigUint64(0, BigInt(offset), false);
          frame.fill((offset / 7) % 251, 8);
          for (const listener of binaryListeners) listener({ channel: `files/download/${taskId}`, data: frame });
          window.setTimeout(() => push(offset + length), 1);
        };
        window.setTimeout(() => push(0), 10);
        return { taskId, size };
      }
      case "files/download/finish":
      case "files/upload/cancel":
        return { success: true };
      default:
        throw new Error(`Method not found: ${method}`);
    }
  }

  // ---- 组装宿主桥 -----------------------------------------------------------
  // 与 DBX globals.css 的 :root（pearl 浅色）和 .dark 规范块保持一致；
  // 镜像宿主 1.1 theme 通道形状（当前宿主只推 theme，不推 appearance）。
  const light = params.get("theme") === "light";
  const theme: DbxPluginTheme = {
    appearance: light ? "light" : "dark",
    tokens: light
      ? { "--color-background": "rgb(255 255 255)", "--color-foreground": "rgb(10 10 10)", "--color-muted": "rgb(245 245 245)", "--color-muted-foreground": "rgb(115 115 115)", "--color-accent": "rgb(245 245 245)", "--color-accent-foreground": "rgb(23 23 23)", "--color-border": "rgb(229 229 229)", "--color-destructive": "rgb(231 0 11)" }
      : { "--color-background": "rgb(19 20 22)", "--color-foreground": "rgb(215 215 219)", "--color-muted": "rgb(42 42 45)", "--color-muted-foreground": "rgb(151 152 157)", "--color-accent": "rgb(46 47 51)", "--color-accent-foreground": "rgb(221 221 226)", "--color-border": "rgb(110 110 114 / 0.28)", "--color-destructive": "rgb(243 98 95)" },
  };
  window.dbxPlugin = {
    ready: Promise.resolve({ connectionId: "mock-conn" }),
    context: {
      connectionId: "mock-conn",
      connection: { name: "Mock Storage", host: "mock.local", readOnly: false, protocol: "fs" },
    },
    theme,
    locale: params.get("locale") ?? "zh-CN",
    request: async <T>() => ({}) as T,
    invoke: async <T>(method: string, payload?: Record<string, unknown>) => invoke(method, payload ?? {}) as Promise<T>,
    notify: async () => undefined,
    sendBinary: async (channel: string, data: Uint8Array | ArrayBuffer | string) => {
      const match = channel.match(/^files\/upload\/(.+)$/);
      if (!match) return;
      const slot = uploads.get(match[1]);
      if (!slot) return;
      if (typeof data === "string") return;
      const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
      const offset = Number(new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, false));
      const chunk = bytes.subarray(8);
      slot.bytes.set(chunk, offset);
      slot.received = Math.max(slot.received, offset + chunk.length);
    },
    onEvent: (listener) => {
      eventListeners.push(listener);
      return () => {
        const index = eventListeners.indexOf(listener);
        if (index >= 0) eventListeners.splice(index, 1);
      };
    },
    onBinary: (listener) => {
      binaryListeners.push(listener);
      return () => {
        const index = binaryListeners.indexOf(listener);
        if (index >= 0) binaryListeners.splice(index, 1);
      };
    },
    decodeBase64: b64decode,
    encodeBase64: b64encode,
    clipboard: { readText: async () => "", writeText: async () => undefined },
  };
}
