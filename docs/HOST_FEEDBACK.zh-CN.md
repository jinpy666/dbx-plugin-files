# Files Studio 插件 → 宿主能力反馈（HOST_FEEDBACK）

> 记录 files 插件在连接表单/工作台集成中发现的宿主能力缺口与改进建议。
> 宿主仓库（t8y2/dbx）不归本仓库管理；本文件是向上游反馈的依据与跟踪点。

## F-1 连接校验（Rust 侧）不评估 `required_when` / `visible_when`

- **现象**：`dbx-core/src/plugins/host.rs` 的
  `validate_plugin_connection_values_for_action` 对静态 `required: true`
  做无条件校验，不读取字段的 `visible_when` / `required_when` 条件。
- **影响**：manifest 无法把协议特定必填项（s3/oss 的 bucket、access_key_id、
  secret_access_key，smb 的 share，opendal-custom 的 service）声明为静态
  required，否则 fs/webdav/ftp/sftp 连接会被
  `Plugin connection field '…' is required` 无条件拦死。插件 v0.1.4 起
  已把这些字段收敛为 `required_when` 成对声明（详见
  `docs/PROGRESS-P-FILES.zh-CN.md` §7），表单级拦截依赖宿主前端
  `pluginFieldIsRequired`，但 Rust 校验层缺少对称的兜底：经 MCP、导入等
  非对话框路径写入的连接不会做条件必填校验。
- **建议**：Rust 校验对带 `visible_when`/`required_when` 的字段按当前
  external_config 值评估条件后再决定 required 检查；或至少跳过不可见字段
  （与前端 `pluginFieldIsVisible` 语义对齐，`lib/plugins/pluginFieldConditions.ts`）。
- **状态**：未修复（2026-09-02 提出）。

## F-2 旧版宿主构建未包含连接对话框修复，动态表单体验依赖随 daily build 发布

- **现象**：插件 manifest 已按协议用 `visible_when` 条件声明字段（宿主
  `plugin-framework-current` 分支的 `PluginConnectionFields.vue` +
  `pluginFieldConditions.ts` 已支持动态显隐/必填）。但用户侧安装的宿主
  构建若早于 §7（2026-08-30）的前端修复，会出现：
  1. `ConnectionDialog.vue` 保存时把 plugin 类型的 `external_config`
     兜底抹成 undefined（落库 null → "Protocol is required"）；
  2. `pluginFieldIsRequired` 把所有插件字段当必填（全字段星号、保存禁用）。
- **影响**：表现为「连接表单不能按协议动态编辑」，实际根因在宿主构建版本，
  插件侧 manifest 条件声明本身是正确且被支持的形态。
- **建议**：确认 §7 修复随宿主 daily build 发布的版本号；在插件文档与
  用户引导中注明最低宿主版本要求（`engines.dbx: ">=0.5.77"` 是否覆盖
  该修复需要核对）。
- **状态**：待宿主发布确认（2026-09-02 提出）。

## F-3 连接写入旁路（MCP / 导入）缺 required 校验与 default 回填

- **现象**：宿主侧经 MCP 或配置导入写入的插件连接记录不经过连接对话框，
  缺少 required 校验与 manifest `default` 回填，可能再次出现
  `external_config` 缺失/不完整的连接记录（PROGRESS §7 遗留提醒中的
  历史脏数据即此类）。
- **建议**：所有连接写入路径统一走
  `validate_plugin_connection_values`（并在 F-1 修复后具备条件校验），
  写入前按 manifest default 回填缺失字段。
- **状态**：未修复（2026-09-02 提出）。
