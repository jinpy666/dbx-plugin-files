<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import { ArrowLeft, ArrowRight, Copy, ArrowUp, RefreshCw, Search } from "@lucide/vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import TransferPanel from "./components/TransferPanel.vue";
import ConfirmDialog from "./components/ConfirmDialog.vue";
import AuditPanel from "./components/AuditPanel.vue";
import PreviewPane from "./components/PreviewPane.vue";
import CustomConfigEditor from "./components/CustomConfigEditor.vue";
import PathField from "./components/PathField.vue";
import QuickPathsMenu from "./components/QuickPathsMenu.vue";
import {
  bindApi,
  baseName,
  call,
  errorMessage,
  isMethodMissing,
  joinPath,
  normalizeEntries,
  parentPath,
  type FileCapabilities,
  type FileEntry,
} from "./lib/api";
import { createTransferTracker, isActive, type TransferJob, type TransferKind } from "./lib/transfers";
import { inspect, type DangerousHit } from "./lib/dangerousPaths";
import { workbenchMessage } from "./lib/i18n";
import { isArchivePath } from "./lib/archive";
import { loadUiPrefs, saveUiPrefs } from "./lib/prefs";
import { sortEntries, toggleSortState, type SortColumn, type SortState } from "./lib/sorting";
import { filterEntries } from "./lib/searchFilter";
import { isLargeDirectory } from "./lib/largeDir";
import { normalizeQuickPaths, type QuickPath } from "./lib/quickPaths";

type ConfirmKind = "delete" | "purge" | "syncDir" | "copyDir" | "newFolder" | "rename" | "copy" | "move" | "extract";
type PaneSide = "left" | "right";
type MenuAction =
  | "open" | "preview" | "download" | "rename" | "delete" | "copyPath"
  | "syncDir" | "copyDir" | "copy" | "move" | "extract" | "archiveContents";

interface ConnectionSummary {
  name?: string;
  host?: string;
  username?: string;
  readOnly?: boolean;
  protocol?: string;
}

// UI 偏好（A-FILES ①/④c）：布局偏好存 localStorage（非敏感）。
const prefs = loadUiPrefs();

const hostContext = ref<Record<string, unknown>>({});
const locale = ref("zh-CN");
const connectionId = computed(() => String(hostContext.value.connectionId || ""));
const connection = computed<ConnectionSummary>(() => {
  const value = hostContext.value.connection;
  return value && typeof value === "object" ? (value as ConnectionSummary) : {};
});
const connectionLabel = computed(() => {
  const host = connection.value.host || connection.value.name || connectionId.value || "storage";
  const user = connection.value.username ? `${connection.value.username}@` : "";
  return `${user}${host}`;
});
// 写权限 = 宿主 context 未标记只读 且 后端策略层未开启只读门禁
// （files/capabilities.readOnly：表单 read_only ∥ 宿主标准 read_only）。
const canWrite = computed(() => !connection.value.readOnly && !capabilities.value?.readOnly);

const t = (key: string, values: Record<string, string | number> = {}) => workbenchMessage(locale.value, key, values);

// ---- 源栏（左栏）-----------------------------------------------------------
const path = ref("/");
const entries = ref<FileEntry[]>([]);
const selection = ref<string[]>([]);
const activePath = ref("");
const sort = ref<SortState>(prefs.sort);
const loading = ref(false);
const error = ref("");
const notice = ref("");
const capabilities = ref<FileCapabilities | undefined>();
const initialized = ref(false);

// ---- 目标栏（右栏，A-FILES ①）---------------------------------------------
const dualPane = ref(prefs.dualPane);
const rightTab = ref<"target" | "preview">(prefs.rightTab);
const rightPath = ref("/");
const rightEntries = ref<FileEntry[]>([]);
const rightSelection = ref<string[]>([]);
const rightActivePath = ref("");
const rightLoading = ref(false);
/** "" = 与左栏同连接；宿主提供连接枚举时可切换其它连接（cross-connection 走 targetConnectionId）。 */
const targetConnectionId = ref("");
const targetConnections = ref<Array<{ id: string; name: string }>>([]);
const dragOverSide = ref<PaneSide | null>(null);

// ---- 快速目录（tiny-rdm quick paths 对标）------------------------------------
// §8.1：后端按协议/根约束/stat 过滤后返回候选（根目录 + fs 协议的用户目录族）；
// 展示形态为路径栏下拉（QuickPathsMenu，替代早期 chips 行，节省一整行空间）。
const leftQuickPaths = ref<QuickPath[]>([]);
const rightQuickPaths = ref<QuickPath[]>([]);

/** files/quickPaths（§8.1）：方法缺失或探针失败时下拉隐藏（旧 sidecar 降级）。 */
async function loadQuickPaths(side: PaneSide) {
  try {
    const params: Record<string, unknown> = {};
    if (side === "right" && targetConnectionId.value) params.connectionId = targetConnectionId.value;
    const result = await call<{ paths: QuickPath[] }>("files/quickPaths", params);
    const list = normalizeQuickPaths(result.paths);
    if (side === "left") leftQuickPaths.value = list;
    else rightQuickPaths.value = list;
  } catch {
    /* 方法缺失或探针失败：隐藏下拉（旧 sidecar 降级） */
  }
}

function navigateQuickPath(side: PaneSide, targetPath: string) {
  if (side === "left") void loadDirectory(targetPath).catch(() => undefined);
  else void loadRightDirectory(targetPath).catch(() => undefined);
}

const dockOpen = ref(true);
const dockTab = ref<"transfers" | "audit" | "connection">("transfers");
const auditRef = ref<InstanceType<typeof AuditPanel>>();

const previewPath = ref<string | null>(null);
const contextMenu = ref<{ x: number; y: number; entry: FileEntry; side: PaneSide }>();

const tracker = createTransferTracker();
const transferJobs = computed(() => Object.values(tracker.jobs));
// P-FILES ①b：等待终态后刷新目录的 job 集（transport=job 的 copy/move/rename）。
const awaitingRefresh = new Set<string>();

/** 登记 sidecar 侧异步 job（提交后 jobId 已返回，首个进度事件未到达前的占位）。 */
function trackSidecarJob(jobId: string, kind: TransferKind, remotePath: string) {
  registerJob({
    jobId,
    connectionId: connectionId.value,
    kind,
    remotePath,
    state: "queued",
    size: 0,
    transferred: 0,
    updatedAt: Date.now(),
  });
  awaitingRefresh.add(jobId);
}

