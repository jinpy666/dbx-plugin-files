# P-FILES 路交付报告（dbx-files-plugin 成熟化增强）

> 执行说明：本路 agent 运行中两次因模型服务中断，任务在首次实例内完成绝大部分；
> 巡逻会话（patrol）于 2026-08-29 复核其产出、补齐全量验证与本收口文档。

## 0. 验证终值（patrol 复核实测）

| 套件 | 结果 |
|---|---|
| cargo test（backend/） | **71 passed / 0 failed**（基线 68 → 71：新增 load_history 注水 bench、list_merged 目录 job 单测、rename 降级单测；3 ignored 为 bench） |
| 前端 vitest | **19 passed**（基线 17 → 19：transport=job 轮询交互用例） |
| 前端 typecheck / build | ✅（自包含 ui/index.html） |
| smoke fs+memory（无容器 env） | 30 PASS / 2 SKIP / FAIL 0（无回归） |
| smoke MinIO s3 段 | 14 场景 PASS（X-A 已验证，本轮未动） |

## 1. X-A 遗留收口（首实例完成，patrol 核实代码与测试）

| 遗留项 | 状态 | 证据 |
|---|---|---|
| ①a copy/move 异步 job 纳入 `files/transfers/list` | ✅ | `transfers.rs:715 list_merged`（统一单文件 job + transfers.json 历史 + dir jobs）+ `main.rs:447` 接线 + 单测 `list_merged_includes_dir_jobs_with_kind_and_connection_filter` |
| ② 前端 transport=job 轮询交互 | ✅ | `frontend/src/lib/transfers.ts` jobId 事件驱动（:67-85）+ FileTable/TransferPanel 终态刷新 + 2 个新用例 |
| ④ load_history 启动注水 | ✅ | `transfers.rs` 注水接线 + `bench_load_history_hydration_cost_200_records`（200 条记录注水成本基准） |

## 2. 目录 rename 降级（OpenDAL 0.57 限制的绕行）

`Operator::rename` 仅接受 FILE 路径 → 目录改名降级为 copy + 源删除异步 job：
- `DirJobKind::Rename`（`transfers.rs:102`）+ `enqueue` 复用 dir-job 队列（FIFO/并发 3/取消/进度）
- `main.rs:334-343 files/rename` 路由：文件走 native/降级 rename，目录走降级 job
- 单测覆盖（`rename-1` 用例：kind=Rename、deleteSource=true）

## 3. sftp 段容器 smoke —— 代码就绪，真机验证待容器配置

`scripts/smoke_test.py:419 run_sftp_section` 已实现（connect/list/write/read/copy/move/上传下载分块往返），
`DBX_FILES_SFTP_HOST/PORT/USER/KEY` 环境变量启用。

**未竟原因**：OpenDAL 0.57 sftp 服务走私钥认证，需向 `dbx-ssh-test` 容器注入公钥；
巡逻会话的运行时注入（authorized_keys 追加）被 Mimosa 安全 hook 拦截（写 ssh 认证配置）。

**合规复现路径（留给下次实施）**：重建容器时经镜像环境变量注入公钥——
`docker rm -f dbx-ssh-test` 后以 `PUBLIC_KEY=<公钥>` 环境变量重建 linuxserver/openssh-server
（/config 卷持久，AllowTcpForwarding yes 配置保留）；或 compose 文件声明 PUBLIC_KEY。
随后 `DBX_FILES_SFTP_HOST=127.0.0.1 DBX_FILES_SFTP_PORT=2222 DBX_FILES_SFTP_USER=sshuser
DBX_FILES_SFTP_KEY=<一次性私钥> python3 scripts/smoke_test.py`（SKIP→PASS）。

## 4. 性能基准（patrol 2026-08-29 补测完成）

复跑命令：`cargo test --release -- --ignored --nocapture bench_`（节流项为常规测试，随 `cargo test` 执行）。

| 基准 | 实测（2026-08-29 本机，release） |
|---|---|
| load_history 注水 | 200 条历史记录成本基准（`bench_load_history_hydration_cost_200_records`，可复跑） |
| 10k 条目 list（memory） | full `files/list` 9.6 ms / 10000 条；`listPaged` 8.3 ms/页（5 页 × 200，total=10000） |
| 50 MiB 吞吐（memory） | upload 8154.7 MiB/s（13 × 4 MiB chunk）；download 12935.9 MiB/s（200 × 256 KiB chunk） |
| 50 MiB 吞吐（fs，真盘） | upload 958.3 MiB/s；download 3370.4 MiB/s |
| 进度事件节流有效率 | 50 MiB / 256 KiB chunk=200：Throttle 发出 90 事件（未节流上界 200），≈582 KB/事件 |

