# shared/frontend — 插件前端宿主适配公共层（vendored）

本目录是从 DBX 插件 monorepo `shared/frontend/` 拆分时 vendored 进来的子集，
只保留 Files 实际使用的模块。宿主桥/宿主特性的前端适配代码在独立仓库内以
相对路径引用，不再依赖 `../shared` 或兄弟插件 checkout。

## 引用方式

前端源码相对引用（Vite / vue-tsc 均无需额外配置，tsconfig 随 import
自动纳入检查）：

```ts
// 位于 frontend/src/App.vue：
import { bridgeBinaryBytes } from "../../shared/frontend/binaryEvent";
// 位于 frontend/src/lib/*.spec.ts：
import { bridgeBinaryBytes } from "../../../shared/frontend/binaryEvent";
```

## 约定

- 每个模块保留薄 spec 引用 shared 模块跑断言（证明本插件工具链下 import
  解析、打包、行为都成立）。
- 不要把本目录改回指向 monorepo 的 `../shared`；跨插件复用的演进路径是
  发布版本化公共包后切换依赖（见 `docs/REPOSITORY_SPLIT.zh-CN.md`）。
- 宿主桥契约若发生变更，在本仓库单点修改本目录并跑通薄 spec 与真机复验；
  修复同样回写 monorepo 公共层，避免两侧漂移。

## 现有模块

| 模块 | 用途 | Files 接入点 |
| --- | --- | --- |
| `binaryEvent.ts` | 宿主桥 binary 事件双形状归一化（`data: Uint8Array` / 旧 `dataBase64`） | `App.vue`（files/download 分块） |
| `uiIntent.ts` | MCP UI intent 通道公共 composable（`useUiIntent(domain, handlers)`）：订阅 `files/ui/intent` 事件（形状归一化）→ 分派 handler → 自动调 `files/ui/state/report` 回报 applied/rejected/summary；`reportSnapshot` 上报无 intentId 的快照型 report | `App.vue`（MCP 面板 digest/cursor 快速定位） |
| `editorTheme.ts` | CodeMirror 6 语法高亮调色板（`EDITOR_TOKEN_COLORS` / `syntaxTokenSpecs` / `dbxSyntaxHighlight`）：暗色为提亮后的 GitHub Dark 系，浅色 VS Code Light+ 同源；**零运行时依赖**，codemirror 系对象由调用方注入 | `components/TextPreview.vue`（追加在 basicSetup 后） |
| `themeSync.ts` | 宿主主题令牌 → 插件 CSS 变量桥（`themeBridgeCss` / `installHostThemeBridge`）：首绘即命中宿主主题、主题变化经 SDK 令牌更新自动跟随；宿主无令牌时回退暗色规范值（optional 降级），并下发语义状态色与模态遮罩令牌 | `main.ts`（挂载前 `installHostThemeBridge()`） |
