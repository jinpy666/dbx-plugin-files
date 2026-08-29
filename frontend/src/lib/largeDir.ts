// 大目录提示阈值（UI parity 第 3 轮）。
//
// 为什么不把浏览链路切换到 files/listPaged（backend/src/engine/ops.rs
// list_paged 语义 + bench_list_10k_memory 实测）：
// - 后端 listPaged 每页都重新全量枚举后切片，单页成本 ≈ 一次全量 list
//   （10k 目录实测：全量 8.8ms，分页 7.3ms/页，5 页 ≈ 36ms 反而更贵）；
// - 前端排序/过滤/全选语义都建立在完整条目数组上（sorting.ts /
//   searchFilter.ts / FileTable select-all），分页驱动会破坏这些语义，
//   或被迫累计加载全部页（N 次 RPC × 全量枚举，成本更高）；
// - FileTable 已做行虚拟滚动（仅渲染可见行），渲染侧无 10k 行压力。
// 因此仅当目录条目数达到阈值时给用户一条提示，数据仍走全量 files/list。

/** 达到该条目数提示「大目录已加载」。与 mock 的 /10k 样例目录对齐。 */
export const LARGE_DIRECTORY_THRESHOLD = 2000;

/** 目录条目数是否达到「大目录」提示阈值。 */
export function isLargeDirectory(count: number, threshold = LARGE_DIRECTORY_THRESHOLD): boolean {
  return Number.isFinite(count) && count >= threshold;
}
