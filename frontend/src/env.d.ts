interface DbxPluginBinaryEvent {
  channel: string;
  /** 当前宿主桥投递零拷贝字节；旧桥（Host API 1.0）投递 base64 字符串。 */
  data?: Uint8Array;
  dataBase64?: string;
}

interface DbxPluginBackendEvent {
  type?: "event";
  method: string;
  params: Record<string, unknown>;
}

/** 当前 SDK 通过 onEvent 同时投递后端事件与宿主环境更新。 */
interface DbxPluginEnvironmentEvent {
  type: "env";
  locale?: string;
  theme?: DbxPluginTheme;
}

type DbxPluginEvent = DbxPluginBackendEvent | DbxPluginEnvironmentEvent;

interface DbxPluginFileTransferApi {
  pick(options?: { accept?: string; multiple?: boolean }): Promise<{ files: Array<{ handleId: string; name: string; size: number; contentType: string }> }>;
  /** Optional native directory picker; older hosts simply omit it. */
  pickDirectory?: () => Promise<string | { path: string } | undefined>;
  read(handleId: string, offset: number, length?: number): Promise<{ dataBase64: string; length: number; eof: boolean }>;
  beginSave(options: { name: string; contentType?: string; size?: number }): Promise<{ handleId: string; chunkBytes: number }>;
  write(handleId: string, offset: number, data: Uint8Array | ArrayBuffer | string): Promise<{ written: number; nextOffset: number }>;
  finish(handleId: string): Promise<void>;
  cancel(handleId: string): Promise<void>;
  onDragState(listener: (active: boolean) => void): () => void;
  onDrop(listener: (files: Array<{ handleId: string; name: string; size: number; contentType: string }>) => void): () => void;
}

interface DbxPluginTheme {
  appearance: "light" | "dark";
  /** 宿主根节点解析后的设计令牌（--color-* / --radius-* / --font-*），Host API 1.0 无此字段。 */
  tokens: Record<string, string>;
}

/**
 * `files/ui/intent` 事件载荷（M2 MCP UI intent 通道；sidecar `mcp/call`
 * 经 emitter 下发，shared/frontend/uiIntent 消费并回报
 * `files/ui/state/report`）。
 */
interface DbxPluginUiIntentEvent {
  intentId: string;
  action: string;
  params?: Record<string, unknown>;
}

/** `files/ui/state/report` 的请求体（intent 回报或快照型）。 */
interface DbxPluginUiStateReport {
  intentId?: string;
  status: "applied" | "rejected" | "snapshot";
  summary?: {
    count?: number;
    truncated?: boolean;
    rows?: Array<Record<string, unknown>>;
    anchor?: string;
    reason?: string;
    panel?: string;
    path?: string;
  };
}

interface DbxPluginAppearance {
  colorScheme: "light" | "dark";
  colors: {
    background: string;
    foreground: string;
    muted: string;
    mutedForeground: string;
    accent: string;
    accentForeground: string;
    border: string;
    destructive: string;
  };
  terminal: { fontFamily: string; fontSize: number };
  ui?: { fontFamily: string };
}

interface DbxPluginApi {
  ready: Promise<Record<string, unknown>>;
  readonly context?: Record<string, unknown>;
  readonly appearance?: DbxPluginAppearance;
  readonly theme?: DbxPluginTheme;
  readonly locale: string;
  /** 宿主能力位（Host API 1.2 起）；storage 缺失表示该宿主无持久化桥。 */
  readonly capabilities?: { storage?: boolean };
  /** 持久化 UI 状态桥（单值 256 KiB / 总量 1 MiB）；配 capabilities.storage 使用，经 shared/frontend/pluginStorage 读写。 */
  readonly storage?: {
    get(key: string): Promise<unknown>;
    set(key: string, value: unknown): Promise<unknown>;
    delete(key: string): Promise<unknown>;
  };
  request<T = unknown>(method: string, params?: unknown): Promise<T>;
  invoke<T = unknown>(method: string, params?: unknown, options?: { timeoutMs?: number }): Promise<T>;
  notify(method: string, params?: unknown): Promise<void>;
  sendBinary(channel: string, data: Uint8Array | ArrayBuffer | string): Promise<void>;
  onEvent(listener: (event: DbxPluginEvent) => void): () => void;
  onBinary(listener: (event: DbxPluginBinaryEvent) => void): () => void;
  onAppearanceChange?(listener: (appearance: DbxPluginAppearance) => void): () => void;
  onContext?(listener: (context: Record<string, unknown>) => void): () => void;
  onInit?(listener: (context: Record<string, unknown>) => void): () => void;
  decodeBase64(value: string): Uint8Array;
  encodeBase64(value: Uint8Array | ArrayBuffer): string;
  readonly fileTransfer?: DbxPluginFileTransferApi;
  readonly workbenchState?: { set(state: Record<string, unknown>): Promise<void> };
  readonly clipboard?: { readText(): Promise<string>; writeText(text: string): Promise<void> };
}

interface Window {
  dbxPlugin: DbxPluginApi;
}