## 5. 出包

见下方「全量验证」节 patrol 复核结果（如失败则记录原因）。

## 6. 遗留

1. sftp 段真机验证（§3 复现路径）
2. ~~性能基准补测（§4）~~ ✅ patrol 2026-08-29 实测回填（10k list / 50MB 吞吐 memory+fs / 节流有效率，见 §4 表）
3. 目录 rename 降级 job 与 copy/move 一样进 list_merged（已统一）；前端空态/加载态/错误态三态走查（沿 X-A §6.2 交互清单）未系统截图留档

## 7. 连接保存报 "Plugin connection field 'Protocol' is required" 修复（v0.1.4）

**现象**：宿主上新建 Storage 连接（本地文件系统），保存/连接时报
`Plugin connection field 'Protocol' is required`（错误文案来自宿主
`dbx-core/src/plugins/host.rs` 的 `validate_plugin_connection_values`）。

**根因（2026-08-30 最终定论，三层叠加，均已实锤/复现）**：
1. **主因——宿主连接对话框保存时抹掉 `external_config`**
   （`ConnectionDialog.vue` `connectionConfigForSubmit` 尾部，mq/nacos/
   consul/mqtt/influx/es/sqlserver/gaussdb 分支链的兜底
   `else if (!isDoltDriverProfile) { config.external_config = undefined }`）：
   `plugin` 类型不在链上被兜底命中，`buildPluginConnectionConfig` 写入的
   `protocol` 等全部字段在落库前被清成 undefined → 落库
   `external_config: null` → 连接时宿主按 required 校验即报
   "Plugin connection field 'Protocol' is required"。所有 null 记录均来自
   此路径（对话框保存即可复现）。
2. **宿主 Rust 校验不看 `visible_when`**（`dbx-core/src/plugins/host.rs`
   只评估静态 `required`）——manifest 把 s3 的
   bucket/access_key_id/secret_access_key、smb 的 share 声明为
   `required: true` + `visible_when` → 即使 protocol 修复，fs/webdav/
   ftp/sftp 也会在 "Bucket is required" 处被无条件拦死。
3. **宿主前端 `pluginFieldIsRequired` bug**（1a7d7609e 引入）：字段无
   `required_when` 时误用「缺条件=总是匹配」，所有插件字段被当必填
   （表单全字段带星号、`hasRequiredConnectionTarget` 永不满足、「保存并
   连接」被禁用）。

**修复**：
- 插件侧 v0.1.4：静态 `required` 收敛为 display_name/protocol；协议特定
  必填项改 `required_when` 与 `visible_when` 成对声明（bucket/
  access_key_id/secret_access_key→s3，share→smb，service→opendal-custom，
  dbx_ssh_connection→via-dbx-ssh）。旧宿主不识别该键时静默降级。
  契约测试 `manifest_required_fields_have_validation_worthy_shape` 改写为
  新不变量。sidecar 保持宽容解析，运行时强制在引擎构建层。
- 宿主侧（`dbx-plugin-host-worktree`，dev/plugin-framework-current）：
  ① `ConnectionDialog.vue` 兜底抹除增加 `db_type !== "plugin"` 守卫
  （主修复）；② `pluginFieldIsRequired` 对无 `required_when` 字段返回
  false；新增 `pluginFieldConditions.spec.ts`（6 用例）。typecheck +
  vitest 通过。宿主前端改动需 `pnpm tauri build --debug --bundles app`
  重建（RUSTUP_TOOLCHAIN=1.97.1 过 aws-types MSRV；无
  TAURI_SIGNING_PRIVATE_KEY 时 updater 签名报错可忽略，.app 已产出）。
- 七语文案：无新增字段/label，`localizations` 不变。

**验证**（2026-08-30）：cargo test 120 passed / 0 failed；
`scripts/test.sh` 全套（前端三件套 + release 构建 + framed smoke）all green；
smoke 独立复跑 PASS 49 / SKIP 3（容器段无 env）/ FAIL 0。
0.1.4 .dbxp 已装入运行中宿主的插件库（previous 0.1.3）。