const confirmOpen = ref(false);
const confirmKind = ref<ConfirmKind>();
const confirmTitle = ref("");
const confirmBody = ref("");
const confirmDanger = ref(false);
const confirmHits = ref<DangerousHit[]>([]);
const confirmBusy = ref(false);
const confirmDraft = ref("");
const confirmTarget = ref<{ path?: string; entry?: FileEntry; targets?: FileEntry[] }>({});
const confirmSide = ref<PaneSide>("left");
const confirmInput = computed(() => confirmKind.value === "newFolder" || confirmKind.value === "rename" || confirmKind.value === "syncDir" || confirmKind.value === "copyDir" || confirmKind.value === "copy" || confirmKind.value === "move" || confirmKind.value === "extract");
const confirmLabel = computed(() => {
  if (confirmKind.value === "newFolder") return t("create");
  if (confirmKind.value === "rename") return t("save");
  return t("confirm");
});

const sortedEntries = computed(() => sortEntries(entries.value, sort.value));
// 当前目录文件名过滤（tiny-rdm 对标缺口#7）：仅影响展示，不影响选择/删除语义。
const searchQuery = ref("");
const filteredEntries = computed(() => filterEntries(sortedEntries.value, searchQuery.value));
const rightSorted = computed(() => sortEntries(rightEntries.value, sort.value));
// 右栏同款过滤（双栏对称性修复）：与左栏共用 filterEntries 语义。
const rightSearchQuery = ref("");
const filteredRightEntries = computed(() => filterEntries(rightSorted.value, rightSearchQuery.value));

watch([sort, dualPane, rightTab], () => {
  saveUiPrefs({ sort: sort.value, dualPane: dualPane.value, rightTab: rightTab.value });
}, { deep: true });

// 快速目录 chips 随连接面变化刷新（右栏切连接/开启双栏时重取）。
watch(dualPane, (on) => {
  if (on) void loadQuickPaths("right");
});
watch(targetConnectionId, () => {
  if (dualPane.value) void loadQuickPaths("right");
});

let noticeTimer = 0;
let pollTimer = 0;
let pollingDisabled = false;
let unsubscribeEvent: (() => void) | undefined;
let unsubscribeBinary: (() => void) | undefined;
let unsubscribeLocale: (() => void) | undefined;
let unsubscribeContext: (() => void) | undefined;

function showNotice(message: string) {
  notice.value = message;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => (notice.value = ""), 4000);
}

function showError(cause: unknown) {
  const message = errorMessage(cause);
  if (isMethodMissing(cause)) {
    const method = cause instanceof Error && "method" in cause ? String((cause as { method?: string }).method) : "";
    error.value = t("featureMissing", { method });
    return;
  }
  error.value = t("operationFailed", { error: message });
}

/** 错误横幅上的重试（④ UI 三态：错误可恢复）。 */
async function retryAfterError() {
  error.value = "";
  await loadDirectory().catch(() => undefined);
}

// ---- host bridge ---------------------------------------------------------

async function waitForHostApi(timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  while (!window.dbxPlugin && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));
  if (!window.dbxPlugin) throw new Error(t("hostApiUnavailable"));
  return window.dbxPlugin;
}

function applyAppearance(root: HTMLElement, appearance: { colors?: Record<string, string>; colorScheme?: string }) {
  const colors = appearance.colors ?? {};
  for (const [key, value] of Object.entries(colors)) {
    if (typeof value === "string") root.style.setProperty(`--${key.replace(/([A-Z])/g, "-$1").toLowerCase()}`, value);
  }
  if (appearance.colorScheme) root.dataset.theme = appearance.colorScheme;
}

function handleEvent(event: { method: string; params: Record<string, unknown> }) {
  if (event.method === "files/transfer/progress") {
    const job = tracker.onProgress(event.params as Parameters<typeof tracker.onProgress>[0]);
    // P-FILES ①b：transport=job 的 copy/move/rename 在终态后自动刷新目录。
    if (job && awaitingRefresh.has(job.jobId) && !isActive(job.state)) {
      awaitingRefresh.delete(job.jobId);
      void loadDirectory().catch(() => undefined);
      if (dualPane.value) void loadRightDirectory().catch(() => undefined);
    }
  }
}

interface DownloadChunk {
  offset: number;
  data: Uint8Array;
}

interface FrameWaiter {
  offset: number;
  resolve: (chunk: DownloadChunk) => void;
  reject: (cause: Error) => void;
  timer: number;
}

const frameQueue = new Map<string, DownloadChunk[]>();
const frameWaiters = new Map<string, FrameWaiter[]>();

function handleBinary(event: { channel: string; dataBase64: string }) {
  if (!event.channel.startsWith("files/download/")) return;
  const data = window.dbxPlugin.decodeBase64(event.dataBase64);
  if (data.byteLength < 8) return;
  const offset = Number(new DataView(data.buffer, data.byteOffset, 8).getBigUint64(0, false));
  const chunk: DownloadChunk = { offset, data: data.slice(8) };
  const waiters = frameWaiters.get(event.channel);
  if (waiters?.length) {
    const waiter = waiters.shift()!;
    waiter.resolve(chunk);
    return;
  }
  const queue = frameQueue.get(event.channel) ?? [];
  queue.push(chunk);
  frameQueue.set(event.channel, queue);
}

function waitForFrame(channel: string, offset: number, timeoutMs = 30_000): Promise<DownloadChunk> {
  const queued = frameQueue.get(channel);
  if (queued?.length) {
    const index = queued.findIndex((chunk) => chunk.offset === offset);
    if (index >= 0) return Promise.resolve(queued.splice(index, 1)[0]);
  }
  return new Promise((resolve, reject) => {
    const waiter: FrameWaiter = {
      offset,
      resolve: (chunk) => {
        window.clearTimeout(waiter.timer);
        resolve(chunk);
      },
      reject: (cause) => {
        window.clearTimeout(waiter.timer);
        reject(cause);
      },
      timer: 0,
    };
    waiter.timer = window.setTimeout(() => {
      const pending = frameWaiters.get(channel) ?? [];
      const position = pending.findIndex((entry) => entry.offset === offset);
      if (position >= 0) pending.splice(position, 1);
      reject(new Error(`download frame timeout at offset ${offset}`));
    }, timeoutMs);
    const waiters = frameWaiters.get(channel) ?? [];
    waiters.push(waiter);
    frameWaiters.set(channel, waiters);
  });
}

