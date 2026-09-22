// FTP 文件名显示解码（issue #32）。
//
// rclone ftp 后端默认携带 `Display` 文件名编码：服务器返回的非 UTF-8 字节
// （GBK 等老服务器）被逐字节转义成 `‛XX`（U+201B + 两位十六进制）保留在
// 文件名里，列表因此显示为乱码。这里把转义序列还原为原始字节后按连接
// 配置的显示字符集解码，仅用于界面显示；`path` 保持原始转义形式，
// 所有操作（打开/重命名/删除等）语义不变。

/** QuoteRune：rclone encoder 转义无效 UTF-8 字节时的前缀符号。 */
const QUOTE = "\u201B";

/** 连接表单可选的显示字符集（与 manifest.json display_charset 选项一致）。 */
export const DISPLAY_CHARSETS = ["gbk", "big5", "shift_jis", "windows-1252"] as const;

export type DisplayCharset = (typeof DISPLAY_CHARSETS)[number];

function decoder(charset: string): TextDecoder | undefined {
  // 每次解码新建实例：缓存实例会把不完整字节序列的内部状态泄漏到
  // 下一次调用（例如单个 GBK 首字节挂起后污染后续解码）。
  try {
    return new TextDecoder(charset);
  } catch {
    // 未知/不支持的字符集标签：调用方回退到原始名称。
    return undefined;
  }
}

/**
 * 把 rclone Display 编码转义的字节序列按 charset 解码成可读文本；
 * 没有转义序列（合法 UTF-8 名字）原样返回。
 */
export function decodeDisplayName(raw: string, charset: string): string {
  if (!charset || !raw.includes(QUOTE)) return raw;
  const codec = decoder(charset);
  if (!codec) return raw;

  let out = "";
  let bytes: number[] = [];
  let escaped = ""; // 当前字节段对应的原始转义文本（解码失败回退用）
  const flush = () => {
    if (!bytes.length) return;
    // TextDecoder 无法报告解码错误：出现 U+FFFD 视为字符集不匹配，
    // 回退原始转义文本（与后端 display.rs 的 had_errors 行为一致）。
    const text = codec.decode(new Uint8Array(bytes));
    out += text.includes("\uFFFD") ? escaped : text;
    bytes = [];
    escaped = "";
  };
  for (let index = 0; index < raw.length; ) {
    if (raw[index] !== QUOTE) {
      flush();
      out += raw[index];
      index += 1;
      continue;
    }
    const next = raw[index + 1];
    if (next === QUOTE) {
      // rclone 对字面 `‛` 的转义：`‛‛`。
      flush();
      out += QUOTE;
      index += 2;
      continue;
    }
    const hex = raw.slice(index + 1, index + 3);
    if (/^[0-9A-Fa-f]{2}$/.test(hex)) {
      escaped += raw.slice(index, index + 3);
      bytes.push(parseInt(hex, 16));
      index += 3;
      continue;
    }
    // 非转义形态的裸 `‛`：原样保留。
    flush();
    out += QUOTE;
    index += 1;
  }
  flush();
  return out;
}

/** 条目显示名：配置了显示字符集且名称含转义序列时才产生差异。 */
export function withDisplayName<T extends { name: string }>(entry: T, charset: string): T {
  if (!charset) return entry;
  const decoded = decodeDisplayName(entry.name, charset);
  return decoded === entry.name ? entry : { ...entry, displayName: decoded };
}
