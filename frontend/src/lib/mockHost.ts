// 开发/验证专用 mock 宿主桥（P-FILES ④）：URL 带 ?mock=1 且宿主桥缺失时启用。
// 用途：浏览器内验证 UI 三态（加载/空/错误）、大目录虚拟滚动、transport=job
// 的 copy/move/rename 终态刷新与传输面板——不进生产路径（动态 import 单独分包）。
// 行为开关（query 参数）：
//   ?mock=1          启用
//   &locale=zh-CN    宿主 locale（默认 zh-CN）
//   &theme=light     宿主 1.1 theme 通道方案（默认 dark，与真实宿主一致）
//   &delay=300       files/list 人为延迟 ms（便于观察加载态）
//   &streamBatch=500 files/listStream 每批条目数（流式分块演示，默认 500）
//   &streamInterval=0 files/listStream 批间隔 ms（默认 0：一拍内连续送达）
//   &streamFailAfter=N 第 N 批后注入失败帧（验证「已加载 N 项后失败」横幅）
//   &streamDisabled=1 files/listStream ack 直接报 disabled（验证无缝回落 files/list）
//   &streamEventsBeforeAck=1 第一批 chunk 在 ack 应答前同步 emit（复现真实宿主桥
//                     「事件先于 ack」写入顺序，验证前端预缓冲重放）
//   &job=1           copy/move 一律走降级 job（默认仅目录/`mockDir`）
//   &ro=1            只读态注入（connection.readOnly + capabilities.readOnly，
//                    P2-13①：供只读徽章/写按钮禁用/右键菜单禁用的 UI 走查）
//   &local=1         模拟桌面宿主（canSaveLocal=true + detect-apps 预设桩）
//   &platform=macos  local=1 下的平台标签（macos|windows|linux，默认 macos）
//   &connectionTest=fail  connection/test 的不可达夹具（不发真实网络请求）
// 任何包含 "error" 的路径都会返回业务错误（便于验证错误横幅与重试）。
// __local__ 连接（双栏左栏本地面）：list/listPaged/stat/quickPaths/read 路由到
// 独立本地树（$HOME 家族 quickPaths）；读写按连接落各自内存树。

import { GENERIC_PROTOCOL_IDS } from "./rcloneServices";

type MockEntry = { kind: "file" | "dir"; size: number; modifiedAt: string };

const CHUNK = 256 * 1024;