function releaseFrames(channel: string) {
  frameQueue.delete(channel);
  const waiters = frameWaiters.get(channel);
  if (waiters) {
    for (const waiter of waiters) waiter.reject(new Error("download channel released"));
    frameWaiters.delete(channel);
  }
}

// ---- directory -----------------------------------------------------------

async function fetchListing(target: string, explicitConnectionId?: string) {
  const params: Record<string, unknown> = { path: target };
  if (explicitConnectionId) params.connectionId = explicitConnectionId;
  const result = await call<{ entries: FileEntry[] }>("files/list", params);
  return normalizeEntries(result.entries ?? []);
}

async function loadDirectory(target?: string) {
  const next = target ?? path.value;
  loading.value = true;
  try {
    entries.value = await fetchListing(next);
    path.value = next;
    selection.value = [];
    activePath.value = "";
    error.value = "";
    // 大目录提示（第 3 轮）：浏览仍走全量 files/list（排序/过滤/全选语义
    // 不回归），仅当条目数达阈值时提示用户列表已虚拟滚动（largeDir.ts 记录
    // 了不切 listPaged 的 bench 依据）。
    if (isLargeDirectory(entries.value.length)) showNotice(t("largeDirectory", { count: entries.value.length }));
  } catch (cause) {
    showError(cause);
    throw cause;
  } finally {
    loading.value = false;
  }
}

async function loadRightDirectory(target?: string) {
  const next = target ?? rightPath.value;
  rightLoading.value = true;
  try {
    rightEntries.value = await fetchListing(next, targetConnectionId.value || undefined);
    rightPath.value = next;
    rightSelection.value = [];
    rightActivePath.value = "";
    if (isLargeDirectory(rightEntries.value.length)) showNotice(t("largeDirectory", { count: rightEntries.value.length }));
  } catch (cause) {
    showError(cause);
    throw cause;
  } finally {
    rightLoading.value = false;
  }
}

async function refreshDirectory() {
  try {
    await loadDirectory();
    // 大目录时 loadDirectory 已给出 largeDirectory 提示，避免被 refreshed 覆盖。
    if (!isLargeDirectory(entries.value.length)) showNotice(t("refreshed"));
  } catch {
    /* banner already shows the error */
  }
}

async function refreshRightDirectory() {
  try {
    await loadRightDirectory();
    if (!isLargeDirectory(rightEntries.value.length)) showNotice(t("refreshed"));
  } catch {
    /* banner already shows the error */
  }
}

function openPreview(target: string) {
  previewPath.value = target;
  if (!dualPane.value) dualPane.value = true;
  rightTab.value = "preview";
}

async function openEntry(entry: FileEntry, side: PaneSide = "left") {
  if (entry.kind === "directory") {
    try {
      if (side === "left") await loadDirectory(entry.path);
      else await loadRightDirectory(entry.path);
    } catch {
      /* keep current listing */
    }
    return;
  }
  openPreview(entry.path);
}

function toggleSort(column: SortColumn) {
  sort.value = toggleSortState(sort.value, column);
}

async function loadCapabilities() {
  try {
    capabilities.value = await call<FileCapabilities>("files/capabilities");
  } catch (cause) {
    if (!isMethodMissing(cause)) showError(cause);
  }
}

/**
 * A-FILES ①：探测宿主是否提供连接枚举（host.listConnections）。
 * 宿主未提供时目标栏仅支持「同连接另一路径」；跨连接能力见交接文档。
 */
async function probeConnections() {
  try {
    const list = await window.dbxPlugin.request<Array<Record<string, unknown>>>("host.listConnections");
    if (Array.isArray(list)) {
      targetConnections.value = list
        .map((item) => ({ id: String(item.id ?? item.connectionId ?? ""), name: String(item.name ?? item.id ?? item.connectionId ?? "") }))
        .filter((item) => item.id && item.id !== connectionId.value);
    }
  } catch {
    targetConnections.value = [];
  }
}

// ---- dialogs ---------------------------------------------------------------

function openConfirm(kind: ConfirmKind, options: {
  title: string;
  body?: string;
  danger?: boolean;
  hits?: DangerousHit[];
  target?: { path?: string; entry?: FileEntry; targets?: FileEntry[] };
  draft?: string;
  side?: PaneSide;
}) {
  confirmKind.value = kind;
  confirmTitle.value = options.title;
  confirmBody.value = options.body ?? "";
  confirmDanger.value = Boolean(options.danger);
  confirmHits.value = options.hits ?? [];
  confirmTarget.value = options.target ?? {};
  confirmDraft.value = options.draft ?? "";
  confirmSide.value = options.side ?? "left";
  confirmOpen.value = true;
}

function closeConfirm() {
  confirmOpen.value = false;
  confirmBusy.value = false;
}

function startNewFolder() {
  confirmDraft.value = "";
  openConfirm("newFolder", { title: t("newFolderTitle"), draft: "" });
}

function startRename(entry: FileEntry, side: PaneSide) {
  confirmDraft.value = entry.name;
  openConfirm("rename", { title: t("renameTitle"), target: { entry }, draft: entry.name, side });
}

function startDelete(targets: FileEntry[], side: PaneSide = "left") {
  if (!targets.length) return;
  const inspection = inspect("delete", { targets: targets.map((entry) => ({ path: entry.path, kind: entry.kind })) });
  const dirs = targets.filter((entry) => entry.kind === "directory");
  if (dirs.length) {
    // 目录删除即递归清空：走 purge 规则确认
    const worst = dirs.reduce<{ level: "none" | "warn" | "danger"; hits: DangerousHit[] }>(
      (acc, dir) => {
        const result = inspect("purge", { path: dir.path });
        return result.level === "danger" || acc.level === "danger" ? { level: "danger", hits: [...acc.hits, ...result.hits] } : result;
      },
      { level: "none", hits: [] },
    );
    openConfirm("delete", {
      title: t("deleteTitle", { count: targets.length }),
      body: `${t("deleteBody")} ${t("deleteRecursiveWarn")}`,
      danger: worst.level === "danger",
      hits: worst.hits,
      target: { targets },
      side,
    });
    return;
  }
  openConfirm("delete", {
    title: t("deleteTitle", { count: targets.length }),
    body: inspection.level === "none" ? t("deleteBody") : `${t("deleteBody")} ${t("deleteRecursiveWarn")}`,
    danger: inspection.level === "danger",
    hits: inspection.hits,
    target: { targets },
    side,
  });
}

