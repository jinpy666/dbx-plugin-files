// Files Studio 按需 renderer 装配层：替代 @file-viewer/preset-all 全量静态引入。
// preset-all 会把 three（3D）、mermaid+katex（图表/公式）、typst、maplibre（地理）、
// libredwg/OCCT（CAD）、EDA、iWork、WordPerfect、CHM、邮件等长尾渲染栈全部打进
// 单文件 bundle；此处只显式引入文件预览真实场景所需的 renderer 子包：
//   - renderer-text                 文本/代码（CodeMirror）/markdown/HTML/ipynb
//   - renderer-pdf                  PDF
//   - renderer-image                png/jpg/webp/svg/gif/bmp/tiff/avif/heic/jxl
//   - renderer-spreadsheet          xlsx/xls/csv/tsv/dbf/ods 等表格
//   - renderer-word                 docx/doc/rtf/odt/odp（open-document 定义）
//   - renderer-media                音频（wav/mp3/flac/ogg/aac/opus/m4a/midi）与视频（mp4/webm/mov/mkv 等）
//   - renderer-presentation/pptx    pptx/pptm/potx/potm/ppsx/ppsm（OOXML 演示文稿）
//   - renderer-ofd                  ofd（GB/T 33190 电子公文）
//   - renderer-mindmap              xmind 思维导图
// 注意：传统二进制 .ppt 的 @file-viewer/ppt 链路自带约 18MB 的 CJK 字体 OTF + WASM，
// 而本项目构建（build.mjs 的 inlineDynamicImports）会把动态导入全部内联进单文件，
// 故只装配 OOXML 的 ./pptx 子入口；.ppt 由 viewer 降级为「外部打开」提示。
// rendererMode:"replace" 下未覆盖的格式由 viewer 降级为 unsupported 提示，
// 不影响宿主改走下载等路径。附带四个小体积 capability 增强：
//   - capability-pdf-identity-repair：CJK 身份字体子集 PDF 修复（pdf-lib）
//   - capability-rtf：rtf.js 渲染 RTF 附件
//   - capability-text-tools：diff2html 渲染 .diff/.patch、pako 解压 gzip 文本
//   - capability-midi：@tonejs/midi 检查 .mid/.midi 乐谱（顶层自动激活 loader）
import textRenderer from "@file-viewer/renderer-text";
import pdfRenderer from "@file-viewer/renderer-pdf";
import imageRenderer from "@file-viewer/renderer-image";
import spreadsheetRenderer from "@file-viewer/renderer-spreadsheet";
import wordRenderer from "@file-viewer/renderer-word";
import mediaRenderer from "@file-viewer/renderer-media";
import ofdRenderer from "@file-viewer/renderer-ofd";
import mindmapRenderer from "@file-viewer/renderer-mindmap";
// renderer-pptx 的 default export 是裸 renderPptx 函数；preset 需要
// { id, definitions, handlers } 装配对象，必须用具名导出 pptxRenderer。
import { pptxRenderer } from "@file-viewer/renderer-presentation/pptx";
import "@file-viewer/capability-pdf-identity-repair";
import "@file-viewer/capability-rtf";
import "@file-viewer/capability-text-tools";
import "@file-viewer/capability-midi";

export const filesStudioRenderers = {
  id: "files-studio-renderers",
  label: "Files Studio renderer preset",
  renderers: [textRenderer, pdfRenderer, imageRenderer, spreadsheetRenderer, wordRenderer, mediaRenderer, pptxRenderer, ofdRenderer, mindmapRenderer],
};
