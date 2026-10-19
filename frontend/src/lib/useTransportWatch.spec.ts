// @vitest-environment happy-dom
// 断网/断链自愈看守（对标 ssh useTerminalTransportWatch.spec）：钉住四个关键
// 语义——探测超时判断链、offline/online 事件即时驱动、恢复动作单飞且须同源
// 可达性确认、native（file:）部署完全不动。
import { defineComponent } from "vue";
import { mount } from "@vue/test-utils";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  TRANSPORT_HOST_REACHABILITY_TIMEOUT_MS,
  TRANSPORT_PROBE_INTERVAL_MS,
  TRANSPORT_PROBE_TIMEOUT_MS,
  isWebDeployment,
  useTransportWatch,
} from "./useTransportWatch";

type WatchOptions = Parameters<typeof useTransportWatch>[0];

function mountWatch(overrides: Partial<WatchOptions> = {}) {
  const options: WatchOptions = {
    isActive: () => true,
    isHealthy: () => true,
    probeTransport: vi.fn().mockResolvedValue(undefined),
    onOutage: vi.fn(),
    onRecover: vi.fn(),
    ...overrides,
  };
  const wrapper = mount(
    defineComponent({
      setup() {
        useTransportWatch(options);
        return () => null;
      },
    }),
  );
  return { wrapper, options };
}

describe("useTransportWatch", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("reports a single outage when the probe RPC never settles and keeps retrying reachability", async () => {
    vi.useFakeTimers();
    const probeTransport = vi.fn(() => new Promise<never>(() => {}));
    const canReachHost = vi.fn().mockResolvedValue(false);
    const onOutage = vi.fn();
    const onRecover = vi.fn();
    const { wrapper } = mountWatch({ probeTransport, canReachHost, onOutage, onRecover });

    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_INTERVAL_MS);
    expect(probeTransport).toHaveBeenCalledOnce();

    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_TIMEOUT_MS);
    expect(onOutage).toHaveBeenCalledOnce();
    expect(canReachHost).toHaveBeenCalledOnce();

    // 断链后探测让位给恢复：可达性未确认前不再打探测 RPC，也绝不恢复。
    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_INTERVAL_MS * 2);
    expect(probeTransport).toHaveBeenCalledOnce();
    expect(onRecover).not.toHaveBeenCalled();
    wrapper.unmount();
  });

  it("recovers exactly once after offline, and concurrent triggers do not overlap recovery", async () => {
    vi.useFakeTimers();
    let releaseReachability!: (value: boolean) => void;
    const canReachHost = vi.fn(
      () => new Promise<boolean>((resolve) => (releaseReachability = resolve)),
    );
    const onOutage = vi.fn();
    const onRecover = vi.fn();
    const { wrapper, options } = mountWatch({ canReachHost, onOutage, onRecover });

    window.dispatchEvent(new Event("offline"));
    await vi.advanceTimersByTimeAsync(1);
    expect(onOutage).toHaveBeenCalledOnce();
    expect(canReachHost).toHaveBeenCalledOnce();

    // 恢复在途时的并发触发（online 事件 + 探测节拍）不叠加第二次可达性探测。
    window.dispatchEvent(new Event("online"));
    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_INTERVAL_MS);
    expect(canReachHost).toHaveBeenCalledOnce();
    expect(onRecover).not.toHaveBeenCalled();

    releaseReachability(true);
    await vi.advanceTimersByTimeAsync(1);
    expect(onRecover).toHaveBeenCalledOnce();

    // 恢复后看守回到健康探测：下一个节拍重新打探测 RPC。
    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_INTERVAL_MS);
    expect(options.probeTransport).toHaveBeenCalledOnce();
    wrapper.unmount();
  });

  it("falls back to an abortable same-origin probe when reachability is not injected", async () => {
    vi.useFakeTimers();
    const fetchMock = vi.fn((_url: RequestInfo | URL, init?: RequestInit) => new Promise<Response>((_resolve, reject) => {
      init?.signal?.addEventListener("abort", () => reject(new DOMException("Aborted", "AbortError")));
    }));
    vi.stubGlobal("fetch", fetchMock);
    const probeTransport = vi.fn().mockRejectedValue(new Error("bridge gone"));
    const onRecover = vi.fn();
    const { wrapper } = mountWatch({ probeTransport, onRecover });

    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_INTERVAL_MS);
    await vi.advanceTimersByTimeAsync(TRANSPORT_HOST_REACHABILITY_TIMEOUT_MS);

    expect(fetchMock).toHaveBeenCalledWith(
      window.location.href,
      expect.objectContaining({ method: "HEAD", cache: "no-store", credentials: "same-origin" }),
    );
    expect(onRecover).not.toHaveBeenCalled();
    wrapper.unmount();
  });

  it("stays inert for native (file:) deployments", async () => {
    vi.useFakeTimers();
    (window as unknown as { happyDOM?: { setURL?: (url: string) => void } }).happyDOM?.setURL?.("file:///app/index.html");
    expect(isWebDeployment(window.location.protocol)).toBe(false);

    const probeTransport = vi.fn().mockResolvedValue(undefined);
    const onOutage = vi.fn();
    const onRecover = vi.fn();
    const { wrapper } = mountWatch({ probeTransport, onOutage, onRecover });

    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_INTERVAL_MS * 3);
    window.dispatchEvent(new Event("offline"));
    window.dispatchEvent(new Event("online"));
    await vi.advanceTimersByTimeAsync(TRANSPORT_PROBE_TIMEOUT_MS);

    expect(probeTransport).not.toHaveBeenCalled();
    expect(onOutage).not.toHaveBeenCalled();
    expect(onRecover).not.toHaveBeenCalled();
    wrapper.unmount();
  });
});