function startDirJob(kind: "syncDir" | "copyDir", entry: FileEntry, side: PaneSide) {
  const defaultTarget = joinPath("/", `${baseName(entry.path) || "copy"}`);
  openConfirm(kind, {
    title: kind === "syncDir" ? t("syncDirTitle", { path: defaultTarget }) : t("copyDirTitle", { path: defaultTarget }),
    body: kind === "syncDir" ? t("syncDirBody") : t("copyDirBody"),
    danger: true,
    target: { entry },
    draft: defaultTarget,
    side,
  });
}

/** P-FILES ①b：copy/move 动作（native 同步返回 / 降级 jobId 由后端决定）。 */
function startCopyMove(kind: "copy" | "move", entry: FileEntry, side: PaneSide) {
  const parent = parentPath(entry.path);
  const draft = kind === "copy"
    ? joinPath(parent, `${entry.name}-copy`)
    : joinPath("/", entry.name);
  openConfirm(kind, {
    title: kind === "copy" ? t("copyTitle") : t("moveTitle"),
    body: kind === "copy" ? t("copyBody") : t("moveBody"),
    target: { entry },
    draft,
    side,
  });
}

/** A-FILES ③：压缩包解压骨架——等后端 files/extract（交接），方法未注册时给出 featureMissing 提示。 */
function startExtract(entry: FileEntry, side: PaneSide) {
  openConfirm("extract", {
    title: t("extractTitle", { name: entry.name }),
    body: t("extractBody"),
    target: { entry },
    draft: parentPath(entry.path) || "/",
    side,
  });
}

/** 指定栏的调用：右栏且选择了其它连接时显式带 connectionId（同连接时走默认注入）。 */
function callFor<T = Record<string, unknown>>(side: PaneSide, method: string, params: Record<string, unknown> = {}): Promise<T> {
  if (side === "right" && targetConnectionId.value) params.connectionId = targetConnectionId.value;
  return call<T>(method, params);
}

async function onConfirm() {
  if (!confirmKind.value) return;
  confirmBusy.value = true;
  const side = confirmSide.value;
  // transport=job 的 copy/move/rename/syncDir/copyDir：job 已登记、终态后
  // 由事件驱动刷新（handleEvent），跳过立即刷新。
  let jobStarted = false;
  try {
    switch (confirmKind.value) {
      case "newFolder": {
        const name = confirmDraft.value.trim();
        if (!name) return;
        await call("files/mkdir", { path: joinPath(path.value, name) });
        showNotice(t("folderCreated"));
        break;
      }
      case "rename": {
        const entry = confirmTarget.value.entry;
        const name = confirmDraft.value.trim();
        if (!entry || !name || name === entry.name) return;
        const result = await callFor<{ transport?: string; jobId?: string | null }>(side, "files/rename", {
          path: entry.path,
          newPath: joinPath(parentPath(entry.path), name),
        });
        if (result.transport === "job" && result.jobId) {
          // 目录 rename 降级（P-FILES ②）：等终态再刷新。
          trackSidecarJob(result.jobId, "rename", `${entry.path} → ${joinPath(parentPath(entry.path), name)}`);
          jobStarted = true;
        }
        showNotice(t("renamed"));
        break;
      }
      case "copy":
      case "move": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        if (targetPath.replace(/\/+$/, "") === entry.path.replace(/\/+$/, "")) {
          error.value = t("operationFailed", { error: t("destMustDiffer") });
          return;
        }
        const result = await callFor<{ success: boolean; transport?: string; jobId?: string | null }>(side, `files/${confirmKind.value}`, {
          sourcePath: entry.path,
          targetPath,
        });
        if (result.transport === "job" && result.jobId) {
          trackSidecarJob(result.jobId, confirmKind.value, `${entry.path} → ${targetPath}`);
          jobStarted = true;
        }
        showNotice(t("jobStarted", { name: baseName(targetPath) }));
        break;
      }
      case "delete": {
        const targets = confirmTarget.value.targets ?? [];
        for (const target of targets) {
          if (target.kind === "directory") await callFor(side, "files/purge", { path: target.path });
          else await callFor(side, "files/delete", { path: target.path });
        }
        showNotice(t("deleted"));
        break;
      }
      case "purge": {
        const target = confirmTarget.value.path;
        if (!target) return;
        await call("files/purge", { path: target });
        showNotice(t("deleted"));
        break;
      }
      case "syncDir":
      case "copyDir": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        const result = await callFor<{ jobId: string }>(side, `files/${confirmKind.value}`, {
          sourcePath: entry.path,
          targetPath,
        });
        if (result.jobId) trackSidecarJob(result.jobId, confirmKind.value, `${entry.path} → ${targetPath}`);
        jobStarted = true;
        showNotice(t("jobStarted", { name: baseName(targetPath) }));
        break;
      }
      case "extract": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        // files/extract 为后端交接方法：未实现时走 featureMissing 提示，
        // 实现后（transport=job）复用 copy/move 的 job 进度语义。
        const result = await callFor<{ success?: boolean; transport?: string; jobId?: string | null }>(side, "files/extract", {
          path: entry.path,
          targetPath,
        });
        if (result.transport === "job" && result.jobId) {
          trackSidecarJob(result.jobId, "copy", `${entry.path} → ${targetPath}`);
          jobStarted = true;
        }
        showNotice(t("jobStarted", { name: baseName(targetPath) }));
        break;
      }
    }
    closeConfirm();
    if (!jobStarted) {
      await loadDirectory().catch(() => undefined);
      if (side === "right" && dualPane.value) await loadRightDirectory().catch(() => undefined);
    }
  } catch (cause) {
    showError(cause);
    closeConfirm();
  } finally {
    confirmBusy.value = false;
  }
}

// ---- cross-pane transfers（A-FILES ①）-----------------------------------------

function pickSideEntries(side: PaneSide, paths: string[]): FileEntry[] {
  const pool = side === "left" ? sortedEntries.value : rightSorted.value;
  return pool.filter((entry) => paths.includes(entry.path));
}

