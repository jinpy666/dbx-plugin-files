//! FTP 文件名显示编解码（issue #32）。
//!
//! rclone ftp 后端默认携带 `Display` 文件名编码：服务器返回的非 UTF-8 字节
//! （GBK 等老服务器）被逐字节转义成 `‛XX`（U+201B + 两位十六进制）保留在
//! 名字里，字面 `‛` 则转义为 `‛‛`。本模块在该转义形式与连接配置的显示
//! 字符集之间双向转换：
//!
//! - `decode_display`：打包下载（compress/archiveDownload）时把条目名解码
//!   成可读文本，zip/tar 里的名字不再是 `‛XX` 乱码；
//! - `encode_display`：解压上传（extract）时把归档内的可读名重新编码成
//!   `‛XX` 转义，rclone 解码后服务器收到的是目标字符集的原始字节。
//!
//! 两个方向都按段处理已转义序列（原样保留），因此对「已转义 + 明文」混排
//! 的输入（例如转义目录下的明文名）是安全且幂等的。charset 为空或未知时
//! 两个函数都是恒等映射。

use encoding_rs::Encoding;

/// rclone encoder 的 QuoteRune（`lib/encoder/encoder.go`）。
const QUOTE: char = '\u{201B}';

fn encoding_for(charset: &str) -> Option<&'static Encoding> {
    let charset = charset.trim();
    if charset.is_empty() {
        return None;
    }
    Encoding::for_label(charset.as_bytes())
}

fn hex_byte(value: u8) -> [char; 2] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    [HEX[(value >> 4) as usize] as char, HEX[(value & 0x0F) as usize] as char]
}

/// `‛XX` 转义字节序列 → 按字符集解码的可读文本；空/未知字符集时原样返回。
pub fn decode_display(raw: &str, charset: &str) -> String {
    let encoding = match encoding_for(charset) {
        Some(encoding) if raw.contains(QUOTE) => encoding,
        _ => return raw.to_string(),
    };
    let mut out = String::with_capacity(raw.len());
    let mut bytes: Vec<u8> = Vec::new();
    let mut escaped = String::new(); // 当前字节段对应的原始转义文本（解码失败回退用）
    let mut flush = |out: &mut String, bytes: &mut Vec<u8>, escaped: &mut String| {
        if bytes.is_empty() {
            return;
        }
        let (decoded, _, had_errors) = encoding.decode(bytes);
        if had_errors {
            out.push_str(escaped);
        } else {
            out.push_str(&decoded);
        }
        bytes.clear();
        escaped.clear();
    };
    let mut chars = raw.chars().peekable();
    while let Some(current) = chars.next() {
        if current != QUOTE {
            flush(&mut out, &mut bytes, &mut escaped);
            out.push(current);
            continue;
        }
        match chars.peek().copied() {
            // `‛‛`：字面 `‛` 的转义。
            Some(next) if next == QUOTE => {
                flush(&mut out, &mut bytes, &mut escaped);
                chars.next();
                out.push(QUOTE);
            }
            Some(next) if next.is_ascii_hexdigit() => {
                chars.next();
                match chars.next() {
                    Some(low) if low.is_ascii_hexdigit() => {
                        escaped.push(QUOTE);
                        escaped.push(next);
                        escaped.push(low);
                        let value = next.to_digit(16).unwrap_or(0) as u8;
                        bytes.push(value * 16 + low.to_digit(16).unwrap_or(0) as u8);
                    }
                    // 缺位的半个转义：结束当前字节段后按字面处理兜底
                    // （rclone 实际不会产出这种形态）。
                    other => {
                        flush(&mut out, &mut bytes, &mut escaped);
                        out.push(QUOTE);
                        out.push(next);
                        if let Some(other) = other {
                            out.push(other);
                        }
                    }
                }
            }
            _ => {
                flush(&mut out, &mut bytes, &mut escaped);
                out.push(QUOTE);
            }
        }
    }
    flush(&mut out, &mut bytes, &mut escaped);
    out
}

