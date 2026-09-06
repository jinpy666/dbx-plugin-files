// 文件名校验（R3-P2-4）：此前新建/重命名零校验——名 `a/b` 提交成功但列表
// 不显示（按 `/` 切层后父段不存在，用户以为操作丢失）；名 `..` 成为可点击、
// 可删除的反模式条目。统一在提交前校验并给行内提示。
// 约定（newFolder/newFile/rename 三分支共用）：
//   - trim 后为空 → "empty"
//   - 含 `/`（或 Windows 语义的 `\`，防跨平台误用）→ "slash"
//   - 名为 `.` 或 `..` → "dot"

export type FileNameIssue = "empty" | "slash" | "dot";

export function validateFileName(raw: string): FileNameIssue | null {
  const name = raw.trim();
  if (!name) return "empty";
  if (name.includes("/") || name.includes("\\")) return "slash";
  if (name === "." || name === "..") return "dot";
  return null;
}
