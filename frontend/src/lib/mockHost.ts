// 开发/验证专用 mock 宿主桥（P-FILES ④）：URL 带 ?mock=1 且宿主桥缺失时启用。
// 用途：浏览器内验证 UI 三态（加载/空/错误）、大目录虚拟滚动、transport=job
// 的 copy/move/rename 终态刷新与传输面板——不进生产路径（动态 import 单独分包）。
// 行为开关（query 参数）：
//   ?mock=1          启用
//   &locale=zh-CN    宿主 locale（默认 zh-CN）
//   &theme=light     宿主 1.1 theme 通道方案（默认 dark，与真实宿主一致）
//   &delay=300       files/list 人为延迟 ms（便于观察加载态）
//   &job=1           copy/move 一律走降级 job（默认仅目录/`mockDir`）
//   &ro=1            只读态注入（connection.readOnly + capabilities.readOnly，
//                    P2-13①：供只读徽章/写按钮禁用/右键菜单禁用的 UI 走查）
//   &connectionTest=fail  connection/test 的不可达夹具（不发真实网络请求）
// 任何包含 "error" 的路径都会返回业务错误（便于验证错误横幅与重试）。
// __local__ 连接（双栏左栏本地面）：list/listPaged/stat/quickPaths/read 路由到
// 独立本地树（$HOME 家族 quickPaths）；读写按连接落各自内存树。

import { CUSTOM_SERVICES } from "./opendalServices";

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

// 最近一次安装实例的 emit 挂钩（installMockHost 内赋值；emitUiIntent 使用）。
let emitEventRef: ((method: string, payload: Record<string, unknown>) => void) | null = null;