/**
 * 跨栏 copy/move：同一连接直接传路径；跨连接（目标栏选择了其它连接）时
 * 契约字段 targetConnectionId / sourceConnectionId 由方向决定。
 */
async function transferBetween(from: PaneSide, move: boolean, dragged?: FileEntry[]) {
  const to: PaneSide = from === "left" ? "right" : "left";
  const paths = dragged ? dragged.map((entry) => entry.path) : from === "left" ? selection.value : rightSelection.value;
  const list = dragged ?? pickSideEntries(from, paths);
  if (!list.length) return;
  const destPath = to === "left" ? path.value : rightPath.value;
  const sourceConnectionId = from === "right" && targetConnectionId.value ? targetConnectionId.value : undefined;
  const targetConnection = to === "right" && targetConnectionId.value ? targetConnectionId.value : undefined;
  try {
    for (const item of list) {
      const params: Record<string, unknown> = {
        sourcePath: item.path,
        targetPath: joinPath(destPath, item.name),
      };
      if (sourceConnectionId) params.sourceConnectionId = sourceConnectionId;
      if (targetConnection) params.targetConnectionId = targetConnection;
      const result = await call<{ transport?: string; jobId?: string | null }>(`files/${move ? "move" : "copy"}`, params);
      if (result.transport === "job" && result.jobId) {
        trackSidecarJob(result.jobId, move ? "move" : "copy", `${item.path} → ${String(params.targetPath)}`);
      }
    }
    showNotice(t("paneTransferred", { count: list.length }));
    if (to === "left") await loadDirectory().catch(() => undefined);
    else await loadRightDirectory().catch(() => undefined);
    if (move && from === "right") await loadRightDirectory().catch(() => undefined);
  } catch (cause) {
    showError(cause);
  }
}

// ---- drag & drop（A-FILES ①）---------------------------------------------------

function onDropTo(side: PaneSide, event: DragEvent) {
  dragOverSide.value = null;
  const raw = event.dataTransfer?.getData("application/x-dbx-files");
  if (!raw) return;
  let payload: { paneId?: string; paths?: string[] };
  try {
    payload = JSON.parse(raw);
  } catch {
    return;
  }
  const from = payload.paneId === "left" ? "left" : payload.paneId === "right" ? "right" : null;
  if (!from || from === side || !payload.paths?.length) return;
  void transferBetween(from, false, pickSideEntries(from, payload.paths));
}

// ---- transfers ---------------------------------------------------------------

const CHUNK_SIZE = 256 * 1024;

function registerJob(job: TransferJob) {
  tracker.register(job);
}

function writeU64(bytes: Uint8Array, value: number) {
  new DataView(bytes.buffer).setBigUint64(0, BigInt(value), false);
}

async function uploadSource(name: string, size: number, readChunk: (offset: number, length: number) => Promise<Uint8Array>) {
  const remotePath = joinPath(path.value, name);
  const start = await call<{ taskId: string; chunkSize?: number }>("files/upload/start", { remotePath, size });
  const taskId = start.taskId;
  const chunkSize = start.chunkSize && start.chunkSize > 0 ? start.chunkSize : CHUNK_SIZE;
  registerJob({
    jobId: taskId,
    taskId,
    connectionId: connectionId.value,
    kind: "upload",
    remotePath,
    state: "running",
    size,
    transferred: 0,
    updatedAt: Date.now(),
  });
  try {
    let offset = 0;
    while (offset < size) {
      const chunk = await readChunk(offset, chunkSize);
      if (!chunk.byteLength) throw new Error("local file ended before its declared size");
      const payload = new Uint8Array(8 + chunk.byteLength);
      writeU64(payload, offset);
      payload.set(chunk, 8);
      await window.dbxPlugin.sendBinary(`files/upload/${taskId}`, payload);
      offset += chunk.byteLength;
      const job = tracker.jobs[taskId];
      if (job) {
        job.transferred = offset;
        job.updatedAt = Date.now();
      }
    }
    await window.dbxPlugin.invoke("files/upload/finish", { taskId }, { timeoutMs: 30 * 60 * 1000 });
    const job = tracker.jobs[taskId];
    if (job) {
      job.state = "completed";
      job.transferred = size;
      job.updatedAt = Date.now();
    }
  } catch (cause) {
    const job = tracker.jobs[taskId];
    if (job) {
      job.state = "failed";
      job.error = errorMessage(cause);
      job.updatedAt = Date.now();
    }
    await window.dbxPlugin.invoke("files/transfer/cancel", { taskId }).catch(() => undefined);
    throw cause;
  }
}

async function uploadLocalFiles(files: readonly File[]) {
  for (const file of files) {
    try {
      await uploadSource(file.name, file.size, async (offset, length) =>
        new Uint8Array(await file.slice(offset, offset + length).arrayBuffer()),
      );
    } catch (cause) {
      showError(cause);
    }
  }
  await loadDirectory().catch(() => undefined);
  if (files.length) showNotice(t("uploaded", { count: files.length }));
}

async function uploadHostFiles(files: Array<{ handleId: string; name: string; size: number }>) {
  const fileTransfer = window.dbxPlugin.fileTransfer;
  if (!fileTransfer) return;
  for (const file of files) {
    try {
      await uploadSource(file.name, file.size, async (offset, length) => {
        const result = await fileTransfer.read(file.handleId, offset, length);
        return window.dbxPlugin.decodeBase64(result.dataBase64);
      });
    } catch (cause) {
      showError(cause);
    } finally {
      await fileTransfer.cancel(file.handleId).catch(() => undefined);
    }
  }
  await loadDirectory().catch(() => undefined);
  if (files.length) showNotice(t("uploaded", { count: files.length }));
}

async function onUpload(files: File[] | null) {
  if (!canWrite.value) return;
  if (files === null) {
    const fileTransfer = window.dbxPlugin.fileTransfer;
    if (!fileTransfer) return;
    try {
      const picked = await fileTransfer.pick({ multiple: true });
      await uploadHostFiles(picked.files);
    } catch (cause) {
      showError(cause);
    }
    return;
  }
  await uploadLocalFiles(files);
}