/// 可读文本（含已转义混排）→ `‛XX` 转义形式；空/未知字符集时原样返回。
pub fn encode_display(raw: &str, charset: &str) -> String {
    let Some(encoding) = encoding_for(charset) else {
        return raw.to_string();
    };
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(current) = chars.next() {
        if current == QUOTE {
            match chars.peek() {
                // 已转义序列 / 字面转义：原样保留（输入可能来自已有路径）。
                Some(next) if *next == QUOTE || next.is_ascii_hexdigit() => {
                    out.push(QUOTE);
                    let next = chars.next().expect("peeked");
                    out.push(next);
                    if next != QUOTE {
                        if let Some(low) = chars.next() {
                            out.push(low);
                        }
                    }
                }
                _ => out.push_str("‛‛"),
            }
            continue;
        }
        if current.is_ascii() {
            out.push(current);
            continue;
        }
        let mut char_buf = [0u8; 4];
        let (bytes, _, had_errors) = encoding.encode(current.encode_utf8(&mut char_buf));
        if had_errors {
            // 字符集无法表示（如 Shift-JIS 缺字）：保留原字符，rclone 端
            // 会按无效 UTF-8 之外的合法序列处理。
            out.push(current);
            continue;
        }
        for byte in bytes.iter().copied() {
            let [high, low] = hex_byte(byte);
            out.push(QUOTE);
            out.push(high);
            out.push(low);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_gbk_name_from_rclone_escape() {
        // "中文.txt" 的 GBK 字节：D6 D0 CE C4。
        let raw = "\u{201B}D6\u{201B}D0\u{201B}CE\u{201B}C4.txt";
        assert_eq!(decode_display(raw, "gbk"), "中文.txt");
    }

    #[test]
    fn decode_round_trips_through_encoding() {
        // 用 encode 生成转义（中文名 → GBK 字节转义），再 decode 还原。
        let encoded = encode_display("文档/报告.docx", "gbk");
        assert!(!encoded.contains('文'));
        assert_eq!(decode_display(&encoded, "gbk"), "文档/报告.docx");
    }

    #[test]
    fn decode_keeps_plain_and_literal_quote_segments() {
        let raw = "plain\u{201B}\u{201B}name";
        assert_eq!(decode_display(raw, "gbk"), "plain\u{201B}name");
        assert_eq!(decode_display("no-escape.bin", "gbk"), "no-escape.bin");
    }

    #[test]
    fn decode_unknown_charset_is_identity() {
        let raw = "\u{201B}D6\u{201B}D0.bin";
        assert_eq!(decode_display(raw, ""), raw);
        assert_eq!(decode_display(raw, "not-a-charset"), raw);
    }

    #[test]
    fn decode_error_falls_back_to_escaped_text() {
        // 0xFC 在 GBK 中不是合法首字节序列 → 回退原始转义。
        let raw = "\u{201B}FC.bin";
        assert_eq!(decode_display(raw, "gbk"), raw);
    }

    #[test]
    fn encode_preserves_existing_escapes_verbatim() {
        // 已转义路径 + 明文名混排：转义段不动，明文重编码。
        let input = "\u{201B}D6\u{201B}D0\u{201B}CE\u{201B}C4/新.txt";
        let output = encode_display(input, "gbk");
        assert!(output.starts_with("\u{201B}D6\u{201B}D0\u{201B}CE\u{201B}C4/"));
        assert_eq!(decode_display(&output, "gbk"), "中文/新.txt");
    }

    #[test]
    fn encode_ascii_and_unknown_charset_are_identity() {
        assert_eq!(encode_display("plain.bin", "gbk"), "plain.bin");
        let raw = "中文.bin";
        assert_eq!(encode_display(raw, ""), raw);
        assert_eq!(encode_display(raw, "not-a-charset"), raw);
    }

    #[test]
    fn encode_quotes_literal_quote() {
        assert_eq!(encode_display("a‛z", "gbk"), "a‛‛z");
        assert_eq!(decode_display("a‛‛z", "gbk"), "a‛z");
    }

    #[test]
    fn encode_unmappable_char_keeps_original() {
        // GBK 无法表示 emoji → 原样保留。
        let output = encode_display("see\u{1F600}.txt", "gbk");
        assert!(output.contains("see"));
        assert!(output.contains('\u{1F600}'));
    }
}
