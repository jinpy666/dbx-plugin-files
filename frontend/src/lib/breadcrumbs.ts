// 面包屑路径解析（A-FILES ④a）：纯函数，供 Breadcrumbs.vue 使用。
// 解析："/" → 根；"/a/b" → [/ , a , b]，每级携带可导航的完整路径。

export interface Crumb {
  name: string;
  path: string;
}

/** 折叠后的渲染项：要么是一级路径，要么是中间省略号（点击展开全部）。 */
export type CrumbItem = Crumb | { collapsed: true };

export function parseCrumbs(path: string): Crumb[] {
  const crumbs: Crumb[] = [{ name: "/", path: "/" }];
  const parts = path.split("/").filter(Boolean);
  let acc = "";
  for (const part of parts) {
    acc += `/${part}`;
    crumbs.push({ name: part, path: acc });
  }
  return crumbs;
}

/**
 * 中间层级折叠：超过 maxVisible 时保留首级（根）与末尾 (maxVisible - 2) 级，
 * 中间以 {collapsed:true} 占位。maxVisible 至少为 3（根 + 省略 + 末级）。
 */
export function collapseCrumbs(crumbs: readonly Crumb[], maxVisible: number): CrumbItem[] {
  const limit = Math.max(3, Math.floor(maxVisible));
  if (crumbs.length <= limit) return [...crumbs];
  const head = crumbs.slice(0, 1);
  const tail = crumbs.slice(-(limit - 1));
  return [...head, { collapsed: true }, ...tail];
}
