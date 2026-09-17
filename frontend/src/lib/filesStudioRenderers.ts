// Files Studio 按需 renderer 装配层：替代 @file-viewer/preset-all 全量静态引入。
// preset-all 会把 three（3D）、mermaid+katex（图表/公式）、typst、maplibre（地理）、
// libredwg/OCCT（CAD）、EDA、iWork、WordPerfect、CHM、邮件等长尾渲染栈全部打进
// 单文件 bundle；此处只显式引入文件预览真实场景所需的 renderer 子包：
//   - renderer-text        文本/代码（CodeMirror）/markdown/HTML
//   - renderer-pdf         PDF
//   - renderer-image       png/jpg/webp/svg/gif/bmp/tiff/avif/heic
//   - renderer-spreadsheet xlsx/xls/csv/dbf
//   - renderer-word        docx/doc/rtf/odt/odp（open-document 定义）
// rendererMode:"replace" 下未覆盖的格式由 viewer 降级为 unsupported 提示，
// 不影响宿主改走下载等路径。附带三个小体积 capability 增强：
//   - capability-pdf-identity-repair：CJK 身份字体子集 PDF 修复（pdf-lib）
//   - capability-rtf：rtf.js 渲染 RTF 附件
//   - capability-text-tools：diff2html 渲染 .diff/.patch、pako 解压 gzip 文本
import textRenderer from "@file-viewer/renderer-text";
import pdfRenderer from "@file-viewer/renderer-pdf";
import imageRenderer from "@file-viewer/renderer-image";
import spreadsheetRenderer from "@file-viewer/renderer-spreadsheet";
import wordRenderer from "@file-viewer/renderer-word";
import "@file-viewer/capability-pdf-identity-repair";
import "@file-viewer/capability-rtf";
import "@file-viewer/capability-text-tools";

export const filesStudioRenderers = {
  id: "files-studio-renderers",
  label: "Files Studio renderer preset",
  renderers: [textRenderer, pdfRenderer, imageRenderer, spreadsheetRenderer, wordRenderer],
};