async function downloadEntry(entry: FileEntry, side: PaneSide = "left") {
  const fileTransfer = window.dbxPlugin.fileTransfer;
  try {
    const startParams: Record<string, unknown> = { remotePath: entry.path };
    if (side === "right" && targetConnectionId.value) startParams.connectionId = targetConnectionId.value;
    const info = await call<{ taskId: string; size: number; fileName?: string; chunkSize?: number }>("files/download/start", startParams);
    const taskId = info.taskId;
    const size = info.size;
    const channel = `files/download/${taskId}`;
    releaseFrames(channel);
    registerJob({
      jobId: taskId,
      taskId,
      connectionId: connectionId.value,
      kind: "download",
      remotePath: entry.path,
      state: "running",
      size,
      transferred: 0,
      updatedAt: Date.now(),
    });
    // 泵式下载：sidecar 在 start 后自行按 offset 顺序推送
    // files/download/{taskId} 帧（8 字节 BE offset + <=256KiB），前端只收帧。
    const target = fileTransfer ? await fileTransfer.beginSave({ name: info.fileName ?? entry.name, size }) : undefined;
    const chunks = target ? undefined : ([] as Uint8Array[]);
    let offset = 0;
    while (offset < size) {
      const chunk = await waitForFrame(channel, offset);
      if (!chunk.data.byteLength) throw new Error("download frame carried no data");
      if (chunks) {
        chunks.push(chunk.data);
      } else if (target) {
        const write = await fileTransfer!.write(target.handleId, chunk.offset, chunk.data);
        offset = Math.max(offset + chunk.data.byteLength, write.nextOffset);
      }
      if (!target && chunks) offset += chunk.data.byteLength;
      const job = tracker.jobs[taskId];
      if (job) {
        job.transferred = offset;
        job.updatedAt = Date.now();
      }
    }
    if (target) {
      await fileTransfer!.finish(target.handleId);
    } else if (chunks) {
      saveBrowserDownload(chunks, info.fileName ?? entry.name);
    }
    await window.dbxPlugin.invoke("files/download/finish", { taskId });
    releaseFrames(channel);
    const job = tracker.jobs[taskId];
    if (job) {
      job.state = "completed";
      job.transferred = size;
      job.updatedAt = Date.now();
    }
    showNotice(t("downloaded", { name: info.fileName ?? entry.name }));
  } catch (cause) {
    showError(cause);
  }
}

