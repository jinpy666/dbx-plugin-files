// 目录导航竞态守卫（R3-P1-1）：慢响应晚到覆盖新导航——先点慢 /docs 再点快
// /10k，docs 响应晚到会把面包屑/列表/选中态整体覆盖回 /docs，最终停在用户
// 早已离开的目录。收口为按栏请求序号：发请求前取号，响应到达时验号，
// 过期序号的响应（含错误）一律丢弃。纯逻辑抽出便于单测。

export interface NavGuard {
  /** 发起新请求前调用：返回本次请求的序号令牌。 */
  next: () => number;
  /** 响应到达时调用：令牌仍是最新序号才允许落地。 */
  isCurrent: (token: number) => boolean;
}

export function createNavGuard(): NavGuard {
  let seq = 0;
  return {
    next: () => {
      seq += 1;
      return seq;
    },
    isCurrent: (token: number) => token === seq,
  };
}