function b64encode(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

function b64decode(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function concatBytes(chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

// 最近一次安装实例的 emit 挂钩（installMockHost 内赋值；emitUiIntent 使用）。
let emitEventRef: ((method: string, payload: Record<string, unknown>) => void) | null = null;

// 模拟本机下载目录里已存在的文件（绝对路径）：files/local/exists 命中依据，
// 集成测试经 seedMockLocalDownload 预置（mock 不读真实 Downloads）。
const localDownloadFiles = new Set<string>();

/** 预置 mock 下载目录同名文件（files/local/exists 探测命中），测试专用。 */
export function seedMockLocalDownload(path: string): void {
  localDownloadFiles.add(path);
}

export function installMockHost() {
  if (window.dbxPlugin) return;
  const params = new URLSearchParams(window.location.search);
  const delayMs = Number(params.get("delay") ?? "250");
  // 流式列表（files/listStream P1）演示参数：批次大小/批间隔可调（对齐 &delay=
  // 的查询参数风格）；&streamFailAfter=N 在第 N 批后注入失败帧；&streamDisabled=1
  // 让 ack 直接报 disabled（走查前端无缝回落 files/list）。
  const streamBatch = Math.max(1, Number(params.get("streamBatch") ?? "500"));
  const streamInterval = Math.max(0, Number(params.get("streamInterval") ?? "0"));
  const streamFailAfter = params.has("streamFailAfter") ? Math.max(0, Number(params.get("streamFailAfter"))) : null;
  const streamDisabled = params.get("streamDisabled") === "1";
  const streamEventsBeforeAck = params.get("streamEventsBeforeAck") === "1";
  const forceJob = params.get("job") === "1";
  // P2-13①：?ro=1 只读态注入（与 canWrite = !connection.readOnly && !capabilities.readOnly 双闸对齐）。
  const readOnly = params.get("ro") === "1";
  // 对标 rclone-dashboard share 链接：?presign=1 让 mock capabilities.presign=true，
  // files/publicLink 返回伪签名 URL（默认 false，模拟不支持公开链接的后端）。
  const presign = params.get("presign") === "1";
  // ?local=1 模拟桌面宿主：canSaveLocal=true + ?platform= 指定 OS（默认 macos），
  // 供设置弹窗「下载/打开方式」分类与平台预设 chips 走查（默认仍模拟 web 宿主）。
  const demoLocal = params.get("local") === "1";
  const demoPlatform = params.get("platform") ?? "macos";

  // ---- 虚拟文件树 ---------------------------------------------------------
  const tree = new Map<string, MockEntry>();
  // A-FILES ②：文件内容（files/read / files/write 用）
  const contents = new Map<string, Uint8Array>();
  const now = Date.now();
  const stamp = (offsetDays: number) => new Date(now - offsetDays * 86_400_000).toISOString();
  const put = (path: string, kind: "file" | "dir", size = 0) =>
    tree.set(path.replace(/\/+$/, "") || "/", { kind, size, modifiedAt: stamp(tree.size % 30) });
  const putText = (path: string, text: string) => {
    const bytes = new TextEncoder().encode(text);
    contents.set(path.replace(/\/+$/, ""), bytes);
    put(path, "file", bytes.byteLength);
  };
  const putBase64 = (path: string, value: string) => {
    const bytes = b64decode(value);
    contents.set(path.replace(/\/+$/, ""), bytes);
    put(path, "file", bytes.byteLength);
  };

  put("/", "dir");
  put("/docs", "dir");
  put("/empty", "dir");
  put("/10k", "dir");
  // 快速目录样例（files/quickPaths mock 段按存在才透出，与真实 fs 侧行为对齐）
  put("/desktop", "dir");
  put("/downloads", "dir");
  put("/documents", "dir");
  put("/pictures", "dir");
  putText("/docs/readme.md", "# Files Studio\n\n双栏文件浏览（A-FILES）验证样例。\n\n- 左栏：源（当前连接）\n- 右栏：目标面板 / 预览\n\n编辑此文件并保存会走 files/write。\n");
  putText("/docs/notes.txt", "line 1\nline 2\nline 3\n");
  putText("/docs/data.csv", "name,value\nalpha,1\nbeta,2\n");
  putText("/docs/data.json", '{"format":"json","ok":true,"items":[1,2,3]}\n');
  putText("/docs/page.html", "<!doctype html><html><body><h1>DBX Files HTML</h1><p>HTML fixture</p></body></html>\n");
  putText("/docs/vector.svg", '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60"><rect width="120" height="60" fill="#2f6feb"/><text x="10" y="36" fill="white">DBX Files</text></svg>');
  // 多编程语言样例（/code）：验证各语言在 CodeMirror 下的识别与编辑。
  put("/code", "dir");
  putText("/code/main.go", "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"DBX Files Go\")\n}\n");
  putText("/code/server.php", "<?php\ndeclare(strict_types=1);\n\nfunction greet(string $name): string {\n    return \"Hello, {$name}\";\n}\n\necho greet('DBX Files');\n");
  putText("/code/Program.cs", "using System;\n\nnamespace DbxFiles;\n\npublic static class Program {\n    public static void Main() {\n        Console.WriteLine(\"DBX Files C#\");\n    }\n}\n");
  putText("/code/Main.kt", "fun main() {\n    val files = listOf(\"dbx\", \"files\")\n    println(\"DBX Files Kotlin: ${files.size}\")\n}\n");
  putText("/code/App.swift", "import Foundation\n\nlet name = \"DBX Files\"\nprint(\"\\(name) Swift\")\n");
  putText("/code/main.dart", "void main() {\n  final files = ['dbx', 'files'];\n  print('DBX Files Dart: ${files.length}');\n}\n");
  putText("/code/hello.scala", "object Hello extends App {\n  println(\"DBX Files Scala\")\n}\n");
  putText("/code/fib.hs", "fib :: Int -> Int\nfib 0 = 0\nfib 1 = 1\nfib n = fib (n - 1) + fib (n - 2)\n\nmain :: IO ()\nmain = print (fib 10)\n");
  putText("/code/script.lua", "local function greet(name)\n  print(\"DBX Files \" .. name)\nend\n\ngreet(\"Lua\")\n");
  putText("/code/tool.pl", "use strict;\nuse warnings;\n\nprint \"DBX Files Perl\\n\";\n");
  putText("/code/deploy.ps1", "$name = 'DBX Files'\nWrite-Host \"Deploying $name (PowerShell)\"\n");
  putText("/code/style.scss", "$accent: #2f6feb;\n\n.button {\n  border-color: $accent;\n  &:hover { opacity: .8; }\n}\n");
  putText("/code/Job.groovy", "println 'DBX Files Groovy'\n");
  putText("/code/schema.proto", "syntax = \"proto3\";\n\nmessage FileEntry {\n  string path = 1;\n  uint64 size = 2;\n}\n");
  putText("/code/render.m", "#import <Foundation/Foundation.h>\n\n// DBX Files Objective-C sample\nNSLog(@\"hello\");\n");
  putText("/code/migrate.sql", "-- DBX Files SQL sample\nCREATE TABLE files (\n  id INTEGER PRIMARY KEY,\n  path TEXT NOT NULL UNIQUE\n);\n");
  putText("/code/site.conf", "# nginx-style conf sample\nserver {\n  listen 8080;\n  root /srv/dbx-files;\n}\n");
  {
    // 1x1 PNG（图片预览 / data URI）
    const png = window.atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    const bytes = new Uint8Array(png.length);
    for (let i = 0; i < png.length; i += 1) bytes[i] = png.charCodeAt(i);
    contents.set("/docs/logo.png", bytes);
    put("/docs/logo.png", "file", bytes.byteLength);
  }
  {
    // 二进制样例（hex dump 预览）
    const bytes = new Uint8Array(1024);
    for (let i = 0; i < bytes.length; i += 1) bytes[i] = (i * 31 + 7) % 256;
    contents.set("/docs/data.bin", bytes);
    put("/docs/data.bin", "file", bytes.byteLength);
  }
  // 最小有效 PDF，供 PDF renderer 验证真实解析链路。
  putBase64("/docs/report.pdf", "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCAzMDAgMTQ0XSAvQ29udGVudHMgNCAwIFIgL1Jlc291cmNlcyA8PCAvRm9udCA8PCAvRjEgNSAwIFIgPj4gPj4gPj4KZW5kb2JqCjQgMCBvYmoKPDwgL0xlbmd0aCA0NCA+PgpzdHJlYW0KQlQgL0YxIDE4IFRmIDM2IDkwIFRkIChEQlggRmlsZXMgUERGKSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCjUgMCBvYmoKPDwgL1R5cGUgL0ZvbnQgL1N1YnR5cGUgL1R5cGUxIC9CYXNlRm9udCAvSGVsdmV0aWNhID4+CmVuZG9iagp4cmVmCjAgNgowMDAwMDAwMDAwIDY1NTM1IGYgCjAwMDAwMDAwMDkgMDAwMDAgbiAKMDAwMDAwMDA1OCAwMDAwMCBuIAowMDAwMDAwMTE1IDAwMDAwIG4gCjAwMDAwMDAyNDEgMDAwMDAgbiAKMDAwMDAwMDMzNCAwMDAwMCBuIAp0cmFpbGVyCjw8IC9TaXplIDYgL1Jvb3QgMSAwIFIgPj4Kc3RhcnR4cmVmCjQwNAolJUVPRgo=");
  // 真实 ZIP/TAR.GZ 样例，解压内容可由 archive renderer 读取。
  putBase64("/backup.zip", "UEsDBBQAAAAIAGN8MV3X+nOnEQAAAA8AAAAJAAAAaGVsbG8udHh080jNyclXSCvKz1WI8gzgAgBQSwMEFAAAAAgAY3wxXT49b8QTAAAAEQAAAAkAAABkYXRhLmpzb26rVkrLL8pNLFGyUqrKLFCq5QIAUEsBAhQDFAAAAAgAY3wxXdf6c6cRAAAADwAAAAkAAAAAAAAAAAAAAIABAAAAAGhlbGxvLnR4dFBLAQIUAxQAAAAIAGN8MV0+PW/EEwAAABEAAAAJAAAAAAAAAAAAAACAATgAAABkYXRhLmpzb25QSwUGAAAAAAIAAgBuAAAAcgAAAAAA");
  putBase64("/docs/site.tar.gz", "H4sIACuYq2oC/+3UsQ6CMBDG8c48BQ9AmkpJnZ10Nk5uF61hKGKgEB/fymLCLsbw/y13ueWGy3e1D6HV8RnV95jEVdVUk3k1piw//TTfOmNVbtQChj5Kl1aqdTq875/furbJT7uj3p8zhRW5ShR96Uf10/zbef6tc+R/CXdpfDFKGHwm4VFLseEBAAAAAAAAAAAAAAAA/JMXZeO+DQAoAAA=");
  putBase64("/docs/sample.docx", "UEsDBBQAAAAIAGN8MV3mdcR+0gAAAIsBAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbH2QvVLDMAzHX8XnlasVGBh6SToAKzD0BXSOkvjw11luad++Sls6cIVR+n/8ZLebQ/BqT4Vdip1+NI3e9O32mImVKJE7Pdea1wBsZwrIJmWKooypBKwylgky2i+cCJ6a5hlsipViXdWlQ/ftK42481W9HWR9oRTyrNXLxbiwOo05e2exig77OPyirK4EI8mzh2eX+UEMGu4SFuVvwDX3Ic8ubiD1iaW+YxAXfKcywJDsLkjS/F9z5840js7SLb+05ZIsMbs4BW9uSkAXf+6H83f3J1BLAwQUAAAACABjfDFdXzOVUpUAAAAHAQAACwAAAF9yZWxzLy5yZWxzjc87DsIwDAbgq0Q+QJ0yMKCmXVi6Ii4QJW5T0TzkhNftycBAEQOjf//6LHfDw6/iRpyXGBS0jYSh70606lKD7JaURW2ErMCVkg6I2TjyOjcxUaibKbLXpY48Y9LmomfCnZR75E8DtqYYrQIebQvi/Ez0jx2naTF0jObqKZQfJ74aVdY8U1Fwj2zRvuOmsoB9h5sX+xdQSwMEFAAAAAgAY3wxXfUQxEqMAAAAtQAAABEAAAB3b3JkL2RvY3VtZW50LnhtbEXOwQ6CMAwG4FdZ9gAUPXhYYCRKvHrlimzCkm1d2in69jI8ePma9k+bNt07ePGyxA5jKw9VLTvdrMrg9Aw2ZrHFkdXayiXnpAB4WmwYucJk45Y9kMKYt5ZmWJFMIpwss4tz8HCs6xOE0UVZTt7RfEpNBSpk3Z8HcXXesuhvl6GBMivSbtr97cH/J/0FUEsBAhQDFAAAAAgAY3wxXeZ1xH7SAAAAiwEAABMAAAAAAAAAAAAAAIABAAAAAFtDb250ZW50X1R5cGVzXS54bWxQSwECFAMUAAAACABjfDFdXzOVUpUAAAAHAQAACwAAAAAAAAAAAAAAgAEDAQAAX3JlbHMvLnJlbHNQSwECFAMUAAAACABjfDFd9RDESowAAAC1AAAAEQAAAAAAAAAAAAAAgAHBAQAAd29yZC9kb2N1bWVudC54bWxQSwUGAAAAAAMAAwC5AAAAfAIAAAAA");
  // XLSX 用 STORED（无压缩）ZIP 打包：viewer 内置 ZIP 解析器对部分 Deflate
  // 头部解析失败（Unsupported ZIP Compression method NaN）。
  putBase64("/docs/sample.xlsx", "UEsDBBQAAAAAAPqDMV1bma6uCwIAAAsCAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbDw/eG1sIHZlcnNpb249IjEuMCI/PjxUeXBlcyB4bWxucz0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3BhY2thZ2UvMjAwNi9jb250ZW50LXR5cGVzIj48RGVmYXVsdCBFeHRlbnNpb249InJlbHMiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtcGFja2FnZS5yZWxhdGlvbnNoaXBzK3htbCIvPjxEZWZhdWx0IEV4dGVuc2lvbj0ieG1sIiBDb250ZW50VHlwZT0iYXBwbGljYXRpb24veG1sIi8+PE92ZXJyaWRlIFBhcnROYW1lPSIveGwvd29ya2Jvb2sueG1sIiBDb250ZW50VHlwZT0iYXBwbGljYXRpb24vdm5kLm9wZW54bWxmb3JtYXRzLW9mZmljZWRvY3VtZW50LnNwcmVhZHNoZWV0bWwuc2hlZXQubWFpbit4bWwiLz48T3ZlcnJpZGUgUGFydE5hbWU9Ii94bC93b3Jrc2hlZXRzL3NoZWV0MS54bWwiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtb2ZmaWNlZG9jdW1lbnQuc3ByZWFkc2hlZXRtbC53b3Jrc2hlZXQreG1sIi8+PC9UeXBlcz5QSwMEFAAAAAAA+oMxXUuDozoFAQAABQEAAAsAAABfcmVscy8ucmVsczw/eG1sIHZlcnNpb249IjEuMCI/PjxSZWxhdGlvbnNoaXBzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L3JlbGF0aW9uc2hpcHMiPjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvb2ZmaWNlRG9jdW1lbnQiIFRhcmdldD0ieGwvd29ya2Jvb2sueG1sIi8+PC9SZWxhdGlvbnNoaXBzPlBLAwQUAAAAAAD6gzFdF1scxvkAAAD5AAAADwAAAHhsL3dvcmtib29rLnhtbDw/eG1sIHZlcnNpb249IjEuMCI/Pjx3b3JrYm9vayB4bWxucz0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3NwcmVhZHNoZWV0bWwvMjAwNi9tYWluIiB4bWxuczpyPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvb2ZmaWNlRG9jdW1lbnQvMjAwNi9yZWxhdGlvbnNoaXBzIj48c2hlZXRzPjxzaGVldCBuYW1lPSJTaGVldDEiIHNoZWV0SWQ9IjEiIHI6aWQ9InJJZDEiLz48L3NoZWV0cz48L3dvcmtib29rPlBLAwQUAAAAAAD6gzFdbTbpdAYBAAAGAQAAGgAAAHhsL19yZWxzL3dvcmtib29rLnhtbC5yZWxzPD94bWwgdmVyc2lvbj0iMS4wIj8+PFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+PFJlbGF0aW9uc2hpcCBJZD0icklkMSIgVHlwZT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcy93b3Jrc2hlZXQiIFRhcmdldD0id29ya3NoZWV0cy9zaGVldDEueG1sIi8+PC9SZWxhdGlvbnNoaXBzPlBLAwQUAAAAAAD6gzFdnaCvky4BAAAuAQAAGAAAAHhsL3dvcmtzaGVldHMvc2hlZXQxLnhtbDw/eG1sIHZlcnNpb249IjEuMCI/Pjx3b3Jrc2hlZXQgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9zcHJlYWRzaGVldG1sLzIwMDYvbWFpbiI+PHNoZWV0RGF0YT48cm93IHI9IjEiPjxjIHI9IkExIiB0PSJpbmxpbmVTdHIiPjxpcz48dD5EQlggRmlsZXMgWExTWDwvdD48L2lzPjwvYz48YyByPSJCMSI+PHY+NDI8L3Y+PC9jPjwvcm93Pjxyb3cgcj0iMiI+PGMgcj0iQTIiIHQ9ImlubGluZVN0ciI+PGlzPjx0PlN0b3JlZCBaSVA8L3Q+PC9pcz48L2M+PC9yb3c+PC9zaGVldERhdGE+PC93b3Jrc2hlZXQ+UEsBAhQDFAAAAAAA+oMxXVuZrq4LAgAACwIAABMAAAAAAAAAAAAAAIABAAAAAFtDb250ZW50X1R5cGVzXS54bWxQSwECFAMUAAAAAAD6gzFdS4OjOgUBAAAFAQAACwAAAAAAAAAAAAAAgAE8AgAAX3JlbHMvLnJlbHNQSwECFAMUAAAAAAD6gzFdF1scxvkAAAD5AAAADwAAAAAAAAAAAAAAgAFqAwAAeGwvd29ya2Jvb2sueG1sUEsBAhQDFAAAAAAA+oMxXW026XQGAQAABgEAABoAAAAAAAAAAAAAAIABkAQAAHhsL19yZWxzL3dvcmtib29rLnhtbC5yZWxzUEsBAhQDFAAAAAAA+oMxXZ2gr5MuAQAALgEAABgAAAAAAAAAAAAAAIABzgUAAHhsL3dvcmtzaGVldHMvc2hlZXQxLnhtbFBLBQYAAAAABQAFAEUBAAAyBwAAAAA=");
  // RIFF/WAV header with a short silent PCM sample, enough for media renderer detection.
  putBase64("/docs/silence.wav", "UklGRiQAAABXQVZFZm10IBAAAAABAAEARKwAAESsAAABAAgAZGF0YQAAAAA=");
  // 四类按需 renderer（presentation/ofd/mindmap/midi，见 filesStudioRenderers.ts）
  // 的最小合法样例：集中放 /previews，一次导航即可逐个打开走查。均为 STORED
  // （无压缩）ZIP 打包，规避 viewer 内置 ZIP 解析器对部分 Deflate 头的兼容问题。
  put("/previews", "dir");
  // pptx：标准 OOXML 包（[Content_Types].xml + ppt/presentation.xml sldIdLst/rId
  // + slides/slide1.xml 标题文本），renderer-presentation/pptx 走 worker 解析。
  putBase64("/previews/roadmap.pptx", "UEsDBBQAAAAAAAAAIQDCwACFZAMAAGQDAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbDw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFR5cGVzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L2NvbnRlbnQtdHlwZXMiPgo8RGVmYXVsdCBFeHRlbnNpb249InJlbHMiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtcGFja2FnZS5yZWxhdGlvbnNoaXBzK3htbCIvPgo8RGVmYXVsdCBFeHRlbnNpb249InhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3htbCIvPgo8T3ZlcnJpZGUgUGFydE5hbWU9Ii9wcHQvcHJlc2VudGF0aW9uLnhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3ZuZC5vcGVueG1sZm9ybWF0cy1vZmZpY2Vkb2N1bWVudC5wcmVzZW50YXRpb25tbC5wcmVzZW50YXRpb24ubWFpbit4bWwiLz4KPE92ZXJyaWRlIFBhcnROYW1lPSIvcHB0L3NsaWRlcy9zbGlkZTEueG1sIiBDb250ZW50VHlwZT0iYXBwbGljYXRpb24vdm5kLm9wZW54bWxmb3JtYXRzLW9mZmljZWRvY3VtZW50LnByZXNlbnRhdGlvbm1sLnNsaWRlK3htbCIvPgo8T3ZlcnJpZGUgUGFydE5hbWU9Ii9wcHQvc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWwiIENvbnRlbnRUeXBlPSJhcHBsaWNhdGlvbi92bmQub3BlbnhtbGZvcm1hdHMtb2ZmaWNlZG9jdW1lbnQucHJlc2VudGF0aW9ubWwuc2xpZGVMYXlvdXQreG1sIi8+CjxPdmVycmlkZSBQYXJ0TmFtZT0iL3BwdC9zbGlkZU1hc3RlcnMvc2xpZGVNYXN0ZXIxLnhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3ZuZC5vcGVueG1sZm9ybWF0cy1vZmZpY2Vkb2N1bWVudC5wcmVzZW50YXRpb25tbC5zbGlkZU1hc3Rlcit4bWwiLz4KPC9UeXBlcz5QSwMEFAAAAAAAAAAhAFfNaN8vAQAALwEAAAsAAABfcmVscy8ucmVsczw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvb2ZmaWNlRG9jdW1lbnQiIFRhcmdldD0icHB0L3ByZXNlbnRhdGlvbi54bWwiLz4KPC9SZWxhdGlvbnNoaXBzPlBLAwQUAAAAAAAAACEAyKGh+YwBAACMAQAAFAAAAHBwdC9wcmVzZW50YXRpb24ueG1sPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8cDpwcmVzZW50YXRpb24geG1sbnM6YT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL2RyYXdpbmdtbC8yMDA2L21haW4iIHhtbG5zOnI9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMiIHhtbG5zOnA9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wcmVzZW50YXRpb25tbC8yMDA2L21haW4iPgo8cDpzbGRJZExzdD48cDpzbGRJZCBpZD0iMjU2IiByOmlkPSJySWQxIi8+PC9wOnNsZElkTHN0Pgo8cDpzbGRTeiBjeD0iMTIxOTIwMDAiIGN5PSI2ODU4MDAwIi8+CjwvcDpwcmVzZW50YXRpb24+UEsDBBQAAAAAAAAAIQAY8ExVuwEAALsBAAAfAAAAcHB0L19yZWxzL3ByZXNlbnRhdGlvbi54bWwucmVsczw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvc2xpZGUiIFRhcmdldD0ic2xpZGVzL3NsaWRlMS54bWwiLz4KPFJlbGF0aW9uc2hpcCBJZD0icklkMiIgVHlwZT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcy9zbGlkZU1hc3RlciIgVGFyZ2V0PSJzbGlkZU1hc3RlcnMvc2xpZGVNYXN0ZXIxLnhtbCIvPgo8L1JlbGF0aW9uc2hpcHM+UEsDBBQAAAAAAAAAIQCwBN371QMAANUDAAAVAAAAcHB0L3NsaWRlcy9zbGlkZTEueG1sPD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiIHN0YW5kYWxvbmU9InllcyI/Pgo8cDpzbGQgeG1sbnM6YT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL2RyYXdpbmdtbC8yMDA2L21haW4iIHhtbG5zOnI9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMiIHhtbG5zOnA9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wcmVzZW50YXRpb25tbC8yMDA2L21haW4iPgo8cDpjU2xkPjxwOnNwVHJlZT48cDpudkdycFNwUHI+PHA6Y052UHIgaWQ9IjEiIG5hbWU9IiIvPjxwOmNOdkdycFNwUHIvPjxwOm52UHIvPjwvcDpudkdycFNwUHI+PHA6Z3JwU3BQci8+CjxwOnNwPgo8cDpudlNwUHI+PHA6Y052UHIgaWQ9IjIiIG5hbWU9IlRpdGxlIDEiLz48cDpjTnZTcFByPjxhOnNwTG9ja3Mgbm9HcnA9IjEiLz48L3A6Y052U3BQcj48cDpudlByPjxwOnBoIHR5cGU9InRpdGxlIi8+PC9wOm52UHI+PC9wOm52U3BQcj4KPHA6c3BQcj48YTp4ZnJtPjxhOm9mZiB4PSI4MzgyMDAiIHk9IjM2NTEyNSIvPjxhOmV4dCBjeD0iMTA1MTU2MDAiIGN5PSIxMzI1NTYzIi8+PC9hOnhmcm0+PGE6cHJzdEdlb20gcHJzdD0icmVjdCI+PGE6YXZMc3QvPjwvYTpwcnN0R2VvbT48L3A6c3BQcj4KPHA6dHhCb2R5PjxhOmJvZHlQci8+PGE6bHN0U3R5bGUvPjxhOnA+PGE6cj48YTpyUHIgbGFuZz0iemgtQ04iIHN6PSI0NDAwIiBiPSIxIi8+PGE6dD5EQlggRmlsZXMgUFBUWDwvYTp0PjwvYTpyPjwvYTpwPjxhOnA+PGE6cj48YTpyUHIgbGFuZz0iemgtQ04iIHN6PSIyNDAwIi8+PGE6dD7mnIDlsI8gT09YTUwg5ryU56S65paH56i/5qC35L6LPC9hOnQ+PC9hOnI+PC9hOnA+PC9wOnR4Qm9keT4KPC9wOnNwPgo8L3A6c3BUcmVlPjwvcDpjU2xkPjxwOmNsck1hcE92cj48YTptYXN0ZXJDbHJNYXBwaW5nLz48L3A6Y2xyTWFwT3ZyPjwvcDpzbGQ+UEsDBBQAAAAAAAAAIQDQczgoOAEAADgBAAAgAAAAcHB0L3NsaWRlcy9fcmVscy9zbGlkZTEueG1sLnJlbHM8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxSZWxhdGlvbnNoaXBzIHhtbG5zPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcGFja2FnZS8yMDA2L3JlbGF0aW9uc2hpcHMiPgo8UmVsYXRpb25zaGlwIElkPSJySWQxIiBUeXBlPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvb2ZmaWNlRG9jdW1lbnQvMjAwNi9yZWxhdGlvbnNoaXBzL3NsaWRlTGF5b3V0IiBUYXJnZXQ9Ii4uL3NsaWRlTGF5b3V0cy9zbGlkZUxheW91dDEueG1sIi8+CjwvUmVsYXRpb25zaGlwcz5QSwMEFAAAAAAAAAAhANbpkoDvAQAA7wEAACEAAABwcHQvc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWw8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCIgc3RhbmRhbG9uZT0ieWVzIj8+CjxwOnNsZExheW91dCB4bWxuczphPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvZHJhd2luZ21sLzIwMDYvbWFpbiIgeG1sbnM6cj0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcyIgeG1sbnM6cD0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3ByZXNlbnRhdGlvbm1sLzIwMDYvbWFpbiIgdHlwZT0iYmxhbmsiPgo8cDpjU2xkIG5hbWU9IkJsYW5rIj48cDpzcFRyZWU+PHA6bnZHcnBTcFByPjxwOmNOdlByIGlkPSIxIiBuYW1lPSIiLz48cDpjTnZHcnBTcFByLz48cDpudlByLz48L3A6bnZHcnBTcFByPjxwOmdycFNwUHIvPjwvcDpzcFRyZWU+PC9wOmNTbGQ+PHA6Y2xyTWFwT3ZyPjxhOm1hc3RlckNsck1hcHBpbmcvPjwvcDpjbHJNYXBPdnI+PC9wOnNsZExheW91dD5QSwMEFAAAAAAAAAAhAOTUfwo4AQAAOAEAACwAAABwcHQvc2xpZGVMYXlvdXRzL19yZWxzL3NsaWRlTGF5b3V0MS54bWwucmVsczw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvc2xpZGVNYXN0ZXIiIFRhcmdldD0iLi4vc2xpZGVNYXN0ZXJzL3NsaWRlTWFzdGVyMS54bWwiLz4KPC9SZWxhdGlvbnNoaXBzPlBLAwQUAAAAAAAAACEA6aWk8rkCAAC5AgAAIQAAAHBwdC9zbGlkZU1hc3RlcnMvc2xpZGVNYXN0ZXIxLnhtbDw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPHA6c2xkTWFzdGVyIHhtbG5zOmE9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9kcmF3aW5nbWwvMjAwNi9tYWluIiB4bWxuczpyPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvb2ZmaWNlRG9jdW1lbnQvMjAwNi9yZWxhdGlvbnNoaXBzIiB4bWxuczpwPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcHJlc2VudGF0aW9ubWwvMjAwNi9tYWluIj4KPHA6Y1NsZD48cDpzcFRyZWU+PHA6bnZHcnBTcFByPjxwOmNOdlByIGlkPSIxIiBuYW1lPSIiLz48cDpjTnZHcnBTcFByLz48cDpudlByLz48L3A6bnZHcnBTcFByPjxwOmdycFNwUHIvPjwvcDpzcFRyZWU+PC9wOmNTbGQ+CjxwOmNsck1hcCBiZzE9Imx0MSIgdHgxPSJkazEiIGJnMj0ibHQyIiB0eDI9ImRrMiIgYWNjZW50MT0iYWNjZW50MSIgYWNjZW50Mj0iYWNjZW50MiIgYWNjZW50Mz0iYWNjZW50MyIgYWNjZW50ND0iYWNjZW50NCIgYWNjZW50NT0iYWNjZW50NSIgYWNjZW50Nj0iYWNjZW50NiIgaGxpbms9ImhsaW5rIiBmb2xIbGluaz0iZm9sSGxpbmsiLz4KPHA6c2xkTGF5b3V0SWRMc3Q+PHA6c2xkTGF5b3V0SWQgaWQ9IjIxNDc0ODM2NDkiIHI6aWQ9InJJZDEiLz48L3A6c2xkTGF5b3V0SWRMc3Q+PC9wOnNsZE1hc3Rlcj5QSwMEFAAAAAAAAAAhANBzOCg4AQAAOAEAACwAAABwcHQvc2xpZGVNYXN0ZXJzL19yZWxzL3NsaWRlTWFzdGVyMS54bWwucmVsczw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04IiBzdGFuZGFsb25lPSJ5ZXMiPz4KPFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+CjxSZWxhdGlvbnNoaXAgSWQ9InJJZDEiIFR5cGU9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9vZmZpY2VEb2N1bWVudC8yMDA2L3JlbGF0aW9uc2hpcHMvc2xpZGVMYXlvdXQiIFRhcmdldD0iLi4vc2xpZGVMYXlvdXRzL3NsaWRlTGF5b3V0MS54bWwiLz4KPC9SZWxhdGlvbnNoaXBzPlBLAQIUAxQAAAAAAAAAIQDCwACFZAMAAGQDAAATAAAAAAAAAAAAAACAAQAAAABbQ29udGVudF9UeXBlc10ueG1sUEsBAhQDFAAAAAAAAAAhAFfNaN8vAQAALwEAAAsAAAAAAAAAAAAAAIABlQMAAF9yZWxzLy5yZWxzUEsBAhQDFAAAAAAAAAAhAMihofmMAQAAjAEAABQAAAAAAAAAAAAAAIAB7QQAAHBwdC9wcmVzZW50YXRpb24ueG1sUEsBAhQDFAAAAAAAAAAhABjwTFW7AQAAuwEAAB8AAAAAAAAAAAAAAIABqwYAAHBwdC9fcmVscy9wcmVzZW50YXRpb24ueG1sLnJlbHNQSwECFAMUAAAAAAAAACEAsATd+9UDAADVAwAAFQAAAAAAAAAAAAAAgAGjCAAAcHB0L3NsaWRlcy9zbGlkZTEueG1sUEsBAhQDFAAAAAAAAAAhANBzOCg4AQAAOAEAACAAAAAAAAAAAAAAAIABqwwAAHBwdC9zbGlkZXMvX3JlbHMvc2xpZGUxLnhtbC5yZWxzUEsBAhQDFAAAAAAAAAAhANbpkoDvAQAA7wEAACEAAAAAAAAAAAAAAIABIQ4AAHBwdC9zbGlkZUxheW91dHMvc2xpZGVMYXlvdXQxLnhtbFBLAQIUAxQAAAAAAAAAIQDk1H8KOAEAADgBAAAsAAAAAAAAAAAAAACAAU8QAABwcHQvc2xpZGVMYXlvdXRzL19yZWxzL3NsaWRlTGF5b3V0MS54bWwucmVsc1BLAQIUAxQAAAAAAAAAIQDppaTyuQIAALkCAAAhAAAAAAAAAAAAAACAAdERAABwcHQvc2xpZGVNYXN0ZXJzL3NsaWRlTWFzdGVyMS54bWxQSwECFAMUAAAAAAAAACEA0HM4KDgBAAA4AQAALAAAAAAAAAAAAAAAgAHJFAAAcHB0L3NsaWRlTWFzdGVycy9fcmVscy9zbGlkZU1hc3RlcjEueG1sLnJlbHNQSwUGAAAAAAoACgDsAgAASxYAAAAA");
  // ofd：GB/T 33190 最小容器（OFD.xml DocRoot → Doc_0/Document.xml PageArea/Pages
  // → Doc_0/Page_0/Content.xml 双 TextObject），renderer-ofd（DLTech21/ofd.js 链路）。
  putBase64("/previews/notice.ofd", "UEsDBBQAAAAAAAAAIQCzbFnlrAAAAKwAAAAHAAAAT0ZELnhtbDw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04Ij8+CjxvZmQ6T0ZEIHhtbG5zOm9mZD0iaHR0cDovL3d3dy5vZmRzcGVjLm9yZy8yMDE2Ij4KPG9mZDpEb2NCb2R5PjxvZmQ6RG9jUm9vdD5Eb2NfMC9Eb2N1bWVudC54bWw8L29mZDpEb2NSb290Pjwvb2ZkOkRvY0JvZHk+Cjwvb2ZkOk9GRD5QSwMEFAAAAAAAAAAhAAP6DDsiAQAAIgEAABIAAABEb2NfMC9Eb2N1bWVudC54bWw8P3htbCB2ZXJzaW9uPSIxLjAiIGVuY29kaW5nPSJVVEYtOCI/Pgo8b2ZkOkRvY3VtZW50IHhtbG5zOm9mZD0iaHR0cDovL3d3dy5vZmRzcGVjLm9yZy8yMDE2Ij4KPG9mZDpDb21tb25EYXRhPjxvZmQ6UGFnZUFyZWE+PG9mZDpQaHlzaWNhbEJveD4wIDAgNTk1IDg0Mjwvb2ZkOlBoeXNpY2FsQm94Pjwvb2ZkOlBhZ2VBcmVhPjwvb2ZkOkNvbW1vbkRhdGE+CjxvZmQ6UGFnZXM+PG9mZDpQYWdlIElEPSIxMSIgQmFzZUxvYz0iUGFnZV8wL0NvbnRlbnQueG1sIi8+PC9vZmQ6UGFnZXM+Cjwvb2ZkOkRvY3VtZW50PlBLAwQUAAAAAAAAACEA7HJYgAICAAACAgAAGAAAAERvY18wL1BhZ2VfMC9Db250ZW50LnhtbDw/eG1sIHZlcnNpb249IjEuMCIgZW5jb2Rpbmc9IlVURi04Ij8+CjxvZmQ6UGFnZSB4bWxuczpvZmQ9Imh0dHA6Ly93d3cub2Zkc3BlYy5vcmcvMjAxNiI+CjxvZmQ6Q29udGVudD4KPG9mZDpMYXllciBJRD0iMTAxIj4KPG9mZDpUZXh0T2JqZWN0IElEPSIxMDIiIEJvdW5kYXJ5PSI1NiA4MCA0ODAgNDAiIFNpemU9IjI4Ij4KPG9mZDpGaWxsQ29sb3IgVmFsdWU9IjEyIDgyIDE1OCIvPjxvZmQ6VGV4dENvZGU+REJYIEZpbGVzIE9GRCDmoLfkvos8L29mZDpUZXh0Q29kZT4KPC9vZmQ6VGV4dE9iamVjdD4KPG9mZDpUZXh0T2JqZWN0IElEPSIxMDMiIEJvdW5kYXJ5PSI1NiAxNDAgNDgwIDMwIiBTaXplPSIxNiI+CjxvZmQ6RmlsbENvbG9yIFZhbHVlPSI0MCA0MCA0MCIvPjxvZmQ6VGV4dENvZGU+R0IvVCAzMzE5MCDnlLXlrZDlhazmlofmnIDlsI/lrrnlmajvvIgxIOmhte+8iTwvb2ZkOlRleHRDb2RlPgo8L29mZDpUZXh0T2JqZWN0Pgo8L29mZDpMYXllcj4KPC9vZmQ6Q29udGVudD4KPC9vZmQ6UGFnZT5QSwECFAMUAAAAAAAAACEAs2xZ5awAAACsAAAABwAAAAAAAAAAAAAAgAEAAAAAT0ZELnhtbFBLAQIUAxQAAAAAAAAAIQAD+gw7IgEAACIBAAASAAAAAAAAAAAAAACAAdEAAABEb2NfMC9Eb2N1bWVudC54bWxQSwECFAMUAAAAAAAAACEA7HJYgAICAAACAgAAGAAAAAAAAAAAAAAAgAEjAgAARG9jXzAvUGFnZV8wL0NvbnRlbnQueG1sUEsFBgAAAAADAAMAuwAAAFsEAAAAAA==");
  // xmind：XMind 2020 content.json（sheet + rootTopic + children.attached 三层），
  // renderer-mindmap 走 JSZip 解包 + parseXmind2020Json。
  putBase64("/previews/mind.xmind", "UEsDBBQAAAAAAAAAIQDyS1TuLAIAACwCAAAMAAAAY29udGVudC5qc29uW3siaWQiOiAic2hlZXQtMSIsICJjbGFzcyI6ICJzaGVldCIsICJ0aXRsZSI6ICJGaWxlcyBTdHVkaW8g6aKE6KeI55+p6Zi1IiwgInJvb3RUb3BpYyI6IHsiaWQiOiAicm9vdCIsICJjbGFzcyI6ICJ0b3BpYyIsICJ0aXRsZSI6ICLmlofku7bpooTop4giLCAiY2hpbGRyZW4iOiB7ImF0dGFjaGVkIjogW3siaWQiOiAidDEiLCAidGl0bGUiOiAi54mI5byP5paH5Lu2IiwgImNoaWxkcmVuIjogeyJhdHRhY2hlZCI6IFt7ImlkIjogInQxYSIsICJ0aXRsZSI6ICJQREYifSwgeyJpZCI6ICJ0MWIiLCAidGl0bGUiOiAiT0ZEIn1dfX0sIHsiaWQiOiAidDIiLCAidGl0bGUiOiAiT2ZmaWNlIiwgImNoaWxkcmVuIjogeyJhdHRhY2hlZCI6IFt7ImlkIjogInQyYSIsICJ0aXRsZSI6ICJQUFRYIn0sIHsiaWQiOiAidDJiIiwgInRpdGxlIjogIlhMU1gifV19fSwgeyJpZCI6ICJ0MyIsICJ0aXRsZSI6ICLpn7Pop4bpopEiLCAiY2hpbGRyZW4iOiB7ImF0dGFjaGVkIjogW3siaWQiOiAidDNhIiwgInRpdGxlIjogIldBViJ9LCB7ImlkIjogInQzYiIsICJ0aXRsZSI6ICJNSURJIn1dfX1dfX19XVBLAQIUAxQAAAAAAAAAIQDyS1TuLAIAACwCAAAMAAAAAAAAAAAAAACAAQAAAABjb250ZW50Lmpzb25QSwUGAAAAAAEAAQA6AAAAVgIAAAAA");
  // mid：SMF format 0（480tpq，电钢琴音色，C4-E4-G4-C5 上行再回落的两小节音序），
  // capability-midi（@tonejs/midi）解析出轨道/音符统计表。
  putBase64("/previews/scale.mid", "TVRoZAAAAAYAAAABAeBNVHJrAAAAWwD/AwhEQlggRGVtbwDABACQPGSDYIA8AACQQGSDYIBAAACQQ2SDYIBDAACQSGSHQIBIAACQSGSDYIBIAACQQ2SDYIBDAACQQGSDYIBAAACQPGSHQIA8AAD/LwA=");
  // parity-tools：8 MiB 无内容仓大文件（files/readRange 走确定性伪字节），
  // 供分块预览链路在 mock 下可见 4 片 × 2MiB 的真实分块行为。
  // 放独立 /media 目录：/docs 的直接文件数被 dashboardParity 等既有断言锚定。
  put("/media", "dir");
  put("/media/big-sample.bin", "file", 8 * 1024 * 1024);
  for (let i = 0; i < 10_000; i += 1) put(`/10k/file-${String(i).padStart(5, "0")}.txt`, "file", 1024 + i);

  // ---- 本地树（内置 __local__ 连接，模拟真实 sidecar 的本地文件系统）----------
  // 双栏左栏默认面：独立路径空间 + $HOME 家族 quickPaths，供浏览器验证
  // 「左=本地、右=远端」而不与远端 mock 树混淆。
  const localTree = new Map<string, MockEntry>();
  const localContents = new Map<string, Uint8Array>();
  const putLocal = (path: string, kind: "file" | "dir", size = 0) =>
    localTree.set(path.replace(/\/+$/, "") || "/", { kind, size, modifiedAt: stamp(localTree.size % 30) });
  const LOCAL_HOME = "/Users/demo";
  putLocal("/", "dir");
  putLocal(LOCAL_HOME, "dir");
  for (const dir of ["Desktop", "Downloads", "Documents", "Pictures", "Movies"]) putLocal(`${LOCAL_HOME}/${dir}`, "dir");
  putLocal("/Applications", "dir");
  putLocal("/tmp", "dir");
  {
    const localNote = new TextEncoder().encode("本地文件（__local__）\n\n这是 sidecar 本地文件系统的 mock 样例。\n");
    localContents.set(`${LOCAL_HOME}/Documents/notes-local.txt`, localNote);
    putLocal(`${LOCAL_HOME}/Documents/notes-local.txt`, "file", localNote.byteLength);
  }
  {
    const png = window.atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    const bytes = new Uint8Array(png.length);
    for (let i = 0; i < png.length; i += 1) bytes[i] = png.charCodeAt(i);
    localContents.set(`${LOCAL_HOME}/Desktop/logo-local.png`, bytes);
    putLocal(`${LOCAL_HOME}/Desktop/logo-local.png`, "file", bytes.byteLength);
  }
  putLocal(`${LOCAL_HOME}/Downloads/installer.dmg`, "file", 32 * 1024 * 1024);
  // 文件类型图标验证样例：集中放 Documents，一次导航即可目视核对各类型图标。
  for (const name of ["报告.docx", "预算.xlsx", "路演.pptx", "手册.pdf", "backup.zip", "song.mp3", "clip.mp4", "app.py"]) {
    putLocal(`${LOCAL_HOME}/Documents/${name}`, "file", 4096);
  }

  const connections = new Map([
    ["mock-conn", { tree, contents, readOnly }],
    ["__local__", { tree: localTree, contents: localContents, readOnly: false }],
  ]);
  const connectionIdOf = (value: unknown) => String(value ?? "mock-conn");
  const storageFor = (value: unknown) => {
    const id = connectionIdOf(value);
    const storage = connections.get(id);
    if (!storage) throw new Error(`Unknown connectionId '${id}'; connect first`);
    return storage;
  };
  const treeFor = (id: unknown) => storageFor(id).tree;
  const contentsFor = (id: unknown) => storageFor(id).contents;

  // 对齐 lifecycle 的必填字段与 custom JSON 形状；只保留 id/策略，不存凭据。
  function parseConnection(payload: Record<string, unknown>) {
    const connection = payload.connection as Record<string, unknown> | undefined;
    if (!connection || typeof connection !== "object" || Array.isArray(connection)) throw new Error("Missing connection payload");
    if (typeof connection.id !== "string" || !connection.id.trim()) throw new Error("Missing connection id");
    const config = connection.external_config as Record<string, unknown> | undefined;
    const protocol = typeof config?.protocol === "string" ? config.protocol.trim() : "";
    if (!protocol) throw new Error("Missing protocol in external_config");
    const protocols = ["fs", "s3", "gcs", "azblob", "obs", "oss", "cos", "qiniu", "r2", "wasabi", "spaces", "scaleway", "idrive", "us3", "ecloud", "nos", "bos", "tos", "ks3", "webdav", "ftp", "sftp", "smb", "sftp-native", "aliyun-drive", "dropbox", "gdrive", "koofr", "onedrive", "pcloud", "seafile", "yandex-disk", "rclone-custom", ...GENERIC_PROTOCOL_IDS];
    if (!protocols.includes(protocol)) throw new Error(`Unsupported protocol '${protocol}'; expected one of ${protocols.join(", ")}`);
    if (GENERIC_PROTOCOL_IDS.has(protocol) || protocol === "rclone-custom") {
      // 对齐 sidecar 透传语义：通用协议的协议值即 rclone backend type，
      // 旧别名额外携带 service 字段；config JSON 必须是对象。
      let custom = config?.config ?? {};
      if (typeof custom === "string") {
        try { custom = JSON.parse(custom); } catch { throw new Error("Invalid service config JSON"); }
        if (!custom || typeof custom !== "object" || Array.isArray(custom)) throw new Error("Service config JSON must be an object");
      }
      if (!custom || typeof custom !== "object" || Array.isArray(custom)) throw new Error("Service config must be a JSON object");
      const service = typeof config?.service === "string" ? config.service.trim() : "";
      if (!GENERIC_PROTOCOL_IDS.has(protocol)) {
        if (!service) throw new Error("custom backend requires 'service'");
        if (!/^[a-z0-9]+$/.test(service)) throw new Error(`Invalid custom backend type '${service}' (lowercase letters and digits only)`);
      }
    }
    return { id: connection.id.trim(), readOnly: connection.read_only === true || config?.read_only === true };
  }

  const children = (source: Map<string, MockEntry>, dir: string): Array<Record<string, unknown>> => {
    const base = dir.replace(/\/+$/, "");
    const prefix = base === "" || base === "/" ? "/" : `${base}/`;
    return [...source.entries()]
      .filter(([path, entry]) => path !== "/" && path.startsWith(prefix) && !path.slice(prefix.length).includes("/"))
      .map(([path, entry]) => ({ name: path.slice(prefix.length), path, kind: entry.kind, size: entry.size, modifiedAt: entry.modifiedAt }))
      .sort((a, b) => String(a.name).localeCompare(String(b.name)));
  };
  // （copy/move/rename 的目录/存在判断已随 P2-13② 收口进各 case 内部，见下。）
  const assertOk = (path: string) => {
    if (path.includes("error")) throw new Error(`mock backend failure for ${path}`);
    // P2-1 夹具：notfound 路径注入「不存在」类业务错误（友好映射层走查用）
    if (path.includes("notfound")) throw new Error(`NotFound: ${path}`);
  };

  // P2-13④：压缩包条目来源表（B-ARCHIVE 夹具层缺位收口）。
  // mock 不存真实 tar 结构：compress 时记录源路径集合，archiveList 按当前树
  // 展开为归档内相对路径条目；三个预置样例归档同样预登记。
  const archiveSources = new Map<string, string[]>();
  archiveSources.set("/backup.zip", ["/docs/readme.md", "/docs/notes.txt", "/docs/logo.png"]);
  archiveSources.set("/docs/site.tar.gz", ["/docs"]);
  archiveSources.set("/docs/dump.tar", ["/docs/data.bin", "/docs/report.pdf"]);

  // ---- 异步 job 表（降级 copy/move/rename）--------------------------------
  let jobSeq = 0;
  const jobs = new Map<string, Record<string, unknown>>();
  /** files/bwlimit 的持久化值（sidecar prefs 假身；空 = 不限）。 */
  let mockBwlimit: string | null = null;
  const timers = new Map<string, number[]>();

  // ---- 流式列表会话（files/listStream P1）----------------------------------
  // requestId → 会话状态：files/listCancel 置 canceled 后分批 emit 自行停止。
  let mockStreamSeq = 0;
  const streamSessions = new Map<string, { canceled: boolean }>();

  function emit(method: string, payload: Record<string, unknown>) {
    for (const listener of eventListeners) listener({ method, params: payload });
  }
  // 测试驱动器挂钩：记录最近一次安装实例的 emit（emitUiIntent 走此通道，
  // 与 sidecar emitter.Event 同面）。未安装时为 null。
  emitEventRef = emit;

  function runJob(jobId: string, kind: string, source: string, target: string, apply: () => void, cancel: { flag: boolean }, sourceConnectionId: string, targetConnectionId = sourceConnectionId) {
    // P2-8：remotePath 与真实 sidecar（transfers.rs，camelCase）契约对齐，
    // 轮询兜底不再把可读任务标题冲掉成裸 jobId。
    jobs.set(jobId, { jobId, kind, status: "queued", sourceConnectionId, targetConnectionId, sourcePath: source, targetPath: target, remotePath: `${source} → ${target}`, filesDone: 0, filesTotal: 2, bytesDone: 0, bytesTotal: 4096 });
    emit("files/transfer/progress", { jobId, kind, state: "queued", remotePath: `${source} → ${target}` });
    const schedule = (ms: number, fn: () => void) => {
      const timer = window.setTimeout(() => {
        if (cancel.flag) return;
        fn();
      }, ms);
      timers.set(jobId, [...(timers.get(jobId) ?? []), timer]);
    };
    schedule(50, () => {
      jobs.set(jobId, { ...jobs.get(jobId)!, status: "running", bytesDone: 2048 });
      emit("files/transfer/progress", { jobId, kind, state: "running", filesDone: 1, filesTotal: 2, bytesDone: 2048, bytesTotal: 4096, remotePath: `${source} → ${target}` });
    });
    schedule(400, () => {
      if (cancel.flag) return;
      // 对齐 sidecar 失败契约：job 内异常 → failed progress 事件（带 error），
      // 供 TransferPanel 失败态/重试按钮的全流程 UI 验证。源路径含 "error"
      // 在提交期就会被 assertOk 拒绝，因此 job 级失败用目标路径注入
      // （目标含 "fail" → 执行期失败；assertOk 只拒 "error"，可过提交）。
      try {
        if (target.includes("fail")) throw new Error(`mock job failure for ${target}`);
        apply();
      } catch (cause) {
        const message = cause instanceof Error ? cause.message : String(cause);
        jobs.set(jobId, { ...jobs.get(jobId)!, status: "failed", error: message });
        emit("files/transfer/progress", { jobId, kind, state: "failed", error: message, remotePath: `${source} → ${target}` });
        return;
      }
      jobs.set(jobId, { ...jobs.get(jobId)!, status: "completed", filesDone: 2, bytesDone: 4096 });
      emit("files/transfer/progress", { jobId, kind, state: "completed", filesDone: 2, filesTotal: 2, bytesDone: 4096, bytesTotal: 4096, remotePath: `${source} → ${target}` });
    });
  }

  function copyEntryBetween(
    sourceTree: Map<string, MockEntry>,
    sourceContents: Map<string, Uint8Array>,
    targetTree: Map<string, MockEntry>,
    targetContents: Map<string, Uint8Array>,
    source: string,
    target: string,
  ) {
    // P2-13②：跨连接复制/移动按 sourceConnectionId/targetConnectionId 路由
    // 源/目标树与内容仓；同连接时两个参数对相同，行为与旧 copyEntry 一致。
    const src = sourceTree.get(source.replace(/\/+$/, ""));
    if (!src) return;
    const cleanTarget = target.replace(/\/+$/, "") || "/";
    if (src.kind === "dir") {
      const prefix = `${source.replace(/\/+$/, "")}/`;
      for (const [path, entry] of [...sourceTree.entries()]) {
        if (!path.startsWith(prefix)) continue;
        targetTree.set(`${cleanTarget}/${path.slice(prefix.length)}`, entry);
        const bytes = sourceContents.get(path);
        if (bytes) targetContents.set(`${cleanTarget}/${path.slice(prefix.length)}`, bytes);
      }
    }
    const bytes = sourceContents.get(source.replace(/\/+$/, ""));
    if (bytes) targetContents.set(cleanTarget, bytes);
    targetTree.set(cleanTarget, { kind: src.kind, size: src.size, modifiedAt: new Date().toISOString() });
  }

  function deleteEntry(path: string, source: Map<string, MockEntry> = tree) {
    const base = path.replace(/\/+$/, "");
    source.delete(base);
    for (const key of [...source.keys()]) if (key.startsWith(`${base}/`)) source.delete(key);
  }

  // ---- 上传槽 ---------------------------------------------------------------
  // P1-5 夹具契约修复：记录 start 时的 connectionId，finish 按 connectionId
  // 落对应树/内容（__local__ → 本地树），与真实 sidecar 的按连接落盘一致。
  const uploads = new Map<string, { path: string; size: number; received: number; bytes: Uint8Array; connectionId: unknown }>();
  const downloads = new Map<string, { path: string; size: number; received: number; connectionId: unknown; timer: number; canceled: boolean }>();

  // ---- 本地挂载（mock 演示 webdav 策略；不触达真实网关/挂载点）----------------
  // files/mount 固定走 webdav 兜底（mock 无 FUSE 概念），mountStatus/unmount
  // 按连接过滤，供设置弹窗「本地挂载」面板走查。
  const mockMounts = new Map<string, { strategy: string; gatewayPort: number; connectionId: unknown }>();
  let mockMountSeq = 0;
  let mockEditSeq = 0;

  // ---- 本机共享（files/serve/*，对标 rclone serve 家族）-----------------------
  // 内存 map 假身（serveId → 行）：start 分配伪回环 URL，list 按连接过滤，
  // stop 幂等删除。不发真实网络请求（runJob 不需要）。
  const mockServes = new Map<string, { serveType: string; url: string; connectionId: string }>();
  let mockServeSeq = 0;

  // ---- 监听器 ---------------------------------------------------------------
  const eventListeners: Array<(event: DbxPluginEvent) => void> = [];
  // mock 镜像当前宿主桥的二进制事件形状（零拷贝 data 字段），与真实宿主一致。
  const binaryListeners: Array<(event: { channel: string; data?: Uint8Array }) => void> = [];

  // ---- 审计（files/audit/list 对齐后端 store 审计语义）-----------------------
  // 与 backend/src/main.rs::audit_list_response 同形：{entries:[{at,action,
  // connectionId,path,result}]}，最新在前；limit clamp 1..=1000（缺省 100）。
  const auditLog: Array<{ at: string; action: string; connectionId: string; path: string; result: string }> = [];
  function recordAudit(action: string, path: string, connectionId: unknown, result = "ok") {
    auditLog.push({ at: new Date().toISOString(), action, connectionId: connectionIdOf(connectionId), path, result });
  }

  const matchesConnection = (job: Record<string, unknown>, id: string | null | undefined) =>
    id == null || job.connectionId === id || job.sourceConnectionId === id || job.targetConnectionId === id;

  async function invoke(method: string, payload: Record<string, unknown>): Promise<unknown> {
    const p = payload as Record<string, string | number | undefined>;
    const str = (key: string) => String(p[key] ?? "");
    switch (method) {
      case "connection/test":
      case "connection/connect": {
        const connection = parseConnection(payload);
        if (method === "connection/test") {
          if (params.get("connectionTest") === "fail") throw new Error("Storage check failed: mock endpoint is unreachable");
          return { success: true, message: "Storage backend reachable" };
        }
        if (connection.id === "__local__") throw new Error("connectionId '__local__' is reserved for the built-in local filesystem");
        const storage = connections.get(connection.id) ?? {
          tree: new Map<string, MockEntry>([["/", { kind: "dir", size: 0, modifiedAt: stamp(0) }]]),
          contents: new Map<string, Uint8Array>(),
        };
        connections.set(connection.id, { ...storage, readOnly: connection.readOnly });
        return { success: true };
      }
      case "connection/disconnect": {
        const id = (payload.connection as Record<string, unknown> | undefined)?.id;
        if (typeof id !== "string" || !id) throw new Error("Missing connection id");
        for (const [taskId, job] of jobs) {
          if (!taskId.startsWith("__") && matchesConnection(job, id) && (job.status === "queued" || job.status === "running")) {
            await invoke("files/transfer/cancel", { taskId });
          }
        }
        if (id !== "__local__") connections.delete(id);
        return { success: true };
      }
      case "files/audit/list": {
        const limitRaw = p.limit;
        const limit = limitRaw === undefined || limitRaw === null
          ? 100
          : limitRaw;
        if (typeof limit !== "number" || !Number.isInteger(limit) || limit < 0 || limit >= 2 ** 64) throw new Error("Invalid request parameters: limit must be a non-negative integer");
        const connection = typeof p.connectionId === "string" && p.connectionId ? p.connectionId : undefined;
        const filtered = auditLog.filter((entry) => !connection || entry.connectionId === connection);
        return { entries: filtered.slice().reverse().slice(0, Math.min(Math.max(limit, 1), 1000)) };
      }
      case "files/list": {
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        assertOk(str("path"));
        // mock 不做封顶截断；truncated 与真实 sidecar 响应同形（缺省 false）。
        return { entries: children(treeFor(p.connectionId), str("path")), truncated: false };
      }
      case "files/listPaged": {
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        const all = children(treeFor(p.connectionId), str("path"));
        const page = Number(p.page ?? 1);
        const size = Number(p.pageSize ?? 200);
        return { entries: all.slice((page - 1) * size, page * size), total: all.length };
      }
      case "files/listStream": {
        // 流式列表（P1）：ack 先行返回 requestId，随后按批次 emit files/list/chunk
        // 事件（对齐真实 sidecar：ack 应答先于事件）。批次大小/间隔由
        // &streamBatch/&streamInterval 控制，&streamFailAfter=N 在第 N 批后注入
        // 失败帧（error + partialCount，entries 空），&streamDisabled=1 模拟
        // 旧 sidecar 的 ack 失败。mock 不做截断。
        if (streamDisabled) throw new Error("files/listStream is disabled: not supported by this sidecar");
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        assertOk(str("path"));
        const requestId = `mock-stream-${++mockStreamSeq}`;
        const stream = { canceled: false };
        streamSessions.set(requestId, stream);
        const all = children(treeFor(p.connectionId), str("path"));
        const emitBatch = (start: number, seq: number) => {
          // 取消（files/listCancel）或会话已终止：停止分批。
          if (stream.canceled || !streamSessions.has(requestId)) return;
          if (streamFailAfter !== null && seq > streamFailAfter) {
            streamSessions.delete(requestId);
            // 失败帧与后端同形：error 非空 + partialCount，不置 done。
            emit("files/list/chunk", { requestId, seq, entries: [], done: false, error: `mock stream failure for ${str("path")}`, partialCount: start });
            return;
          }
          const slice = all.slice(start, start + streamBatch);
          const done = start + slice.length >= all.length;
          emit("files/list/chunk", { requestId, seq, entries: slice, done, ...(done ? { total: all.length } : {}) });
          if (done) {
            streamSessions.delete(requestId);
            return;
          }
          window.setTimeout(() => emitBatch(start + slice.length, seq + 1), streamInterval);
        };
        // &streamEventsBeforeAck=1：第一批在 invoke 应答前同步 emit——复现真实
        // 宿主桥「事件先于 ack」的写入顺序（前端预缓冲重放的回归夹具）；
        // 默认 invoke 返回 ack 之后再开始 emit（setTimeout 排队），保证前端
        // 先登记 requestId 再收到 chunk 事件。
        if (streamEventsBeforeAck) emitBatch(0, 1);
        else window.setTimeout(() => emitBatch(0, 1), streamInterval);
        return { requestId };
      }
      case "files/listCancel": {
        const stream = streamSessions.get(str("requestId"));
        if (stream) stream.canceled = true;
        // 与后端契约一致：未知/已结束的 requestId 返回 false。
        return { cancelled: Boolean(stream) };
      }
      case "files/capabilities":
        return { scheme: "mock", list: true, write: true, read: true, stat: true, delete: true, createDir: true, copy: true, rename: true, presign, readOnly: storageFor(p.connectionId).readOnly };
      case "files/size": {
        // 对标真实 sidecar：目录按直接子项汇总条目数与字节数；文件返回自身。
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        const path = str("path");
        assertOk(path);
        const entry = treeFor(p.connectionId).get(path.replace(/\/+$/, "") || "/");
        if (!entry) throw new Error(`NotFound: ${path}`);
        if (entry.kind === "file") return { count: 1, bytes: entry.size };
        let count = 0;
        let bytes = 0;
        for (const child of children(treeFor(p.connectionId), path)) {
          if (child.kind === "file") {
            count += 1;
            bytes += Number(child.size);
          }
        }
        return { count, bytes };
      }
      case "files/publicLink": {
        await new Promise((resolve) => setTimeout(resolve, delayMs));
        if (!presign) throw new Error("operation not supported: backend has no public links");
        const path = str("path");
        assertOk(path);
        return { url: `https://mock.example/presigned${path}?X-Expires=audit` };
      }
      case "files/quickPaths": {
        const source = treeFor(p.connectionId);
        // mock 无真实 $HOME：远端树返回根目录 + 实际存在的样例目录 chips；
        // __local__ 连接返回本地 home 家族（对齐真实 sidecar 的 fs 行为）。
        if (p.connectionId === "__local__") {
          return {
            paths: [
              { key: "root", path: "/" },
              { key: "home", path: LOCAL_HOME },
              ...["Desktop", "Downloads", "Documents", "Pictures"]
                .map((dir) => ({ key: dir.toLowerCase(), path: `${LOCAL_HOME}/${dir}` }))
                .filter((item) => localTree.has(item.path)),
            ],
          };
        }
        return {
          paths: [
            { key: "root", path: "/" },
            ...["desktop", "downloads", "documents", "pictures"]
              .filter((key) => source.has(`/${key}`))
              .map((key) => ({ key, path: `/${key}` })),
          ],
        };
      }
      case "files/stat": {
        const path = str("path");
        assertOk(path);
        const entry = treeFor(p.connectionId).get(path.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${path}`);
        return { entry: { name: path.split("/").filter(Boolean).pop() ?? "/", path, kind: entry.kind, size: entry.size, modifiedAt: entry.modifiedAt } };
      }
      case "files/mkdir": {
        const path = str("path");
        assertOk(path);
        // 按连接路由（P-FILES）：双栏左栏 __local__ 的新建文件夹落本地树。
        treeFor(p.connectionId).set(path.replace(/\/+$/, "") || "/", { kind: "dir", size: 0, modifiedAt: new Date().toISOString() });
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/delete": {
        const path = str("path");
        assertOk(path);
        // R3-P2-2：按连接路由（此前写死远端树，双栏左栏删除「成功即无效」，
        // 与 P1-5 修复前的上传假成功同构）。
        treeFor(p.connectionId).delete(path.replace(/\/+$/, ""));
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/purge": {
        const path = str("path");
        assertOk(path);
        if (path.replace(/\/+$/, "") === "" || path.replace(/\/+$/, "") === "/") throw new Error("Purge of the connection root '/' is refused");
        // R3-P2-2：按连接路由（同 delete）。
        deleteEntry(path, treeFor(p.connectionId));
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/rename": {
        const source = str("path");
        const target = str("newPath");
        assertOk(source);
        // 按连接路由（与 copy/move 的 P2-13② 收口一致）：__local__ 栏重命名落本地树。
        const renameTree = treeFor(p.connectionId);
        const renameContents = contentsFor(p.connectionId);
        if (renameTree.get(source.replace(/\/+$/, ""))?.kind === "dir" || forceJob) {
          const jobId = `mock-job-${++jobSeq}`;
          const cancel = { flag: false };
          jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
          runJob(jobId, "rename", source, target, () => {
            copyEntryBetween(renameTree, renameContents, renameTree, renameContents, source, target);
            deleteEntry(source, renameTree);
          }, cancel, connectionIdOf(p.connectionId));
          return { success: true, transport: "job", jobId };
        }
        const entry = renameTree.get(source.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${source}`);
        copyEntryBetween(renameTree, renameContents, renameTree, renameContents, source, target);
        deleteEntry(source, renameTree);
        recordAudit(method, source, p.connectionId);
        return { success: true, transport: "native", jobId: null };
      }
      case "files/copy":
      case "files/move":
      case "files/syncDir":
      case "files/copyDir": {
        const source = str("sourcePath");
        const target = str("targetPath");
        assertOk(source);
        const directoryJob = method === "files/syncDir" || method === "files/copyDir";
        if (directoryJob && (typeof p.sourceConnectionId !== "string" || typeof p.targetConnectionId !== "string")) {
          throw new Error("Invalid request parameters: sourceConnectionId and targetConnectionId are required");
        }
        // P2-13②：按契约路由 sourceConnectionId/targetConnectionId（缺省回落
        // connectionId / 远端树），跨连接双栏复制本地 → 远端可自洽走查。
        const sourceTree = treeFor(p.sourceConnectionId ?? p.connectionId);
        const sourceContents = contentsFor(p.sourceConnectionId ?? p.connectionId);
        const targetTree = treeFor(p.targetConnectionId ?? p.connectionId);
        const targetContents = contentsFor(p.targetConnectionId ?? p.connectionId);
        const sourceId = connectionIdOf(p.sourceConnectionId ?? p.connectionId);
        const targetId = connectionIdOf(p.targetConnectionId ?? p.connectionId);
        if (sourceId === targetId && target.replace(/\/+$/, "") === source.replace(/\/+$/, "")) throw new Error("Destination must differ from the source");
        const sourceIsDir = sourceTree.get(source.replace(/\/+$/, ""))?.kind === "dir";
        if (directoryJob || sourceIsDir || forceJob || sourceId !== targetId) {
          const jobId = `mock-job-${++jobSeq}`;
          const cancel = { flag: false };
          jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
          const move = method === "files/move";
          const sync = method === "files/syncDir";
          runJob(jobId, sync ? "syncDir" : method === "files/copyDir" ? "copyDir" : move ? "move" : "copy", source, target, () => {
            copyEntryBetween(sourceTree, sourceContents, targetTree, targetContents, source, target);
            if (move) deleteEntry(source, sourceTree);
          }, cancel, sourceId, targetId);
          return directoryJob ? { jobId } : { success: true, transport: "job", jobId };
        }
        const entry = sourceTree.get(source.replace(/\/+$/, ""));
        if (!entry) throw new Error(`NotFound: ${source}`);
        copyEntryBetween(sourceTree, sourceContents, targetTree, targetContents, source, target);
        if (method === "files/move") deleteEntry(source, sourceTree);
        recordAudit(method, source, sourceId);
        return { success: true, transport: "native", jobId: null };
      }
      case "files/search": {
        const term = str("pattern").toLowerCase();
        const rootPath = (typeof p.root === "string" && p.root ? p.root : "/").replace(/\/+$/, "") || "/";
        const hits: Array<Record<string, unknown>> = [];
        const remoteTree = treeFor(p.connectionId);
        for (const [entryPath, entry] of remoteTree) {
          if (entry.kind !== "file") continue;
          if (!entryPath.startsWith(rootPath) && rootPath !== "/") continue;
          const name = entryPath.split("/").pop() ?? "";
          if (term && name.toLowerCase().includes(term.toLowerCase())) {
            hits.push({ path: entryPath, size: entry.size ?? 0, modifiedAt: entry.modifiedAt ?? "" });
          }
        }
        return { entries: hits.slice(0, typeof p.limit === "number" ? p.limit : 200), truncated: false, scanned: hits.length };
      }
      case "files/copyurl": {
        const dirPath = str("dirPath");
        const url = str("url");
        assertOk(dirPath);
        const cleanUrl = url.split(/[?#]/)[0] ?? url;
        const auto = cleanUrl.split("/").filter(Boolean).pop() ?? "download";
        const filename = typeof p.filename === "string" && p.filename ? p.filename : auto;
        const target = `${dirPath.replace(/\/+$/, "")}/${filename}`;
        const urlTree = treeFor(p.connectionId);
        const urlContents = contentsFor(p.connectionId);
        urlTree.set(target, { kind: "file", size: 15, modifiedAt: new Date().toISOString() });
        urlContents.set(target, new TextEncoder().encode(`imported:${url}`));
        recordAudit(method, target, p.connectionId);
        return { path: target, filename };
      }
      case "files/bisync/state": {
        // mock：默认视为已有同步状态；URL 带 bisyncNew=1 时返回首次态。
        const isNew = new URLSearchParams(window.location.search).has("bisyncNew");
        return { session: "mock-pair", state: isNew ? "new" : "synced" };
      }
      case "files/bisync/start": {
        const bs = str("sourcePath");
        const bt = str("targetPath");
        assertOk(bs);
        const bJobId = `mock-job-${++jobSeq}`;
        const bCancel = { flag: false };
        jobs.set(`__cancel_${bJobId}`, bCancel as unknown as Record<string, unknown>);
        runJob(bJobId, "bisync", bs, bt, () => {}, bCancel, connectionIdOf(p.sourceConnectionId ?? p.connectionId), connectionIdOf(p.targetConnectionId ?? p.connectionId));
        return { jobId: bJobId };
      }
      case "files/about": {
        // 本地内存树的假容量：按条目数粗略估算，让侧栏占用条有东西可渲染。
        const aboutId = connectionIdOf(p.connectionId);
        const aboutTree = treeFor(p.connectionId);
        const count = [...aboutTree.values()].filter((entry) => entry.kind === "file").length;
        const used = count * 8192;
        const total = 4 * 1024 * 1024 * 1024;
        return { used, total, free: total - used, _conn: aboutId };
      }
      case "files/check": {
        const checkSrc = str("sourcePath");
        const checkDst = str("targetPath");
        assertOk(checkSrc);
        const jobId = `mock-job-${++jobSeq}`;
        const cancel = { flag: false };
        jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
        runJob(jobId, "check", checkSrc, checkDst, () => {}, cancel, connectionIdOf(p.sourceConnectionId ?? p.connectionId), connectionIdOf(p.targetConnectionId ?? p.connectionId));
        // 真实 sidecar 在终态事件/轮询行携带差异摘要；mock 给一个确定性示例，
        // 让传输面板的着色分级（identical 绿 / 差异橙）可见。
        const checkRecord = jobs.get(jobId);
        if (checkRecord) {
          checkRecord.checkSummary = "1 differences (missing on source: 0, missing on target: 1, differ: 0, errors: 0)";
        }
        return { jobId };
      }
      case "files/checksum/verify": {
        // SUM 校验（批次7）假实现：与 files/check 同形态的 check 作业，
        // 被核验目录 = SUM 文件父目录（后端真实语义）。
        const sumPath = str("sumPath");
        assertOk(sumPath);
        const parent = sumPath.split("/").filter(Boolean).slice(0, -1).join("/");
        const dirPath = parent ? `/${parent}` : "/";
        const jobId = `mock-job-${++jobSeq}`;
        const cancel = { flag: false };
        jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
        runJob(jobId, "check", dirPath, dirPath, () => {}, cancel, connectionIdOf(p.connectionId));
        return { jobId };
      }
      case "files/hashsum": {
        const hashPath = str("path");
        assertOk(hashPath);
        const hashTree = treeFor(p.connectionId);
        const hashEntry = hashTree.get(hashPath.replace(/\/+$/, ""));
        if (!hashEntry || hashEntry.kind !== "dir") throw new Error(`NotFound: ${hashPath}`);
        const base = hashPath.split("/").filter(Boolean).pop() ?? "dir";
        const parent = hashPath.split("/").filter(Boolean).slice(0, -1).join("/");
        const sumPath = parent ? `/${parent}/${base}.md5` : `/${base}.md5`;
        recordAudit(method, hashPath, p.connectionId);
        return { path: sumPath, hashType: typeof p.hashType === "string" ? p.hashType : "md5", files: 1 };
      }
      case "files/cleanup": {
        recordAudit(method, "/", p.connectionId);
        return { success: true };
      }
      case "files/rmdirs": {
        const rmdirsPath = str("path");
        assertOk(rmdirsPath);
        recordAudit(method, rmdirsPath, p.connectionId);
        return { success: true };
      }
      case "files/bwlimit": {
        // sidecar prefs 语义的假实现：空参读取，"off"/空串清除，其余原样保存。
        const rate = p.rate;
        if (rate !== undefined) {
          if (typeof rate !== "string") throw new Error("Invalid request parameters: rate must be a string");
          const normalized = rate.trim();
          if (normalized && normalized !== "off" && !/^\d+(\.\d+)?[kKmMgGtT]?[bB]?(:\d+(\.\d+)?[kKmMgGtT]?[bB]?)?$/.test(normalized)) {
            // 与真实 sidecar 对齐：rclone 拒绝无法解析的限速值（bad bwlimit）。
            throw new Error("bad bwlimit: invalid rate");
          }
          mockBwlimit = normalized === "off" || !normalized ? null : normalized;
        }
        return { rate: mockBwlimit };
      }
      case "files/transfers/list":
      case "files/transfers/clear": {
        const connection = payload.connectionId;
        if (connection != null && typeof connection !== "string") throw new Error("Invalid request parameters: connectionId must be a string");
        if (method === "files/transfers/list") {
          return { jobs: [...jobs.entries()].filter(([key, job]) => !key.startsWith("__") && matchesConnection(job, connection)).map(([, job]) => job) };
        }
        // 与 sidecar 语义一致：仅清完成态，queued/running 不动。
        let cleared = 0;
        for (const [key, job] of [...jobs.entries()]) {
          if (key.startsWith("__") || !matchesConnection(job, connection)) continue;
          const status = String((job as Record<string, unknown>).status ?? "");
          if (status === "completed" || status === "failed" || status === "canceled") {
            jobs.delete(key);
            jobs.delete(`__cancel_${key}`);
            timers.delete(key);
            cleared += 1;
          }
        }
        return { cleared };
      }
      case "files/transfer/status": {
        const job = jobs.get(str("jobId"));
        if (!job) throw new Error(`Unknown jobId '${str("jobId")}'`);
        return { job, kind: job.taskId ? "transfer" : "dirJob" };
      }
      case "files/transfers/delete": {
        // 镜像 sidecar 语义：仅删除已结束记录，queued/running 拒绝；
        // 未知 id 返回 removed: 0（前端按无变化处理）。
        const key = str("taskId");
        const job = jobs.get(key);
        if (!job) return { removed: 0 };
        const status = String((job as Record<string, unknown>).status ?? "");
        if (status === "queued" || status === "running") throw new Error("Transfer is still in progress; cancel it first");
        jobs.delete(key);
        jobs.delete(`__cancel_${key}`);
        timers.delete(key);
        return { removed: 1 };
      }
      case "files/local/capabilities": {
        // mock 模拟 web 宿主：无本机落盘，前端走宿主保存/浏览器兜底路径；
        // ?local=1 时模拟桌面宿主（下载目录/外部打开可用）。
        if (demoLocal) {
          return { canSaveLocal: true, downloadsDir: "/Users/demo/Downloads", platform: demoPlatform };
        }
        return { canSaveLocal: false, downloadsDir: "", platform: "web" };
      }
      case "files/local/detect-apps": {
        // ?local=1 平台预设演示：返回一组伪路径（校验桩恒通过），不含真实探测。
        if (!demoLocal) return { platform: "web", apps: [] };
        const presets: Record<string, Array<{ id: string; name: string; path: string }>> = {
          macos: [
            { id: "wps", name: "WPS Office", path: "/Applications/wpsoffice.app" },
            { id: "excel", name: "Microsoft Excel", path: "/Applications/Microsoft Excel.app" },
            { id: "libreoffice", name: "LibreOffice", path: "/Applications/LibreOffice.app" },
          ],
          windows: [
            { id: "wps", name: "WPS Office", path: "C:\\Program Files\\Kingsoft\\WPS Office\\ksolaunch.exe" },
            { id: "excel", name: "Microsoft Excel", path: "C:\\Program Files\\Microsoft Office\\root\\Office16\\EXCEL.EXE" },
          ],
          linux: [
            { id: "libreoffice", name: "LibreOffice", path: "/usr/bin/libreoffice" },
            { id: "vscode", name: "VS Code", path: "/usr/bin/code" },
          ],
        };
        return { platform: demoPlatform, apps: presets[demoPlatform] ?? [] };
      }
      case "files/local/validate-directory": {
        // mock 不探测真实文件系统：绝对路径形态即通过，保持设置保存链路可走查。
        const dir = str("path");
        if (!dir) throw new Error("Missing path");
        if (!dir.startsWith("/") && !/^[A-Za-z]:[\\/]/.test(dir)) {
          throw new Error("not an absolute path");
        }
        return { valid: true, path: dir };
      }
      case "files/local/validate-open-app": {
        // mock 不探测真实文件系统：非空即通过，保持设置链路可走查。
        const app = str("path");
        if (!app) throw new Error("Missing path");
        return { valid: true, path: app };
      }
      case "files/mount": {
        const mountId = `mock-mount-${++mockMountSeq}`;
        const gatewayPort = 40000 + (mockMountSeq % 1000);
        mockMounts.set(mountId, { strategy: "webdav", gatewayPort, connectionId: connectionIdOf(p.connectionId) });
        return { mountId, strategy: "webdav", gatewayPort, gatewayUrl: `http://127.0.0.1:${gatewayPort}/tok/${mountId}/`, fallbackReason: "mock: no FUSE driver" };
      }
      case "files/mountStatus": {
        const wanted = p.connectionId == null ? null : connectionIdOf(p.connectionId);
        const mounts = [...mockMounts.entries()]
          .filter(([, row]) => wanted == null || row.connectionId === wanted)
          .map(([mountId, row]) => ({ mountId, strategy: row.strategy, readOnly: true, gatewayPort: row.gatewayPort, mounted: true }));
        return { mounts };
      }
      // VFS 缓存管理（批次5）：mock 只有 webdav 兜底行 → refresh 全部计入
      // skipped；stats 无 rclone 行，只回空挂载列表（形状与真实 sidecar 一致）。
      case "files/mount/refresh": {
        const mountId = str("mountId");
        let refreshed = 0;
        let skipped = 0;
        const wantedId = p.connectionId == null ? null : connectionIdOf(p.connectionId);
        for (const [id, row] of mockMounts.entries()) {
          if (mountId && id !== mountId) continue;
          if (wantedId != null && row.connectionId !== wantedId) continue;
          if (row.strategy === "rclone") refreshed += 1;
          else skipped += 1;
        }
        if (mountId && refreshed + skipped === 0) throw new Error("Mount not found");
        return { refreshed, skipped, errors: [] };
      }
      case "files/mount/stats": {
        const wanted = p.connectionId == null ? null : connectionIdOf(p.connectionId);
        const mounts = [...mockMounts.entries()]
          .filter(([, row]) => wanted == null || row.connectionId === wanted)
          .filter(([, row]) => row.strategy === "rclone")
          .map(([mountId]) => ({
            mountId,
            strategy: "rclone",
            stats: { metadataCache: { dirs: 1, files: 0 } },
          }));
        return { mounts, skipped: mockMounts.size - mounts.length };
      }
      case "files/local/reveal": {
        const target = str("path");
        if (!target) throw new Error("Missing path");
        return { success: true };
      }
      case "files/remote-edit/open": {
        // 打开方式（远程编辑本地副本）：mock 立即回会话（真实链路由 sidecar
        // 拉临时副本/启动应用/监视回传并经 files/remote-edit/state 事件回报）。
        const remote = str("remotePath");
        if (!remote) throw new Error("Missing remotePath");
        const name = remote.split("/").pop() || "file";
        return { key: `mock-edit-${++mockEditSeq}`, localPath: `/tmp/dbx-mock-edit/${name}` };
      }
      case "files/remote-edit/status": {
        return { sessions: [] };
      }
      case "files/remote-edit/close": {
        if (!str("key")) throw new Error("Missing key");
        return { success: true };
      }
      case "files/unmount": {
        const mountId = str("mountId");
        if (mountId && !mockMounts.delete(mountId)) throw new Error("Mount not found");
        if (!mountId) mockMounts.clear();
        return { removed: 1 };
      }
      case "files/serve/start": {
        // serve_type 白名单与真实 sidecar 对齐：缺省 http，仅 http/webdav。
        const rawType = typeof p.serveType === "string" && p.serveType.trim() ? p.serveType.trim() : "http";
        if (rawType !== "http" && rawType !== "webdav") throw new Error(`unsupported serve type '${rawType}'; only http and webdav are allowed`);
        const serveId = `mock-serve-${++mockServeSeq}`;
        const port = 42000 + (mockServeSeq % 1000);
        const row = { serveType: rawType, url: `http://127.0.0.1:${port}`, connectionId: connectionIdOf(p.connectionId) };
        mockServes.set(serveId, row);
        recordAudit(method, str("path"), p.connectionId);
        return { serveId, url: row.url, serveType: rawType };
      }
      case "files/serve/stop": {
        const serveId = str("serveId");
        const owner = mockServes.get(serveId);
        if (owner && owner.connectionId !== connectionIdOf(p.connectionId)) throw new Error(`serve '${serveId}' does not belong to this connection`);
        // 幂等：未知 serveId 也按成功处理（真实侧 rcd 可能已重启）。
        mockServes.delete(serveId);
        return { success: true };
      }
      case "files/serve/list": {
        const wanted = connectionIdOf(p.connectionId);
        return {
          serves: [...mockServes.entries()]
            .filter(([, row]) => row.connectionId === wanted)
            .map(([serveId, row]) => ({ serveId, url: row.url, serveType: row.serveType })),
        };
      }
      case "files/transfer/cancel": {
        if (typeof p.taskId !== "string") throw new Error("Invalid request parameters: taskId must be a string");
        const jobId = str("taskId");
        const upload = uploads.get(jobId);
        const download = downloads.get(jobId);
        const slot = upload ?? download;
        if (slot) {
          // 镜像 sidecar：取消释放上传槽/停止下载泵，并发出终态事件。
          uploads.delete(jobId);
          if (download) {
            download.canceled = true;
            window.clearTimeout(download.timer);
            downloads.delete(jobId);
          }
          const kind = upload ? "upload" : "download";
          const connectionId = String(slot.connectionId ?? "mock-conn");
          jobs.set(jobId, { ...jobs.get(jobId), taskId: jobId, kind, connectionId, remotePath: slot.path, status: "canceled", totalBytes: slot.size, transferredBytes: slot.received });
          emit("files/transfer/progress", { taskId: jobId, kind, connectionId, remotePath: slot.path, state: "canceled", size: slot.size, transferred: slot.received, total: slot.size });
          return { success: true };
        }
        const cancel = jobs.get(`__cancel_${jobId}`) as unknown as { flag: boolean } | undefined;
        if (cancel) {
          cancel.flag = true;
          for (const timer of timers.get(jobId) ?? []) window.clearTimeout(timer);
          jobs.set(jobId, { ...jobs.get(jobId)!, status: "canceled" });
          emit("files/transfer/progress", { jobId, state: "canceled" });
          return { success: true };
        }
        throw new Error("Transfer task was not found");
      }
      case "files/upload/start": {
        storageFor(p.connectionId);
        const taskId = `mock-upload-${++jobSeq}`;
        uploads.set(taskId, { path: str("remotePath"), size: Number(p.size ?? 0), received: 0, bytes: new Uint8Array(Number(p.size ?? 0)), connectionId: p.connectionId });
        jobs.set(taskId, { taskId, kind: "upload", connectionId: connectionIdOf(p.connectionId), remotePath: str("remotePath"), status: "running", totalBytes: Number(p.size ?? 0), transferredBytes: 0 });
        return { taskId };
      }
      case "files/archiveList": {
        // P2-13④：压缩包内容列表（B-ARCHIVE 夹具收口）。条目契约对齐后端
        // archive.rs::ArchiveEntry：{name, path(归档内相对路径), kind, size}；
        // page/pageSize 可选（缺省 1/200，clamp 1..1000），返回 {entries, total}。
        const path = str("path");
        assertOk(path);
        const archiveTree = treeFor(p.connectionId);
        const targets = archiveSources.get(path.replace(/\/+$/, ""));
        if (!targets) throw new Error(`NotFound: ${path}`);
        const entries: Array<Record<string, unknown>> = [];
        for (const rawSource of targets) {
          const clean = rawSource.replace(/\/+$/, "");
          const root = archiveTree.get(clean);
          if (!root) continue;
          if (root.kind === "file") {
            entries.push({ name: clean.split("/").pop() ?? clean, path: clean.replace(/^\//, ""), kind: "file", size: root.size });
            continue;
          }
          const prefix = `${clean}/`;
          for (const [key, value] of archiveTree) {
            if (!key.startsWith(prefix) || value.kind !== "file") continue;
            entries.push({ name: key.split("/").pop() ?? key, path: key.replace(/^\//, ""), kind: "file", size: value.size });
          }
        }
        entries.sort((a, b) => String(a.path).localeCompare(String(b.path)));
        const total = entries.length;
        const page = Math.max(1, Number(p.page ?? 1));
        const pageSize = Math.min(1000, Math.max(1, Number(p.pageSize ?? 200)));
        return { entries: entries.slice((page - 1) * pageSize, page * pageSize), total };
      }
      case "files/read": {
        // A-FILES ②：预览读取（≤2MiB base64 + truncated）；按连接路由树。
        const path = str("path");
        assertOk(path);
        const source = treeFor(p.connectionId);
        const entry = source.get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "file") throw new Error(`NotFound: ${path}`);
        const all = contentsFor(p.connectionId).get(path.replace(/\/+$/, "")) ?? new Uint8Array(Math.min(entry.size, 64));
        const max = Number(p.maxBytes ?? 2 * 1024 * 1024);
        const data = all.length > max ? all.subarray(0, max) : all;
        return { dataBase64: b64encode(data), truncated: all.length > data.length, size: all.length };
      }
      case "files/readRange": {
        // parity-tools：预览分块读取。参数 { path, offset, length }，length 服务端
        // clamp 到 2MiB；返回 { dataBase64, totalSize, offset, eof }。有内容仓的
        // 文件按真实字节切片，无内容仓的大文件给确定性伪字节（(offset+i)%256），
        // e2e 可校验分块拼接的正确性。
        const path = str("path");
        assertOk(path);
        const source = treeFor(p.connectionId);
        const entry = source.get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "file") throw new Error(`NotFound: ${path}`);
        const totalSize = entry.size;
        const offset = Math.max(0, Number(p.offset ?? 0));
        const length = Math.min(2 * 1024 * 1024, Math.max(0, Number(p.length ?? 0)));
        const end = Math.min(totalSize, offset + length);
        const stored = contentsFor(p.connectionId).get(path.replace(/\/+$/, ""));
        const chunk = stored ? stored.subarray(offset, end) : new Uint8Array(Math.max(0, end - offset));
        if (!stored) {
          for (let i = 0; i < chunk.length; i += 1) chunk[i] = (offset + i) % 256;
        }
        return { dataBase64: b64encode(chunk), totalSize, offset, eof: end >= totalSize };
      }
      case "files/archiveDownload": {
        // parity-tools：目录打包下载。契约与 files/download/start 同形（返回
        // 任务 id），进度经 files/transfer/progress（kind=archiveDownload）推
        // queued→running→completed；取消经 files/transfer/cancel（__cancel_ 槽）。
        const path = str("path");
        assertOk(path);
        const entry = treeFor(p.connectionId).get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "dir") throw new Error(`NotFound: ${path}`);
        const taskId = `mock-archive-${++jobSeq}`;
        const connectionId = connectionIdOf(p.connectionId);
        const cancel = { flag: false };
        jobs.set(`__cancel_${taskId}`, cancel as unknown as Record<string, unknown>);
        jobs.set(taskId, { taskId, kind: "archiveDownload", connectionId, remotePath: path, status: "queued", totalBytes: 0, transferredBytes: 0 });
        emit("files/transfer/progress", { taskId, kind: "archiveDownload", connectionId, remotePath: path, state: "queued", transferred: 0, total: 0 });
        const schedule = (ms: number, fn: () => void) => {
          const timer = window.setTimeout(() => {
            if (cancel.flag) return;
            fn();
          }, ms);
          timers.set(taskId, [...(timers.get(taskId) ?? []), timer]);
        };
        schedule(50, () => {
          jobs.set(taskId, { ...jobs.get(taskId), status: "running", totalBytes: 4096, transferredBytes: 2048 });
          emit("files/transfer/progress", { taskId, kind: "archiveDownload", connectionId, remotePath: path, state: "running", size: 4096, transferred: 2048, total: 4096 });
        });
        schedule(400, () => {
          if (cancel.flag) return;
          jobs.set(taskId, { ...jobs.get(taskId), status: "completed", totalBytes: 4096, transferredBytes: 4096 });
          emit("files/transfer/progress", { taskId, kind: "archiveDownload", connectionId, remotePath: path, state: "completed", size: 4096, transferred: 4096, total: 4096 });
        });
        return { taskId, size: 0 };
      }
      case "files/write": {
        // A-FILES ②：小文件写回（≤4MiB）
        const path = str("path");
        assertOk(path);
        const bytes = b64decode(String(p.dataBase64 ?? ""));
        if (bytes.byteLength > 4 * 1024 * 1024) throw new Error("payload exceeds 4MiB; use the upload slot instead");
        // 按连接路由（P-FILES）：新建文件走本方法，需与 list 的树一致。
        contentsFor(p.connectionId).set(path.replace(/\/+$/, ""), bytes);
        treeFor(p.connectionId).set(path.replace(/\/+$/, ""), { kind: "file", size: bytes.byteLength, modifiedAt: new Date().toISOString() });
        recordAudit(method, path, p.connectionId);
        return { success: true };
      }
      case "files/compress": {
        // P-FILES：压缩（tar/tar.gz 语义模拟）——收集 paths（文件/目录递归）
        // 的伪归档字节；≤10 文件同步返回，否则降级 mock job。
        const paths = Array.isArray(p.paths) ? (p.paths as unknown[]).map(String) : [];
        const target = str("targetPath");
        assertOk(target);
        if (!paths.length) throw new Error("paths must not be empty");
        if (!/\.(tar|tar\.gz|tgz)$/i.test(target)) throw new Error("Archive target must end with .tar, .tar.gz or .tgz");
        if (treeFor(p.connectionId).has(target.replace(/\/+$/, ""))) throw new Error(`Archive target '${target}' already exists`);
        const source = treeFor(p.connectionId);
        const collected: Uint8Array[] = [];
        for (const raw of paths) {
          const path = raw.replace(/\/+$/, "");
          const entry = source.get(path);
          if (!entry) throw new Error(`NotFound: ${path}`);
          if (entry.kind === "file") {
            collected.push(contentsFor(p.connectionId).get(path) ?? new Uint8Array(0));
            continue;
          }
          for (const [key, value] of source) {
            if (key.startsWith(`${path}/`) && value.kind === "file") {
              collected.push(contentsFor(p.connectionId).get(key) ?? new Uint8Array(0));
            }
          }
        }
        if (!collected.length) throw new Error("Nothing to compress: the sources hold no files");
        // 伪归档字节（gzip 魔数前缀区分格式；mock 不消费真实 tar 结构）。
        const gzip = /\.(tar\.gz|tgz)$/i.test(target);
        const bytes = gzip ? concatBytes([new Uint8Array([0x1f, 0x8b]), concatBytes(collected)]) : concatBytes(collected);
        const apply = () => {
          const routed = treeFor(p.connectionId);
          contentsFor(p.connectionId).set(target.replace(/\/+$/, ""), bytes);
          routed.set(target.replace(/\/+$/, ""), { kind: "file", size: bytes.byteLength, modifiedAt: new Date().toISOString() });
          archiveSources.set(target.replace(/\/+$/, ""), paths.map((item) => item.replace(/\/+$/, "")));
          recordAudit(method, target, p.connectionId);
        };
        if (collected.length <= 10 && bytes.byteLength <= 8 * 1024 * 1024) {
          apply();
          return { success: true, transport: "native", jobId: null };
        }
        const jobId = `mock-job-${++jobSeq}`;
        const cancel = { flag: false };
        jobs.set(`__cancel_${jobId}`, cancel as unknown as Record<string, unknown>);
        runJob(jobId, "compress", paths[0], target, apply, cancel, connectionIdOf(p.connectionId));
        return { success: true, transport: "job", jobId };
      }
      case "files/upload/finish": {
        const slot = uploads.get(str("taskId"));
        const job = jobs.get(str("taskId"));
        // sidecar 对已终结的上传 finish 幂等；取消后绝不重新落文件。
        if (!slot && job?.kind === "upload" && ["completed", "failed", "canceled"].includes(String(job.status))) return { success: true };
        if (!slot) throw new Error("Upload task was not found");
        if (slot.received !== slot.size) throw new Error(`Upload is incomplete: ${slot.received}/${slot.size}`);
        try {
          assertOk(slot.path);
        } catch (cause) {
          uploads.delete(str("taskId"));
          const message = cause instanceof Error ? cause.message : String(cause);
          jobs.set(str("taskId"), { ...job, status: "failed", error: message, transferredBytes: slot.received });
          emit("files/transfer/progress", { taskId: str("taskId"), kind: "upload", connectionId: connectionIdOf(slot.connectionId), remotePath: slot.path, state: "failed", error: message, size: slot.size, transferred: slot.received, total: slot.size });
          throw cause;
        }
        // 按 start 记录的 connectionId 落树 + 写内容（此前写死远端树，
        // 双栏本地上传「成功即消失」；P2-13③ 同款收口）。
        const landed = slot.path.replace(/\/+$/, "") || "/";
        contentsFor(slot.connectionId).set(landed, slot.bytes);
        treeFor(slot.connectionId).set(landed, { kind: "file", size: slot.size, modifiedAt: new Date().toISOString() });
        recordAudit("files/upload", slot.path, slot.connectionId);
        jobs.set(str("taskId"), { ...job, status: "completed", transferredBytes: slot.received });
        uploads.delete(str("taskId"));
        return { success: true };
      }
      case "files/local/exists": {
        // 同名预检（ask 策略）：mock 不碰真实文件系统，命中集合由
        // seedMockLocalDownload 注入（集成测试用）。
        const name = str("name");
        const dir = typeof p.dir === "string" && p.dir.trim() ? p.dir.trim() : "/Users/demo/Downloads";
        const path = `${dir.replace(/\/+$/, "")}/${name}`;
        return { exists: localDownloadFiles.has(path), path };
      }
      case "files/download/start": {
        const path = str("remotePath");
        assertOk(path);
        // R5-P2-8：按 connectionId 路由（此前写死远端 tree，双栏本地面下载
        // 报 NotFound——delete/purge/copy/move 等同族方法已收口，唯此漏网）。
        const entry = treeFor(p.connectionId).get(path.replace(/\/+$/, ""));
        if (!entry || entry.kind !== "file") throw new Error(`NotFound: ${path}`);
        const taskId = `mock-download-${++jobSeq}`;
        const size = entry.size;
        // conflict（overwrite）参数在 mock 落盘语义里无操作：mock 不写真实
        // 文件系统，仅校验链路放行。
        const slot = { path, size, received: 0, connectionId: p.connectionId, timer: 0, canceled: false };
        downloads.set(taskId, slot);
        jobs.set(taskId, { taskId, kind: "download", connectionId: connectionIdOf(p.connectionId), remotePath: path, status: "running", totalBytes: size, transferredBytes: 0 });
        // 模拟 sidecar 下载泵：start 返回后异步按 offset 推帧。
        const push = (offset: number) => {
          if (slot.canceled || offset >= size) return;
          const length = Math.min(CHUNK, size - offset);
          const frame = new Uint8Array(8 + length);
          new DataView(frame.buffer).setBigUint64(0, BigInt(offset), false);
          frame.fill((offset / 7) % 251, 8);
          slot.received = offset + length;
          jobs.set(taskId, { ...jobs.get(taskId), transferredBytes: slot.received });
          for (const listener of binaryListeners) listener({ channel: `files/download/${taskId}`, data: frame });
          if (!slot.canceled) slot.timer = window.setTimeout(() => push(offset + length), 1);
        };
        slot.timer = window.setTimeout(() => push(0), 10);
        return { taskId, size };
      }
      case "files/download/finish": {
        const slot = downloads.get(str("taskId"));
        if (slot) {
          slot.canceled = true;
          window.clearTimeout(slot.timer);
          downloads.delete(str("taskId"));
          jobs.set(str("taskId"), { ...jobs.get(str("taskId")), status: "completed", transferredBytes: slot.received });
        }
        return { success: true };
      }
      case "files/ui/state/report": {
        // MCP UI intent 回报（M2，AGENTS.md 硬性规则 7：mock 镜像真实桥形状）。
        // 镜像 sidecar mcp.rs::report 校验——带 intentId 时 status 必须是
        // applied|rejected；无 intentId 为快照型（恒 success，sidecar 覆盖
        // 最新快照）。fixture 不缓存快照（单测经 report 调用形状断言即可）。
        const status = String(p.status ?? "");
        const intentId = typeof p.intentId === "string" ? p.intentId.trim() : "";
        if (intentId && status !== "applied" && status !== "rejected") throw new Error("status must be applied or rejected");
        return { success: true };
      }
      default:
        throw new Error(`Method not found: ${method}`);
    }
  }

  // ---- 组装宿主桥 -----------------------------------------------------------
  // 与 DBX globals.css 的 :root（pearl 浅色）和 .dark 规范块保持一致；
  // 镜像宿主 1.1 theme 通道形状（当前宿主只推 theme，不推 appearance）。
  const light = params.get("theme") === "light";
  const theme: DbxPluginTheme = {
    appearance: light ? "light" : "dark",
    tokens: light
      ? { "--color-background": "rgb(255 255 255)", "--color-foreground": "rgb(10 10 10)", "--color-muted": "rgb(245 245 245)", "--color-muted-foreground": "rgb(115 115 115)", "--color-accent": "rgb(245 245 245)", "--color-accent-foreground": "rgb(23 23 23)", "--color-border": "rgb(229 229 229)", "--color-destructive": "rgb(231 0 11)" }
      : { "--color-background": "rgb(19 20 22)", "--color-foreground": "rgb(215 215 219)", "--color-muted": "rgb(42 42 45)", "--color-muted-foreground": "rgb(151 152 157)", "--color-accent": "rgb(46 47 51)", "--color-accent-foreground": "rgb(221 221 226)", "--color-border": "rgb(110 110 114 / 0.28)", "--color-destructive": "rgb(243 98 95)" },
  };
  let context: Record<string, unknown> = {
    connectionId: "mock-conn",
    connection: { name: "Mock Storage", host: "mock.local", readOnly, protocol: "fs" },
  };
  let currentLocale = params.get("locale") ?? "zh-CN";
  let currentTheme = theme;
  const contextListeners = new Set<(context: Record<string, unknown>) => void>();
  const initListeners = new Set<(context: Record<string, unknown>) => void>();
  window.dbxPlugin = {
    // ready 返回完整 context（真实宿主同形）：?ro=1 的 readOnly 经 connection
   // 透传给 App 的 canWrite 判定（此前 ready 只带 connectionId，ro 注入不生效）。
    ready: Promise.resolve(context),
    get context() { return context; },
    get theme() { return currentTheme; },
    get locale() { return currentLocale; },
    request: async <T>(method: string) => {
      if (method === "host.getContext") return structuredClone(context) as T;
      // 连接枚举：让双栏目标选择与调试壳的连接列表能看到两个内置连接。
      if (method === "host.listConnections") {
        // 与真实宿主对齐：只列用户连接；__local__ 是 sidecar 内置保留连接，
        // 由工作台自己注入左栏（这里再列一次会让左栏下拉出现两个「本地文件」）。
        return [{ id: "mock-conn", name: "Mock Storage" }] as T;
      }
      throw new Error(`Unsupported plugin host method '${method}'`);
    },
    invoke: async <T>(method: string, payload?: Record<string, unknown>) => invoke(method, payload ?? {}) as Promise<T>,
    notify: async () => undefined,
    sendBinary: async (channel: string, data: Uint8Array | ArrayBuffer | string) => {
      const match = channel.match(/^files\/upload\/(.+)$/);
      if (!match) return;
      const slot = uploads.get(match[1]);
      if (!slot) return;
      if (typeof data === "string") return;
      const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
      const offset = Number(new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, false));
      const chunk = bytes.subarray(8);
      slot.bytes.set(chunk, offset);
      slot.received = Math.max(slot.received, offset + chunk.length);
      jobs.set(match[1], { ...jobs.get(match[1]), transferredBytes: slot.received });
    },
    onEvent: (listener) => {
      eventListeners.push(listener);
      return () => {
        const index = eventListeners.indexOf(listener);
        if (index >= 0) eventListeners.splice(index, 1);
      };
    },
    onBinary: (listener) => {
      binaryListeners.push(listener);
      return () => {
        const index = binaryListeners.indexOf(listener);
        if (index >= 0) binaryListeners.splice(index, 1);
      };
    },
    onContext: (listener) => {
      contextListeners.add(listener);
      return () => { contextListeners.delete(listener); };
    },
    onInit: (listener) => {
      initListeners.add(listener);
      listener(context);
      return () => { initListeners.delete(listener); };
    },
    decodeBase64: b64decode,
    encodeBase64: b64encode,
    clipboard: { readText: async () => "", writeText: async () => undefined },
    // 宿主 host.storage mock（Host API 1.2，pluginHostBridge storage 命名空间同形）：
    // 值为任意 JSON，get 未命中返回 null，set(undefined) 归一化为 null。
    capabilities: { storage: true },
    storage: (() => {
      const map = new Map<string, unknown>();
      return {
        get: async (key: string) => (map.has(key) ? structuredClone(map.get(key)) : null),
        set: async (key: string, value: unknown) => {
          map.set(key, value === undefined ? null : structuredClone(value));
          return null;
        },
        delete: async (key: string) => {
          map.delete(key);
          return null;
        },
      };
    })(),
  };

  // 测试驱动器只作为安装结果返回，不向真实 dbxPlugin API 添加方法。
  return {
    setContext(next: Record<string, unknown>) {
      context = structuredClone(next);
      contextListeners.forEach((listener) => listener(context));
      document.dispatchEvent(new CustomEvent("dbx-plugin-context", { detail: context }));
    },
    setEnvironment(next: { locale?: string; theme?: DbxPluginTheme }) {
      if (typeof next.locale === "string") currentLocale = next.locale;
      if (next.theme) currentTheme = next.theme;
      const event: DbxPluginEnvironmentEvent = { type: "env", ...next };
      eventListeners.forEach((listener) => listener(event));
      document.dispatchEvent(new CustomEvent("dbx-plugin-env", { detail: event }));
    },
    emitEvent(method: string, payload: Record<string, unknown>) {
      emit(method, payload);
    },
  };
}

/** 测试/走查注入：按 sidecar `files/ui/intent` 事件形状发一条 intent
 * （mock 与真实 emitter.Event 同面；useUiIntent 消费后回报
 * files/ui/state/report）。mock 未安装时显式报错，不静默吞掉。 */
export function emitUiIntent(message: { intentId: string; action: string; params?: Record<string, unknown> }) {
  if (!emitEventRef) throw new Error("mock host is not installed");
  emitEventRef("files/ui/intent", {
    intentId: message.intentId,
    action: message.action,
    params: message.params ?? {},
  });
}
