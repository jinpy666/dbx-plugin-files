import { onBeforeUnmount } from "vue";

export const TRANSPORT_PROBE_INTERVAL_MS = 5_000;
export const TRANSPORT_PROBE_TIMEOUT_MS = 8_000;
export const TRANSPORT_HOST_REACHABILITY_TIMEOUT_MS = 2_000;

/** web 部署判定：DBX 把插件跑在 http(s) 浏览器上下文；native webview（file:）
 * 的传输生命周期由宿主推送，看守不启用。 */
export function isWebDeployment(protocol: string): boolean {
  return protocol === "http:" || protocol === "https:";
}

/**
 * 断网/断链自愈看守（对标 ssh useTerminalTransportWatch，files 无会话态故
 * 断链/恢复沿回调报给宿主组件）。浏览器 online/offline 事件给即时信号；
 * Docker/WS 链路里 navigator.onLine 恒真，用有界只读探测兜底（5s 节奏 /
 * 8s 单次超时）。探测超时必须在本页面判定：宿主无法对收不到的请求 enforce
 * 超时。恢复动作单飞（不并发）：先同源可达性探测确认宿主回来，才报
 * onRecover；桥缺失/探测被拒静默退化（isHealthy/isActive 由调用方把门），
 * 本体不外抛。卸载即摘除监听与定时器。
 */
export function useTransportWatch(options: {
  /** 存在可探测的活动存储连接（本地 __local__ 等无链路面板不探测）。 */
  isActive: () => boolean;
  /** 仅连接处于健康态（connState==="connected"）才探测/判中断：业务失败、
   * boot 自愈窗口各有自己的恢复链路，看守不抢跑。 */
  isHealthy: () => boolean;
  /** 只读探测 RPC（files/capabilities），options 注入避免看守耦合调用面。 */
  probeTransport: () => Promise<unknown>;
  /** 同源可达性探测；缺省时退化为 HEAD 本页 URL（见 probeHostReachability）。 */
  canReachHost?: () => Promise<boolean>;
  onOutage: () => void;
  onRecover: () => void;
}) {
  const browserDeployment = isWebDeployment(window.location.protocol);
  let probeTimer = 0;
  let probeInFlight = false;
  let recoveryInFlight = false;
  let outageActive = false;
  let stopped = false;

  async function probeHostReachability(): Promise<boolean> {
    if (options.canReachHost) {
      try {
        return await options.canReachHost();
      } catch {
        return false;
      }
    }

    // invoke 的超时由宿主在收到请求后才生效，卡在浏览器与 Docker 宿主之间
    // 的请求它管不着——恢复判定用可中断的同源请求（对标 ssh 同名探测）。
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), TRANSPORT_HOST_REACHABILITY_TIMEOUT_MS);
    try {
      await fetch(window.location.href, {
        method: "HEAD",
        cache: "no-store",
        credentials: "same-origin",
        signal: controller.signal,
      });
      return true;
    } catch {
      return false;
    } finally {
      window.clearTimeout(timer);
    }
  }

  function withTimeout<T>(operation: Promise<T>, timeoutMs: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      let settled = false;
      const timer = window.setTimeout(() => {
        if (settled) return;
        settled = true;
        reject(new Error(`transport probe timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      Promise.resolve(operation).then(
        (value) => {
          if (settled) return;
          settled = true;
          window.clearTimeout(timer);
          resolve(value);
        },
        (cause: unknown) => {
          if (settled) return;
          settled = true;
          window.clearTimeout(timer);
          reject(cause);
        },
      );
    });
  }

  /** 断链只报一次（outageActive 去重），随后立即试探一次恢复。 */
  function enterOutage() {
    if (stopped || !browserDeployment || outageActive) return;
    outageActive = true;
    options.onOutage();
    void attemptRecover();
  }

  /** 恢复单飞：可达性探测通过才摘除断链态并回调 onRecover；未可达则留在
   * 断链态等下一个探测节拍 / online 事件。 */
  async function attemptRecover() {
    if (stopped || !browserDeployment || recoveryInFlight || !outageActive) return;
    recoveryInFlight = true;
    try {
      if (await probeHostReachability()) {
        outageActive = false;
        options.onRecover();
      }
    } finally {
      recoveryInFlight = false;
    }
  }

  async function probe() {
    if (stopped || !browserDeployment || document.visibilityState === "hidden") return;
    if (outageActive) {
      await attemptRecover();
      return;
    }
    if (probeInFlight || !options.isActive() || !options.isHealthy()) return;
    probeInFlight = true;
    try {
      await withTimeout(Promise.resolve().then(() => options.probeTransport()), TRANSPORT_PROBE_TIMEOUT_MS);
    } catch {
      // 超时/拒绝都视为链路死亡：探测 RPC 不会无故失败（isHealthy 已把门，
      // 业务失败不走到这）。挂起恢复交给 attemptRecover 的可达性探测。
      enterOutage();
    } finally {
      probeInFlight = false;
    }
  }

  function onOffline() {
    if (!options.isActive() || !options.isHealthy()) return;
    enterOutage();
  }

  function onOnline() {
    void attemptRecover();
  }

  function onVisibilityChange() {
    if (document.visibilityState === "visible") void probe();
  }

  if (browserDeployment) {
    window.addEventListener("offline", onOffline);
    window.addEventListener("online", onOnline);
    document.addEventListener("visibilitychange", onVisibilityChange);
    probeTimer = window.setInterval(() => void probe(), TRANSPORT_PROBE_INTERVAL_MS);
  }

  onBeforeUnmount(() => {
    stopped = true;
    window.clearInterval(probeTimer);
    window.removeEventListener("offline", onOffline);
    window.removeEventListener("online", onOnline);
    document.removeEventListener("visibilitychange", onVisibilityChange);
  });
}
