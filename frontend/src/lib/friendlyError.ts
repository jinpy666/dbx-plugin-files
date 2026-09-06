// 友好错误映射层（P2-1，对标 ldap friendlyLdapError / ssh connectError 收口方案）：
// sidecar/宿主的原始错误串（"NotFound: /x"、"permission denied: …"）对用户不可
// 操作，`friendlyError` 把已知类别映射到 i18n 七语文案，未知错误保底原文透传
// （App 侧把原文挂在横幅 title 上供悬停查看）。另提供 `isTransportFailure`
// 供连接状态 pill 解耦（P2-3）：仅网络/传输层失败才算断连，业务错误不改 pill。

type Translate = (key: string, values?: Record<string, string | number>) => string;

const RULES: ReadonlyArray<{ pattern: RegExp; key: string }> = [
  // 不存在类：NotFound: /x、no such file、ENOENT
  { pattern: /not\s*found|no such (?:file|directory|entry|path|object)|enoent/i, key: "errNotFound" },
  // 权限类：epERM、eacces、forbidden、403
  { pattern: /permission|access denied|forbidden|unauthorized|\beperm\b|\beacces\b|\b403\b/i, key: "errPermission" },
  // 已存在类：重名/归档目标冲突
  { pattern: /already exists|file exists|\beexist\b/i, key: "errExists" },
  // 网络/传输类（先于通用 timeout 判断的兄弟规则合一即可，顺序无强依赖）
  {
    pattern: /connection refused|no such host|network error|connection reset|broken pipe|unreachable|i\/o timeout|timed? ?out|timeout|deadline exceeded|econn(aborted|refused|reset)/i,
    key: "errNetwork",
  },
];

/** 已知错误类别 → i18n key；未知原文透传（保底原文）。 */
export function friendlyError(message: string, t: Translate): string {
  const raw = String(message ?? "");
  for (const rule of RULES) {
    if (rule.pattern.test(raw)) return t(rule.key);
  }
  return raw;
}

/** 传输/网络层失败（连接 pill 置 disconnected 的唯一依据，P2-3）。 */
export function isTransportFailure(message: string): boolean {
  return /connection refused|no such host|network error|connection reset|broken pipe|unreachable|i\/o timeout|timed? ?out|timeout|deadline exceeded|econn(aborted|refused|reset)/i.test(
    String(message ?? ""),
  );
}

/** 「目标不存在」类业务错误（重名预检的 stat 判定依据，P2-10）。 */
export function isNotFoundMessage(message: string): boolean {
  return /not\s*found|no such (?:file|directory|entry|path|object)|enoent/i.test(String(message ?? ""));
}