function saveBrowserDownload(chunks: Uint8Array[], fileName: string) {
  const blob = new Blob(chunks as unknown as BlobPart[]);
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = fileName;
  anchor.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

async function downloadSelection() {
  const files = sortedEntries.value.filter((entry) => selection.value.includes(entry.path) && entry.kind === "file");
  for (const entry of files) {
    await downloadEntry(entry);
  }
}

function onPreviewDownload(target: string | null) {
  if (!target) return;
  const entry = sortedEntries.value.find((item) => item.path === target)
    ?? rightSorted.value.find((item) => item.path === target);
  if (entry) void downloadEntry(entry, rightSorted.value.some((item) => item.path === target) ? "right" : "left");
}

function onPreviewSaved() {
  void loadDirectory().catch(() => undefined);
  if (dualPane.value) void loadRightDirectory().catch(() => undefined);
  showNotice(t("saved"));
}

async function cancelTransfer(jobId: string) {
  try {
    await call("files/transfer/cancel", { taskId: jobId });
    showNotice(t("jobCanceled"));
  } catch (cause) {
    showError(cause);
  }
  void tracker.refresh(invokeAdapter);
}

function invokeAdapter<T>(method: string, params?: unknown) {
  return window.dbxPlugin.invoke<T>(method, params);
}

// ---- context menu（A-FILES ④b：统一动作面）------------------------------------

function menuAction(action: MenuAction) {
  const menu = contextMenu.value;
  contextMenu.value = undefined;
  if (!menu) return;
  const { entry, side } = menu;
  switch (action) {
    case "open":
      void openEntry(entry, side);
      break;
    case "preview":
    case "archiveContents":
      openPreview(entry.path);
      break;
    case "download":
      if (entry.kind === "file") void downloadEntry(entry, side);
      break;
    case "rename":
      startRename(entry, side);
      break;
    case "delete":
      startDelete([entry], side);
      break;
    case "copyPath":
      void window.dbxPlugin.clipboard?.writeText(entry.path).then(() => showNotice(t("copiedPath")));
      break;
    case "syncDir":
    case "copyDir":
      startDirJob(action, entry, side);
      break;
    case "copy":
    case "move":
      startCopyMove(action, entry, side);
      break;
    case "extract":
      startExtract(entry, side);
      break;
  }
}

// ---- lifecycle -----------------------------------------------------------------

async function initialize() {
  const api = await waitForHostApi();
  try {
    hostContext.value = await Promise.any([api.ready, api.request<Record<string, unknown>>("host.getContext")]);
  } catch {
    hostContext.value = api.context ?? {};
  }
  locale.value = api.locale || "zh-CN";
  if (api.appearance) applyAppearance(document.documentElement, api.appearance);
  unsubscribeLocale = api.onLocaleChange?.((next) => (locale.value = next || "zh-CN"));
  unsubscribeContext = api.onContextChange?.((context) => {
    hostContext.value = context;
  });
  unsubscribeEvent = api.onEvent(handleEvent);
  unsubscribeBinary = api.onBinary(handleBinary);
  bindApi((method, params, options) => api.invoke(method, params, options), connectionId.value || null);
  initialized.value = true;
  await loadCapabilities();
  await loadDirectory("/").catch(() => undefined);
  if (dualPane.value) await loadRightDirectory("/").catch(() => undefined);
  void loadQuickPaths("left");
  if (dualPane.value) void loadQuickPaths("right");
  void probeConnections();
  pollTimer = window.setInterval(() => {
    if (pollingDisabled) return;
    tracker
      .refresh(invokeAdapter, connectionId.value)
      .catch((cause) => {
        if (isMethodMissing(cause)) pollingDisabled = true;
      });
  }, 5000);
}

function onContextClick() {
  contextMenu.value = undefined;
}

function onToolbarNavigate(target: string) {
  void loadDirectory(target).catch(() => undefined);
}

watch(dockTab, (tab) => {
  if (tab === "audit") auditRef.value?.refresh();
});

onMounted(() => {
  document.addEventListener("click", onContextClick);
  void initialize().catch((cause) => {
    error.value = t("operationFailed", { error: errorMessage(cause) });
  });
});

onBeforeUnmount(() => {
  document.removeEventListener("click", onContextClick);
  window.clearTimeout(noticeTimer);
  window.clearInterval(pollTimer);
  unsubscribeEvent?.();
  unsubscribeBinary?.();
  unsubscribeLocale?.();
  unsubscribeContext?.();
});
</script>

<template>
  <main class="wb-workbench">
    <div v-if="error" class="wb-error-banner">
      <span>{{ error }}</span>
      <button class="wb-icon-button wb-icon-neutral" :title="t('retry')" @click="retryAfterError">↻</button>
      <button class="wb-icon-button wb-icon-neutral" @click="error = ''">✕</button>
    </div>
    <div v-if="notice" class="wb-notice">{{ notice }}</div>

    <FileToolbar
      :can-write="canWrite"
      :busy="loading"
      :has-selection="selection.length > 0"
      :dock-open="dockOpen"
      :dock-tab="dockTab"
      :dual-pane="dualPane"
      :t="t"
      @new-folder="startNewFolder"
      @upload="onUpload"
      @download="downloadSelection"
      @delete="startDelete(sortedEntries.filter((entry) => selection.includes(entry.path)))"
      @toggle-dual-pane="dualPane = !dualPane"
      @toggle-dock="(tab) => { if (dockOpen && dockTab === tab) dockOpen = false; else { dockOpen = true; dockTab = tab; if (tab === 'audit') auditRef?.refresh(); } }"
    />

    <div class="wb-content">
      <!-- 源栏（左栏）：当前连接浏览 -->
      <section class="wb-pane wb-pane-source" @dragover.prevent @dragenter="dragOverSide = 'left'" @dragleave="dragOverSide = dragOverSide === 'left' ? null : dragOverSide" @drop.prevent="onDropTo('left', $event)">
        <!-- 与右栏 tabs 等高的空条：双栏模式下保证两侧工具行对齐 -->
        <div v-if="dualPane" class="wb-pane-tabs wb-pane-tabs-ghost" aria-hidden="true"></div>
        <!-- 左栏工具行（双栏对称性修复：路径行从全局工具栏移入栏内，与右栏同构） -->
        <div class="wb-pane-header">
          <button class="wb-icon-button wb-icon-neutral" :title="t('up')" :disabled="!path || path === '/'" @click="onToolbarNavigate(parentPath(path))"><ArrowUp /></button>
          <button class="wb-icon-button wb-icon-neutral" :title="t('refresh')" :disabled="loading" @click="refreshDirectory"><RefreshCw :class="{ 'wb-spin': loading }" /></button>
          <div class="wb-path-toolbar">
            <PathField :path="path" :t="t" @navigate="onToolbarNavigate" />
            <!-- 快速目录下拉（fs：主目录族 + 根目录；其它协议：仅根目录） -->
            <QuickPathsMenu
              v-if="leftQuickPaths.length"
              :paths="leftQuickPaths"
              :current-path="path"
              :t="t"
              @navigate="navigateQuickPath('left', $event)"
            />
            <span class="wb-search-box">
              <Search class="wb-search-icon" aria-hidden="true" />
              <input
                :value="searchQuery"
                class="wb-search-input"
                :placeholder="t('searchPlaceholder')"
                type="search"
                spellcheck="false"
                @input="searchQuery = ($event.target as HTMLInputElement).value"
                @keydown.esc.prevent="searchQuery = ''"
              />
            </span>
          </div>
        </div>
        <FileTable
          pane-id="left"
          :entries="filteredEntries"
          :selection="selection"
          :active-path="activePath"
          :sort="sort"
          :loading="loading"
          :t="t"
          @update:selection="selection = $event"
          @update:active-path="activePath = $event"
          @open="(entry) => openEntry(entry, 'left')"
          @contextmenu="(payload) => (contextMenu = { ...payload, side: 'left' })"
          @sort="toggleSort"
        />
        <div v-if="dragOverSide === 'left' && dualPane" class="wb-drop-overlay">{{ t("dropToCopy") }}</div>
      </section>

      <!-- 双栏桥：跨栏 copy/move 按钮（A-FILES ①） -->
      <div v-if="dualPane" class="wb-pane-bridge">
        <button class="wb-icon-button wb-icon-neutral" :title="t('copyToTarget')" :disabled="!selection.length" @click="transferBetween('left', false)"><Copy /></button>
        <button class="wb-icon-button wb-icon-neutral" :title="t('moveToTarget')" :disabled="!selection.length || !canWrite" @click="transferBetween('left', true)"><ArrowRight /></button>
        <span class="wb-toolbar-separator" />
        <button class="wb-icon-button wb-icon-neutral" :title="t('copyToSource')" :disabled="!rightSelection.length" @click="transferBetween('right', false)"><Copy class="wb-flip-h" /></button>
        <button class="wb-icon-button wb-icon-neutral" :title="t('moveToSource')" :disabled="!rightSelection.length" @click="transferBetween('right', true)"><ArrowLeft /></button>
      </div>

      <!-- 目标栏（右栏）：目标连接浏览 / 预览 Tab -->
      <section
        v-if="dualPane"
        class="wb-pane wb-pane-target"
        @dragover.prevent
        @dragenter="dragOverSide = 'right'"
        @dragleave="dragOverSide = dragOverSide === 'right' ? null : dragOverSide"
        @drop.prevent="onDropTo('right', $event)"
      >
        <div class="wb-pane-tabs">
          <button :class="{ 'is-active': rightTab === 'target' }" @click="rightTab = 'target'">{{ t("targetPane") }}</button>
          <button :class="{ 'is-active': rightTab === 'preview' }" @click="rightTab = 'preview'">{{ t("previewTitle") }}</button>
        </div>
        <template v-if="rightTab === 'target'">
          <div class="wb-pane-header">
            <button class="wb-icon-button wb-icon-neutral" :title="t('up')" :disabled="!rightPath || rightPath === '/'" @click="loadRightDirectory(parentPath(rightPath))"><ArrowUp /></button>
            <button class="wb-icon-button wb-icon-neutral" :title="t('refresh')" :disabled="rightLoading" @click="refreshRightDirectory"><RefreshCw :class="{ 'wb-spin': rightLoading }" /></button>
            <!-- 与左栏工具栏同款：面包屑 + 路径输入 + 过滤框（双栏对称性修复） -->
            <div class="wb-path-toolbar">
              <PathField :path="rightPath" :t="t" @navigate="(target) => loadRightDirectory(target)" />
              <!-- 与左栏同款快速目录下拉（右栏切连接时数据面已随 loadQuickPaths 刷新） -->
              <QuickPathsMenu
                v-if="rightQuickPaths.length"
                :paths="rightQuickPaths"
                :current-path="rightPath"
                :t="t"
                @navigate="navigateQuickPath('right', $event)"
              />
              <span class="wb-search-box">
                <Search class="wb-search-icon" aria-hidden="true" />
                <input
                  :value="rightSearchQuery"
                  class="wb-search-input"
                  :placeholder="t('searchPlaceholder')"
                  type="search"
                  spellcheck="false"
                  @input="rightSearchQuery = ($event.target as HTMLInputElement).value"
                  @keydown.esc.prevent="rightSearchQuery = ''"
                />
              </span>
            </div>
            <select v-if="targetConnections.length" v-model="targetConnectionId" class="wb-target-connection" :title="t('targetConnection')" @change="loadRightDirectory(rightPath)">
              <option value="">{{ t("sameConnection") }}</option>
              <option v-for="item in targetConnections" :key="item.id" :value="item.id">{{ item.name }}</option>
            </select>
          </div>
          <FileTable
            pane-id="right"
            :entries="filteredRightEntries"
            :selection="rightSelection"
            :active-path="rightActivePath"
            :sort="sort"
            :loading="rightLoading"
            :t="t"
            @update:selection="rightSelection = $event"
            @update:active-path="rightActivePath = $event"
            @open="(entry) => openEntry(entry, 'right')"
            @contextmenu="(payload) => (contextMenu = { ...payload, side: 'right' })"
            @sort="toggleSort"
          />
        </template>
        <PreviewPane
          v-else
          :path="previewPath"
          :can-write="canWrite"
          :t="t"
          @close="previewPath = null"
          @saved="onPreviewSaved"
          @download="onPreviewDownload"
        />
        <div v-if="dragOverSide === 'right' && rightTab === 'target'" class="wb-drop-overlay">{{ t("dropToCopy") }}</div>
      </section>

      <aside v-if="dockOpen" class="wb-dock">
        <div class="wb-dock-tabs">
          <button :class="{ 'is-active': dockTab === 'transfers' }" @click="dockTab = 'transfers'">{{ t("transferPanel") }}</button>
          <button :class="{ 'is-active': dockTab === 'audit' }" @click="dockTab = 'audit'">{{ t("auditPanel") }}</button>
          <button :class="{ 'is-active': dockTab === 'connection' }" @click="dockTab = 'connection'">{{ t("connectionPanel") }}</button>
        </div>
        <div class="wb-dock-body">
          <TransferPanel v-if="dockTab === 'transfers'" :jobs="transferJobs" :t="t" @cancel="cancelTransfer" />
          <AuditPanel v-else-if="dockTab === 'audit'" ref="auditRef" :t="t" />
          <div v-else style="display: flex; flex-direction: column; gap: 10px">
            <div class="wb-transfer-item">
              <div class="wb-transfer-title"><strong>{{ connectionLabel }}</strong></div>
              <div class="wb-transfer-meta">
                <span>{{ connection.protocol ?? "" }}</span>
                <span v-if="connection.readOnly || capabilities?.readOnly" class="wb-readonly-badge">{{ t("readOnly") }}</span>
              </div>
            </div>
            <div class="wb-muted" style="font-size: 11px">{{ t("customConfigTitle") }}</div>
            <CustomConfigEditor :t="t" @notice="showNotice" @error="showError" />
          </div>
        </div>
      </aside>
    </div>

    <!-- 统一右键菜单（A-FILES ④b）：源栏/目标栏共用 -->
    <div v-if="contextMenu" class="wb-context-menu" :style="{ left: `${contextMenu.x}px`, top: `${contextMenu.y}px` }" @click.stop>
      <button v-if="contextMenu.entry.kind === 'directory'" @click="menuAction('open')">{{ t("openDirectory") }}</button>
      <button v-if="contextMenu.entry.kind === 'file' && !isArchivePath(contextMenu.entry.path)" @click="menuAction('preview')">{{ t("preview") }}</button>
      <button v-if="contextMenu.entry.kind === 'file' && isArchivePath(contextMenu.entry.path)" @click="menuAction('archiveContents')">{{ t("archiveContents") }}</button>
      <button v-if="contextMenu.entry.kind === 'file'" @click="menuAction('download')">{{ t("download") }}</button>
      <button v-if="contextMenu.entry.kind === 'file' && isArchivePath(contextMenu.entry.path) && canWrite" @click="menuAction('extract')">{{ t("extractTo") }}</button>
      <button v-if="contextMenu.entry.kind === 'directory' && canWrite" @click="menuAction('syncDir')">{{ t("transferKind.syncDir") }}…</button>
      <button v-if="contextMenu.entry.kind === 'directory' && canWrite" @click="menuAction('copyDir')">{{ t("transferKind.copyDir") }}…</button>
      <hr />
      <button v-if="canWrite" @click="menuAction('copy')">{{ t("transferKind.copy") }}…</button>
      <button v-if="canWrite" @click="menuAction('move')">{{ t("transferKind.move") }}…</button>
      <button v-if="canWrite" @click="menuAction('rename')">{{ t("rename") }}</button>
      <button class="is-danger" :disabled="!canWrite" @click="menuAction('delete')">{{ t("delete") }}</button>
      <hr />
      <button @click="menuAction('copyPath')">{{ t("copyPath") }}</button>
    </div>

    <ConfirmDialog
      :open="confirmOpen"
      :title="confirmTitle"
      :body="confirmBody"
      :danger="confirmDanger"
      :danger-list="confirmHits.map((hit) => hit.label)"
      :confirm-label="confirmLabel"
      :cancel-label="t('cancel')"
      :busy="confirmBusy"
      @confirm="onConfirm"
      @cancel="closeConfirm"
    >
      <label v-if="confirmInput" style="display: flex; flex-direction: column; gap: 4px">
        <span v-if="confirmKind === 'newFolder'">{{ t("newFolderPlaceholder") }}</span>
        <span v-else-if="confirmKind === 'rename'">{{ t("renameTitle") }}</span>
        <span v-else>{{ t("pathPlaceholder") }}</span>
        <input v-model="confirmDraft" spellcheck="false" @keydown.enter.prevent="onConfirm" />
      </label>
    </ConfirmDialog>
  </main>
</template>