export function installMockHost() {
  if (window.dbxPlugin) return;
  const params = new URLSearchParams(window.location.search);
  const delayMs = Number(params.get("delay") ?? "250");
  const forceJob = params.get("job") === "1";
  // P2-13①：?ro=1 只读态注入（与 canWrite = !connection.readOnly && !capabilities.readOnly 双闸对齐）。
  const readOnly = params.get("ro") === "1";

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
  putText("/docs/readme.md", "# Files Studio\n\n双栏文件浏览（A-FILES）验证样例。\n\n- 左栏：源（当前连接）\n- 右栏：目标面板 / 预览\n\n编辑此文件并保存会走 files/write。\n");
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
  // 文件类型图标验证样例：集中放 Documents，一次导航即可目视核对各类型图标。
  for (const name of ["报告.docx", "预算.xlsx", "路演.pptx", "手册.pdf", "backup.zip", "song.mp3", "clip.mp4", "app.py"]) {
    putLocal(`${LOCAL_HOME}/Documents/${name}`, "file", 4096);
  }

  const connections = new Map([
    ["mock-conn", { tree, contents, readOnly }],
    ["__local__", { tree: localTree, contents: localContents, readOnly: false }],
  ]);
  const connectionIdOf = (value: unknown) => String(value ?? "mock-conn");
  const storageFor = (value: unknown) => {
    const id = connectionIdOf(value);
    const storage = connections.get(id);
    if (!storage) throw new Error(`Unknown connectionId '${id}'; connect first`);
    return storage;
  };
  const treeFor = (id: unknown) => storageFor(id).tree;
  const contentsFor = (id: unknown) => storageFor(id).contents;

  // 对齐 lifecycle 的必填字段与 custom JSON 形状；只保留 id/策略，不存凭据。
  function parseConnection(payload: Record<string, unknown>) {
    const connection = payload.connection as Record<string, unknown> | undefined;
    if (!connection || typeof connection !== "object" || Array.isArray(connection)) throw new Error("Missing connection payload");
    if (typeof connection.id !== "string" || !connection.id.trim()) throw new Error("Missing connection id");
    const config = connection.external_config as Record<string, unknown> | undefined;
    const protocol = typeof config?.protocol === "string" ? config.protocol.trim() : "";
    if (!protocol) throw new Error("Missing protocol in external_config");
    const protocols = ["fs", "s3", "gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp", "smb", "sftp-native", "opendal-custom"];
    if (!protocols.includes(protocol)) throw new Error(`Unsupported protocol '${protocol}'; expected one of ${protocols.join(", ")}`);
    if (protocol === "opendal-custom") {
      let custom = config?.config ?? {};
      if (typeof custom === "string") {
        try { custom = JSON.parse(custom); } catch { throw new Error("Invalid service config JSON"); }
        if (!custom || typeof custom !== "object" || Array.isArray(custom)) throw new Error("Service config JSON must be an object");
      }
      if (!custom || typeof custom !== "object" || Array.isArray(custom)) throw new Error("Service config must be a JSON object");
      const service = typeof config?.service === "string" ? config.service.trim() : "";
      if (!service) throw new Error("opendal-custom requires 'service'");
      if (!/^[a-zA-Z0-9_-]+$/.test(service)) throw new Error(`Invalid opendal-custom service '${service}'`);
      if (!CUSTOM_SERVICES.has(service) && service !== "memory") throw new Error(`Mock fixture does not support OpenDAL service '${service}'`);
    }
    return { id: connection.id.trim(), readOnly: connection.read_only === true || config?.read_only === true };
  }

  const children = (source: Map<string, MockEntry>, dir: string): Array<Record<string, unknown>> => {
    const base = dir.replace(/\/+$/, "");
    const prefix = base === "" || base === "/" ? "/" : `${base}/`;
    return [...source.entries()]
      .filter(([path, entry]) => path !== "/" && path.startsWith(prefix) && !path.slice(prefix.length).includes("/"))
      .map(([path, entry]) => ({ name: path.slice(prefix.length), path, kind: entry.kind, size: entry.size, modifiedAt: entry.modifiedAt }))
      .sort((a, b) => String(a.name).localeCompare(String(b.name)));
  };
  // （copy/move/rename 的目录/存在判断已随 P2-13② 收口进各 case 内部，见下。）
  const assertOk = (path: string) => {
    if (path.includes("error")) throw new Error(`mock backend failure for ${path}`);
    // P2-1 夹具：notfound 路径注入「不存在」类业务错误（友好映射层走查用）
    if (path.includes("notfound")) throw new Error(`NotFound: ${path}`);
  };

  // P2-13④：压缩包条目来源表（B-ARCHIVE 夹具层缺位收口）。
  // mock 不存真实 tar 结构：compress 时记录源路径集合，archiveList 按当前树
  // 展开为归档内相对路径条目；三个预置样例归档同样预登记。
  const archiveSources = new Map<string, string[]>();
  archiveSources.set("/backup.zip", ["/docs/readme.md", "/docs/notes.txt", "/docs/logo.png"]);
  archiveSources.set("/docs/site.tar.gz", ["/docs"]);
  archiveSources.set("/docs/dump.tar", ["/docs/data.bin", "/docs/report.pdf"]);

  // ---- 异步 job 表（降级 copy/move/rename）--------------------------------
  let jobSeq = 0;
  const jobs = new Map<string, Record<string, unknown>>();
  const timers = new Map<string, number[]>();

  function emit(method: string, payload: Record<string, unknown>) {
    for (const listener of eventListeners) listener({ method, params: payload });
  }
  // 测试驱动器挂钩：记录最近一次安装实例的 emit（emitUiIntent 走此通道，
  // 与 sidecar emitter.Event 同面）。未安装时为 null。
  emitEventRef = emit;

  function runJob(jobId: string, kind: string, source: string, target: string, apply: () => void, cancel: { flag: boolean }, sourceConnectionId: string, targetConnectionId = sourceConnectionId) {
    // P2-8：remotePath 与真实 sidecar（transfers.rs，camelCase）契约对齐，
    // 轮询兜底不再把可读任务标题冲掉成裸 jobId。
    jobs.set(jobId, { jobId, kind, status: "queued", sourceConnectionId, targetConnectionId, sourcePath: source, targetPath: target, remotePath: `${source} → ${target}`, filesDone: 0, filesTotal: 2, bytesDone: 0, bytesTotal: 4096 });
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

  function copyEntryBetween(
    sourceTree: Map<string, MockEntry>,
    sourceContents: Map<string, Uint8Array>,
    targetTree: Map<string, MockEntry>,
    targetContents: Map<string, Uint8Array>,
    source: string,
    target: string,
  ) {
    // P2-13②：跨连接复制/移动按 sourceConnectionId/targetConnectionId 路由
    // 源/目标树与内容仓；同连接时两个参数对相同，行为与旧 copyEntry 一致。
    const src = sourceTree.get(source.replace(/\/+$/, ""));
    if (!src) return;
    const cleanTarget = target.replace(/\/+$/, "") || "/";
    if (src.kind === "dir") {
      const prefix = `${source.replace(/\/+$/, "")}/`;
      for (const [path, entry] of [...sourceTree.entries()]) {
        if (!path.startsWith(prefix)) continue;
        targetTree.set(`${cleanTarget}/${path.slice(prefix.length)}`, entry);
        const bytes = sourceContents.get(path);
        if (bytes) targetContents.set(`${cleanTarget}/${path.slice(prefix.length)}`, bytes);
      }
    }
    const bytes = sourceContents.get(source.replace(/\/+$/, ""));
    if (bytes) targetContents.set(cleanTarget, bytes);
    targetTree.set(cleanTarget, { kind: src.kind, size: src.size, modifiedAt: new Date().toISOString() });
  }

  function deleteEntry(path: string, source: Map<string, MockEntry> = tree) {
    const base = path.replace(/\/+$/, "");
    source.delete(base);
    for (const key of [...source.keys()]) if (key.startsWith(`${base}/`)) source.delete(key);
  }

  // ---- 上传槽 ---------------------------------------------------------------
  // P1-5 夹具契约修复：记录 start 时的 connectionId，finish 按 connectionId
  // 落对应树/内容（__local__ → 本地树），与真实 sidecar 的按连接落盘一致。
  const uploads = new Map<string, { path: string; size: number; received: number; bytes: Uint8Array; connectionId: unknown }>();
  const downloads = new Map<string, { path: string; size: number; received: number; connectionId: unknown; timer: number; canceled: boolean }>();

  // ---- 监听器 ---------------------------------------------------------------
  const eventListeners: Array<(event: DbxPluginEvent) => void> = [];
  // mock 镜像当前宿主桥的二进制事件形状（零拷贝 data 字段），与真实宿主一致。
  const binaryListeners: Array<(event: { channel: string; data?: Uint8Array }) => void> = [];

  // ---- 审计（files/audit/list 对齐后端 store 审计语义）-----------------------
  // 与 backend/src/main.rs::audit_list_response 同形：{entries:[{at,action,
  // connectionId,path,result}]}，最新在前；limit clamp 1..=1000（缺省 100）。
  const auditLog: Array<{ at: string; action: string; connectionId: string; path: string; result: string }> = [];
  function recordAudit(action: string, path: string, connectionId: unknown, result = "ok") {
    auditLog.push({ at: new Date().toISOString(), action, connectionId: connectionIdOf(connectionId), path, result });
  }

  const matchesConnection = (job: Record<string, unknown>, id: string | null | undefined) =>
    id == null || job.connectionId === id || job.sourceConnectionId === id || job.targetConnectionId === id;

  async function invoke(method: string, payload: Record<string, unknown>): Promise<unknown> {
    const p = payload as Record<string, string | number | undefined>;
    const str = (key: string) => String(p[key] ?? "");
    switch (method) {
      case "connection/test":
      case "connection/connect": {
        const connection = parseConnection(payload);
        if (method === "connection/test") {
          if (params.get("connectionTest") === "fail") throw new Error("Storage check failed: mock endpoint is unreachable");
          return { success: true, message: "Storage backend reachable" };
        }
        if (connection.id === "__local__") throw new Error("connectionId '__local__' is reserved for the built-in local filesystem");
        const storage = connections.get(connection.id) ?? {
          tree: new Map<string, MockEntry>([["/", { kind: "dir", size: 0, modifiedAt: stamp(0) }]]),
          contents: new Map<string, Uint8Array>(),
        };
        connections.set(connection.id, { ...storage, readOnly: connection.readOnly });
        return { success: true };
      }
      case "connection/disconnect": {
        const id = (payload.connection as Record<string, unknown> | undefined)?.id;
        if (typeof id !== "string" || !id) throw new Error("Missing connection id");
        for (const [taskId, job] of jobs) {
          if (!taskId.startsWith("__") && matchesConnection(job, id) && (job.status === "queued" || job.status === "running")) {
            await invoke("files/transfer/cancel", { taskId });
          }
        }
        if (id !== "__local__") connections.delete(id);
        return { success: true };
      }
      case "files/audit/list": {
        const limitRaw = p.limit;
        const limit = limitRaw === undefined || limitRaw === null
          ? 100
          : limitRaw;
        if (typeof limit !== "number" || !Number.isInteger(limit) || limit < 0 || limit >= 2 ** 64) throw new Error("Invalid request parameters: limit must be a non-negative integer");
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
        return { scheme: "mock", list: true, write: true, read: true, stat: true, delete: true, createDir: true, copy: true, rename: true, presign: false, readOnly: storageFor(p.connectionId).readOnly };
      case "files/quickPaths": {
        const source = treeFor(p.connectionId);
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
              .filter((key) => source.has(`/${key}`))
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
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/delete": {
        const path = str("path");
        assertOk(path);
        // R3-P2-2：按连接路由（此前写死远端树，双栏左栏删除「成功即无效」，
        // 与 P1-5 修复前的上传假成功同构）。
        treeFor(p.connectionId).delete(path.replace(/\/+$/, ""));
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/purge": {
        const path = str("path");
        assertOk(path);
        if (path.replace(/\/+$/, "") === "" || path.replace(/\/+$/, "") === "/") throw new Error("Purge of the connection root '/' is refused");
        // R3-P2-2：按连接路由（同 delete）。
        deleteEntry(path, treeFor(p.connectionId));
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/rename": {
        const source = str("path");
        const target = str("newPath");
        assertOk(source);
        // 按连接路由（与 copy/move 的 P2-13② 收口一致）：__local__ 栏重命名落本地树。
        const renameTree = treeFor(p.connectionId);
        const renameContents = contentsFor(p.connectionId);
        if (renameTree.get(source.replace(/\/+$/, ""))?.kind === "dir" || forceJob) {
          const jobId = `mock-job-${++jobSeq}`;
          const cancel = { flag: false };
          jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
          runJob(jobId, "rename", source, target, () => {
            copyEntryBetween(renameTree, renameContents, renameTree, renameContents, source, target);
            deleteEntry(source, renameTree);
          }, cancel, connectionIdOf(p.connectionId));
          return { success: true, transport: "job", jobId };
        }
        const entry = renameTree.get(source.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${source}`);
        copyEntryBetween(renameTree, renameContents, renameTree, renameContents, source, target);
        deleteEntry(source, renameTree);
        recordAudit(method, source, p.connectionId);
        return { success: true, transport: "native", jobId: null };
      }
      case "files/copy":
      case "files/move":
      case "files/syncDir":
      case "files/copyDir": {
        const source = str("sourcePath");
        const target = str("targetPath");
        assertOk(source);
        const directoryJob = method === "files/syncDir" || method === "files/copyDir";
        if (directoryJob && (typeof p.sourceConnectionId !== "string" || typeof p.targetConnectionId !== "string")) {
          throw new Error("Invalid request parameters: sourceConnectionId and targetConnectionId are required");
        }
        // P2-13②：按契约路由 sourceConnectionId/targetConnectionId（缺省回落
        // connectionId / 远端树），跨连接双栏复制本地 → 远端可自洽走查。
        const sourceTree = treeFor(p.sourceConnectionId ?? p.connectionId);
        const sourceContents = contentsFor(p.sourceConnectionId ?? p.connectionId);
        const targetTree = treeFor(p.targetConnectionId ?? p.connectionId);
        const targetContents = contentsFor(p.targetConnectionId ?? p.connectionId);
        const sourceId = connectionIdOf(p.sourceConnectionId ?? p.connectionId);
        const targetId = connectionIdOf(p.targetConnectionId ?? p.connectionId);
        if (sourceId === targetId && target.replace(/\/+$/, "") === source.replace(/\/+$/, "")) throw new Error("Destination must differ from the source");
        const sourceIsDir = sourceTree.get(source.replace(/\/+$/, ""))?.kind === "dir";
        if (directoryJob || sourceIsDir || forceJob || sourceId !== targetId) {
          const jobId = `mock-job-${++jobSeq}`;
          const cancel = { flag: false };
          jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
          const move = method === "files/move";
          const sync = method === "files/syncDir";
          runJob(jobId, sync ? "syncDir" : method === "files/copyDir" ? "copyDir" : move ? "move" : "copy", source, target, () => {
            copyEntryBetween(sourceTree, sourceContents, targetTree, targetContents, source, target);
            if (move) deleteEntry(source, sourceTree);
          }, cancel, sourceId, targetId);
          return directoryJob ? { jobId } : { success: true, transport: "job", jobId };
        }
        const entry = sourceTree.get(source.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${source}`);
        copyEntryBetween(sourceTree, sourceContents, targetTree, targetContents, source, target);
        if (method === "files/move") deleteEntry(source, sourceTree);
        recordAudit(method, source, sourceId);
        return { success: true, transport: "native", jobId: null };
      }
      case "files/transfers/list":
      case "files/transfers/clear": {
        const connection = payload.connectionId;
        if (connection != null && typeof connection !== "string") throw new Error("Invalid request parameters: connectionId must be a string");
        if (method === "files/transfers/list") {
          return { jobs: [...jobs.entries()].filter(([key, job]) => !key.startsWith("__") && matchesConnection(job, connection)).map(([, job]) => job) };
        }
        // 与 sidecar 语义一致：仅清完成态，queued/running 不动。
        let cleared = 0;
        for (const [key, job] of [...jobs.entries()]) {
          if (key.startsWith("__") || !matchesConnection(job, connection)) continue;
          const status = String((job as Record<string, unknown>).status ?? "");
          if (status === "completed" || status === "failed" || status === "canceled") {
            jobs.delete(key);
            jobs.delete(`__cancel_${key}`);
            timers.delete(key);
            cleared += 1;
          }
        }
        return { cleared };
      }
      case "files/transfer/status": {
        const job = jobs.get(str("jobId"));
        if (!job) throw new Error(`Unknown jobId '${str("jobId")}'`);
        return { job, kind: job.taskId ? "transfer" : "dirJob" };
      }
      case "files/transfers/delete": {
        // 镜像 sidecar 语义：仅删除已结束记录，queued/running 拒绝；
        // 未知 id 返回 removed: 0（前端按无变化处理）。
        const key = str("taskId");
        const job = jobs.get(key);
        if (!job) return { removed: 0 };
        const status = String((job as Record<string, unknown>).status ?? "");
        if (status === "queued" || status === "running") throw new Error("Transfer is still in progress; cancel it first");
        jobs.delete(key);
        jobs.delete(`__cancel_${key}`);
        timers.delete(key);
        return { removed: 1 };
      }
      case "files/local/capabilities": {
        // mock 模拟 web 宿主：无本机落盘，前端走宿主保存/浏览器兜底路径。
        return { canSaveLocal: false, downloadsDir: "", platform: "web" };
      }
      case "files/transfer/cancel": {
        if (typeof p.taskId !== "string") throw new Error("Invalid request parameters: taskId must be a string");
        const jobId = str("taskId");
        const upload = uploads.get(jobId);
        const download = downloads.get(jobId);
        const slot = upload ?? download;
        if (slot) {
          // 镜像 sidecar：取消释放上传槽/停止下载泵，并发出终态事件。
          uploads.delete(jobId);
          if (download) {
            download.canceled = true;
            window.clearTimeout(download.timer);
            downloads.delete(jobId);
          }
          const kind = upload ? "upload" : "download";
          const connectionId = String(slot.connectionId ?? "mock-conn");
          jobs.set(jobId, { ...jobs.get(jobId), taskId: jobId, kind, connectionId, remotePath: slot.path, status: "canceled", totalBytes: slot.size, transferredBytes: slot.received });
          emit("files/transfer/progress", { taskId: jobId, kind, connectionId, remotePath: slot.path, state: "canceled", size: slot.size, transferred: slot.received, total: slot.size });
          return { success: true };
        }
        const cancel = jobs.get(`__cancel_${jobId}`) as unknown as { flag: boolean } | undefined;
        if (cancel) {
          cancel.flag = true;
          for (const timer of timers.get(jobId) ?? []) window.clearTimeout(timer);
          jobs.set(jobId, { ...jobs.get(jobId)!, status: "canceled" });
          emit("files/transfer/progress", { jobId, state: "canceled" });
          return { success: true };
        }
        throw new Error("Transfer task was not found");
      }
      case "files/upload/start": {
        storageFor(p.connectionId);
        const taskId = `mock-upload-${++jobSeq}`;
        uploads.set(taskId, { path: str("remotePath"), size: Number(p.size ?? 0), received: 0, bytes: new Uint8Array(Number(p.size ?? 0)), connectionId: p.connectionId });
        jobs.set(taskId, { taskId, kind: "upload", connectionId: connectionIdOf(p.connectionId), remotePath: str("remotePath"), status: "running", totalBytes: Number(p.size ?? 0), transferredBytes: 0 });
        return { taskId };
      }
      case "files/archiveList": {
        // P2-13④：压缩包内容列表（B-ARCHIVE 夹具收口）。条目契约对齐后端
        // archive.rs::ArchiveEntry：{name, path(归档内相对路径), kind, size}；
        // page/pageSize 可选（缺省 1/200，clamp 1..1000），返回 {entries, total}。
        const path = str("path");
        assertOk(path);
        const archiveTree = treeFor(p.connectionId);
        const targets = archiveSources.get(path.replace(/\/+$/, ""));
        if (!targets) throw new Error(`NotFound: ${path}`);
        const entries: Array<Record<string, unknown>> = [];
        for (const rawSource of targets) {
          const clean = rawSource.replace(/\/+$/, "");
          const root = archiveTree.get(clean);
          if (!root) continue;
          if (root.kind === "file") {
            entries.push({ name: clean.split("/").pop() ?? clean, path: clean.replace(/^\//, ""), kind: "file", size: root.size });
            continue;
          }
          const prefix = `${clean}/`;
          for (const [key, value] of archiveTree) {
            if (!key.startsWith(prefix) || value.kind !== "file") continue;
            entries.push({ name: key.split("/").pop() ?? key, path: key.replace(/^\//, ""), kind: "file", size: value.size });
          }
        }
        entries.sort((a, b) => String(a.path).localeCompare(String(b.path)));
        const total = entries.length;
        const page = Math.max(1, Number(p.page ?? 1));
        const pageSize = Math.min(1000, Math.max(1, Number(p.pageSize ?? 200)));
        return { entries: entries.slice((page - 1) * pageSize, page * pageSize), total };
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
        recordAudit(method, path, p.connectionId);
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
          archiveSources.set(target.replace(/\/+$/, ""), paths.map((item) => item.replace(/\/+$/, "")));
          recordAudit(method, target, p.connectionId);
        };
        if (collected.length <= 10 && bytes.byteLength <= 8 * 1024 * 1024) {
          apply();
          return { success: true, transport: "native", jobId: null };
        }
        const jobId = `mock-job-${++jobSeq}`;
        const cancel = { flag: false };
        jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
        runJob(jobId, "compress", paths[0], target, apply, cancel, connectionIdOf(p.connectionId));
        return { success: true, transport: "job", jobId };
      }
      case "files/upload/finish": {
        const slot = uploads.get(str("taskId"));
        const job = jobs.get(str("taskId"));
        // sidecar 对已终结的上传 finish 幂等；取消后绝不重新落文件。
        if (!slot && job?.kind === "upload" && ["completed", "failed", "canceled"].includes(String(job.status))) return { success: true };
        if (!slot) throw new Error("Upload task was not found");
        if (slot.received !== slot.size) throw new Error(`Upload is incomplete: ${slot.received}/${slot.size}`);
        try {
          assertOk(slot.path);
        } catch (cause) {
          uploads.delete(str("taskId"));
          const message = cause instanceof Error ? cause.message : String(cause);
          jobs.set(str("taskId"), { ...job, status: "failed", error: message, transferredBytes: slot.received });
          emit("files/transfer/progress", { taskId: str("taskId"), kind: "upload", connectionId: connectionIdOf(slot.connectionId), remotePath: slot.path, state: "failed", error: message, size: slot.size, transferred: slot.received, total: slot.size });
          throw cause;
        }
        // 按 start 记录的 connectionId 落树 + 写内容（此前写死远端树，
        // 双栏本地上传「成功即消失」；P2-13③ 同款收口）。
        const landed = slot.path.replace(/\/+$/, "") || "/";
        contentsFor(slot.connectionId).set(landed, slot.bytes);
        treeFor(slot.connectionId).set(landed, { kind: "file", size: slot.size, modifiedAt: new Date().toISOString() });
        recordAudit("files/upload", slot.path, slot.connectionId);
        jobs.set(str("taskId"), { ...job, status: "completed", transferredBytes: slot.received });
        uploads.delete(str("taskId"));
        return { success: true };
      }
      case "files/download/start": {
        const path = str("remotePath");
        assertOk(path);
        // R5-P2-8：按 connectionId 路由（此前写死远端 tree，双栏本地面下载
        // 报 NotFound——delete/purge/copy/move 等同族方法已收口，唯此漏网）。
        const entry = treeFor(p.connectionId).get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "file") throw new Error(`NotFound: ${path}`);
        const taskId = `mock-download-${++jobSeq}`;
        const size = entry.size;
        const slot = { path, size, received: 0, connectionId: p.connectionId, timer: 0, canceled: false };
        downloads.set(taskId, slot);
        jobs.set(taskId, { taskId, kind: "download", connectionId: connectionIdOf(p.connectionId), remotePath: path, status: "running", totalBytes: size, transferredBytes: 0 });
        // 模拟 sidecar 下载泵：start 返回后异步按 offset 推帧。
        const push = (offset: number) => {
          if (slot.canceled || offset >= size) return;
          const length = Math.min(CHUNK, size - offset);
          const frame = new Uint8Array(8 + length);
          new DataView(frame.buffer).setBigUint64(0, BigInt(offset), false);
          frame.fill((offset / 7) % 251, 8);
          slot.received = offset + length;
          jobs.set(taskId, { ...jobs.get(taskId), transferredBytes: slot.received });
          for (const listener of binaryListeners) listener({ channel: `files/download/${taskId}`, data: frame });
          if (!slot.canceled) slot.timer = window.setTimeout(() => push(offset + length), 1);
        };
        slot.timer = window.setTimeout(() => push(0), 10);
        return { taskId, size };
      }
      case "files/download/finish": {
        const slot = downloads.get(str("taskId"));
        if (slot) {
          slot.canceled = true;
          window.clearTimeout(slot.timer);
          downloads.delete(str("taskId"));
          jobs.set(str("taskId"), { ...jobs.get(str("taskId")), status: "completed", transferredBytes: slot.received });
        }
        return { success: true };
      }
      case "files/ui/state/report": {
        // MCP UI intent 回报（M2，AGENTS.md 硬性规则 7：mock 镜像真实桥形状）。
        // 镜像 sidecar mcp.rs::report 校验——带 intentId 时 status 必须是
        // applied|rejected；无 intentId 为快照型（恒 success，sidecar 覆盖
        // 最新快照）。fixture 不缓存快照（单测经 report 调用形状断言即可）。
        const status = String(p.status ?? "");
        const intentId = typeof p.intentId === "string" ? p.intentId.trim() : "";
        if (intentId && status !== "applied" && status !== "rejected") throw new Error("status must be applied or rejected");
        return { success: true };
      }
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
  let context: Record<string, unknown> = {
    connectionId: "mock-conn",
    connection: { name: "Mock Storage", host: "mock.local", readOnly, protocol: "fs" },
  };
  let currentLocale = params.get("locale") ?? "zh-CN";
  let currentTheme = theme;
  const contextListeners = new Set<(context: Record<string, unknown>) => void>();
  const initListeners = new Set<(context: Record<string, unknown>) => void>();
  window.dbxPlugin = {
    // ready 返回完整 context（真实宿主同形）：?ro=1 的 readOnly 经 connection
   // 透传给 App 的 canWrite 判定（此前 ready 只带 connectionId，ro 注入不生效）。
    ready: Promise.resolve(context),
    get context() { return context; },
    get theme() { return currentTheme; },
    get locale() { return currentLocale; },
    request: async <T>(method: string) => {
      if (method === "host.getContext") return structuredClone(context) as T;
      // 连接枚举：让双栏目标选择与调试壳的连接列表能看到两个内置连接。
      if (method === "host.listConnections") {
        return [
          { id: "mock-conn", name: "Mock Storage" },
          { id: "__local__", name: "本地文件" },
        ] as T;
      }
      throw new Error(`Unsupported plugin host method '${method}'`);
    },
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
      jobs.set(match[1], { ...jobs.get(match[1]), transferredBytes: slot.received });
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
    onContext: (listener) => {
      contextListeners.add(listener);
      return () => { contextListeners.delete(listener); };
    },
    onInit: (listener) => {
      initListeners.add(listener);
      listener(context);
      return () => { initListeners.delete(listener); };
    },
    decodeBase64: b64decode,
    encodeBase64: b64encode,
    clipboard: { readText: async () => "", writeText: async () => undefined },
  };

  // 测试驱动器只作为安装结果返回，不向真实 dbxPlugin API 添加方法。
  return {
    setContext(next: Record<string, unknown>) {
      context = structuredClone(next);
      contextListeners.forEach((listener) => listener(context));
      document.dispatchEvent(new CustomEvent("dbx-plugin-context", { detail: context }));
    },
    setEnvironment(next: { locale?: string; theme?: DbxPluginTheme }) {
      if (typeof next.locale === "string") currentLocale = next.locale;
      if (next.theme) currentTheme = next.theme;
      const event: DbxPluginEnvironmentEvent = { type: "env", ...next };
      eventListeners.forEach((listener) => listener(event));
      document.dispatchEvent(new CustomEvent("dbx-plugin-env", { detail: event }));
    },
    emitEvent(method: string, payload: Record<string, unknown>) {
      emit(method, payload);
    },
  };
}

/** 测试/走查注入：按 sidecar `files/ui/intent` 事件形状发一条 intent
 * （mock 与真实 emitter.Event 同面；useUiIntent 消费后回报
 * files/ui/state/report）。mock 未安装时显式报错，不静默吞掉。 */
export function emitUiIntent(message: { intentId: string; action: string; params?: Record<string, unknown> }) {
  if (!emitEventRef) throw new Error("mock host is not installed");
  emitEventRef("files/ui/intent", {
    intentId: message.intentId,
    action: message.action,
    params: message.params ?? {},
  });
}
