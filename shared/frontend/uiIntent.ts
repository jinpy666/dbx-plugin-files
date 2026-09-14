/**
 * useUiIntent — MCP UI intent 通道公共 composable（三插件单点维护）。
 *
 * 设计来源 shared/IMPL_PLAN_PLUGIN_MCP.zh-CN.md（v2）§1：sidecar 的
 * `mcp/call` 在收到 `<x>_ui_search` / `<x>_ui_focus` / `<x>_ui_select` 时
 * 生成 intentId、发 `<domain>/ui/intent` 事件并等待 report（默认 5s）。
 * 前端订阅该事件 → 落 UI（条件填表、面板切换、选中行）→ 调
 * `<domain>/ui/state/report` {intentId, status:"applied"|"rejected",
 * summary} 回报；无 intentId 的 report 是快照型（关键动作后主动上报，
 * `<x>_ui_state` 不带 intentId 时返回最新快照）。
 *
 * 事件形状归一化沿用 binaryEvent 模式：宿主桥投递的 `{type:"event",
 * method, params}` 与裸 `{method, params}` 两种形状都收敛到统一结构，
 * 插件前端不各自判断。禁止在插件内各抄一份（AGENTS.md 硬性规则 7）。
 */

/** sidecar 发出的 `<domain>/ui/intent` 事件（归一化后）。 */
export interface UiIntentMessage {
  intentId: string;
  action: string;
  params: Record<string, unknown>;
}

/** intent 回报 / 快照的 summary（rows ≤5 行，单元格由调用方截断；
 * DN/path 等定位字段不截断）。各插件可附加域内字段（如 panel）。 */
export interface UiIntentSummary {
  count?: number;
  truncated?: boolean;
  rows?: Array<Record<string, unknown>>;
  /** 定位字段（如命中行 DN）。 */
  anchor?: string;
  /** rejected 时的人类可读原因。 */
  reason?: string;
  [key: string]: unknown;
}

/** handler 的处置结果：applied（可带 summary）或 rejected（带原因）。 */
export type UiIntentOutcome =
  | { status: "applied"; summary?: UiIntentSummary }
  | { status: "rejected"; reason?: string };

export type UiIntentHandler = (
  params: Record<string, unknown>,
) => Promise<UiIntentOutcome | void>;

export interface UiIntentHandlers {
  focus?: UiIntentHandler;
  search?: UiIntentHandler;
  select?: UiIntentHandler;
  /** 其余 action（如未来新增面板动作）按名字扩展。 */
  [action: string]: UiIntentHandler | undefined;
}

export interface UseUiIntentResult {
  /** 取消事件订阅（组件卸载时调用）。 */
  stop(): void;
  /** 快照型 report：无 intentId，sidecar 覆盖最新快照。 */
  reportSnapshot(summary: UiIntentSummary): void;
}

interface RawPluginEvent {
  type?: string;
  method?: string;
  params?: Record<string, unknown>;
}

/**
 * readUiIntentEvent 把宿主桥事件归一化为 UiIntentMessage；非本域 intent
 * 事件返回 null（纯函数，插件薄 spec 保底测试）。
 */
export function readUiIntentEvent(event: unknown, domain: string): UiIntentMessage | null {
  if (!event || typeof event !== "object") return null;
  const raw = event as RawPluginEvent;
  if (raw.type === "env") return null;
  if (raw.method !== `${domain}/ui/intent`) return null;
  const params = (raw.params ?? {}) as Record<string, unknown>;
  const intentId = typeof params.intentId === "string" ? params.intentId : "";
  const action = typeof params.action === "string" ? params.action : "";
  if (!intentId || !action) return null;
  const inner =
    params.params && typeof params.params === "object"
      ? (params.params as Record<string, unknown>)
      : {};
  return { intentId, action, params: inner };
}

/**
 * useUiIntent 订阅 `<domain>/ui/intent` 并把动作分派给 handlers；handler
 * 完成后自动调 `<domain>/ui/state/report` 回报。handler 缺失或抛错一律
 * rejected（sidecar 返回给 MCP 调用方，不假死）。
 */
export function useUiIntent(domain: string, handlers: UiIntentHandlers): UseUiIntentResult {
  const report = (body: Record<string, unknown>) => {
    const api = window.dbxPlugin;
    if (!api) return;
    void api
      .invoke(`${domain}/ui/state/report`, body)
      .catch(() => undefined); // 回报失败不打断 UI 流（sidecar 侧按 pending 超时收敛）
  };

  const handleEvent = (event: unknown) => {
    const message = readUiIntentEvent(event, domain);
    if (!message) return;
    const handler = handlers[message.action];
    if (!handler) {
      report({
        intentId: message.intentId,
        status: "rejected",
        summary: { reason: `no handler for action "${message.action}"` },
      });
      return;
    }
    void (async () => handler(message.params))()
      .then((outcome) => {
        if (outcome && outcome.status === "rejected") {
          report({ intentId: message.intentId, status: "rejected", summary: { reason: outcome.reason ?? "" } });
          return;
        }
        report({
          intentId: message.intentId,
          status: "applied",
          summary: (outcome && outcome.summary) ?? {},
        });
      })
      .catch((cause: unknown) => {
        report({
          intentId: message.intentId,
          status: "rejected",
          summary: { reason: cause instanceof Error ? cause.message : String(cause) },
        });
      });
  };

  const api = window.dbxPlugin;
  const unsubscribe = api?.onEvent ? api.onEvent(handleEvent) : () => undefined;

  return {
    stop: () => unsubscribe(),
    reportSnapshot: (summary) => report({ status: "snapshot", summary }),
  };
}