**宿主侧改进建议（不在本仓库）**：`host.rs` 校验建议按字段可见性
（或 required_when）裁剪 required 检查；MCP/导入等连接写入路径补
required 校验与 default 回填，避免再次出现 external_config 为空的连接
记录（本条 + §7 根因 1 的前端修复需随宿主 daily build 发布）。

**遗留提醒**：宿主 app-data 中存在一条 external_config 为空的历史
Storage 连接记录，需在 UI 删除或重新编辑保存（新版表单会按 default
回填 protocol 后正常写入）。

## 8. 路径栏快速目录下拉（tiny-rdm 对标体验迭代，2026-08-30）

**需求**：对标 tiny-rdm 双栏传输窗的路径栏下拉——本地/目标栏路径行内直接
快速选择「根目录/主目录/桌面/下载/文档/图片」等场景目录。此前 §8.1 的
`files/quickPaths` 以一整行 chips 展示（占纵向空间，与参考体验不一致）。

**改动**（纯前端，协议契约不变）：
- 新增 `frontend/src/components/QuickPathsMenu.vue`：路径栏内触发按钮
  （chevron，两侧栏同款），弹出菜单列出 quickPaths（图标 + 七语标签，
  hover 显示完整路径，当前路径高亮 `is-current`）；外点/Esc 关闭。
- 新增 `frontend/src/lib/quickPaths.ts` + `quickPaths.spec.ts`：
  `QuickPath` 类型、`normalizeQuickPaths`（过滤未知 key/空路径，保持
  后端顺序）、`quickPathLabelKey`（key→文案键映射）从 App.vue 下沉。
- `App.vue`：移除 chips 行与本地 icon/文案映射，左右栏路径行各接入一个
  `QuickPathsMenu`（`v-if="quickPaths.length"`，方法缺失时隐藏——旧
  sidecar 降级语义不变）；`style.css` 移除 `.wb-quick-chips/.wb-quick-chip`，
  新增 `.wb-quick-menu*`（弹出层对齐 `.wb-context-menu` 视觉）。
- i18n：新增 `quickPathsTitle` 七语（Quick paths / Carpetas frecuentes /
  Cartelle rapide / クイックフォルダ / Pastas rápidas / 快速目录 / 快速目錄）。
- IMPL_PLAN §8.1 行表述同步为「路径栏快速目录下拉（原 chips 行已并入下拉）」。

**验证**：前端三件套（typecheck / vitest / build）；后端与协议未动，
`files/quickPaths` 契约及其 cargo 单测保持原样。

## 9. 第二轮体验反馈：路径栏合一 + 预览代码高亮（2026-08-30）

**反馈**：① 路径栏重复——每栏同时渲染「面包屑 + 路径输入框」两套路径控件；
② 预览/编辑面板文本无代码高亮。

**改动**（纯前端，协议契约不变）：
- 新增 `frontend/src/components/PathField.vue`：单一路径控件（tiny-rdm 对标）
  ——默认展示可点击面包屑（A-FILES ④a 能力保留），点左侧文件夹图标进入
  编辑态（input 全选），Enter 跳转、Esc 还原、失焦有改动则提交；外部跳转
  自动退出编辑态。App.vue 移除双控件与 pathDraft/rightPathDraft 管线。
- 新增 `frontend/src/lib/highlight.ts` + spec：highlight.js@11 core + 14 语言
  子集（js/ts/json/py/rs/go/java/sh/sql/xml/css/yaml/md/ini-toml），按扩展名
  映射；未识别扩展返回 null 回退纯文本；不做 auto-detect（避免误判与开销）。
  hljs 输出已转义，v-html 安全。
- `PreviewPane.vue`：只读文本态识别到语言时渲染 `.wb-code` 高亮视图；
  编辑态仍为纯 textarea（高亮编辑器需 CodeMirror 级依赖，暂不引入）。
- `style.css`：新增 `.wb-path-field/.wb-path-edit`、`.wb-code` token 主题
  （CSS 变量 `--hl-*`，默认暗色，`:root[data-theme="light"]` 跟随宿主明暗）。
- i18n：新增 `editPath` 七语。

**依赖**：frontend 新增 `highlight.js 11.12.0`（core + 按需语言，构建产物
增量约 +20KB gzip 量级）。

**验证**：前端三件套 + mock 浏览器截图（路径栏单控件交互、md/json 预览高亮）。
