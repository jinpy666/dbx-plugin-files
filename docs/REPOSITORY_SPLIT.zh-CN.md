# Files 插件独立仓库迁移说明

## 迁移来源与历史

本仓库由 `/Users/Jinpy/btroot/dbx-plugins/files` 拆出，迁移基线为源 monorepo
commit `ce19beb4fa75cc79048e3ba8852c298d674fd9b3`（拆分产物 subtree head 为
`a3802c9fd74e5bc9fa00d862a56a112d4069de80`）。初始导入使用
`git subtree split --prefix=files`，因此保留了 Files 目录相关提交历史。

源仓库当时的 `host` 是用户正在使用的子模块 checkout，本次迁移没有更新、reset、
清理或写入它。README 曾引用的 `docs/screenshots-*` 截图与演示动图在 monorepo
中本就未入库（工作区卫生约定），本次未迁入；独立仓库 README 只引用
`git ls-files` 真实存在的路径。

## 独立仓库内容

- 根目录直接包含 `manifest.json`、`dbx-plugin.toml`、`frontend/`、`backend/`、
  `assets/`、`ui/`、`scripts/`、`docs/` 和 `.github/`。
- `shared/frontend/` 只保留 Files 实际使用的 `binaryEvent`、`themeSync`、
  `uiIntent`、`editorTheme` 及说明；没有迁入 SSH、LDAP 或 Kafka 代码。前端
  导入深度由 monorepo 的三/四级改为独立仓库的两/三级。
- `scripts/connection-forms/verify.mjs` 是连接表单契约的独立校验器，包含与 DBX
  Host 条件语义一致的可见性/必填级联检查及 Files 九协议 × 只读开关的字段
  场景矩阵与七语言（en/zh-CN/zh-TW/es/it/ja/pt-BR）标签断言。
- `shared/sdk/rust/dbx-plugin-sdk/` 是与拆分基线匹配的最小 sidecar SDK vendoring
  （与 ssh 独立仓库同源副本），使 `cargo test` 不再依赖 `../host`。它只包含
  sidecar 协议 SDK，不包含 DBX.app。
- `backend/Cargo.toml` 的 `[patch.crates-io]` 由
  `../../host/plugins/sdk/rust/dbx-plugin-sdk` 改为
  `../shared/sdk/rust/dbx-plugin-sdk`；`dbx-plugin-sdk = "0.1.0"` 的 crates.io
  版本声明行保留（patch 覆盖其来源）。Cargo.lock 中 SDK 仍是 `0.1.0` 路径
  依赖，锁文件内容无变化。

## CI 与发布

- `.github/workflows/ci.yml` 校验 manifest、dbx-plugin.toml、Rust backend identity、
  连接表单，运行前端 typecheck/test/build、`cargo test`、离线 MCP stdio smoke，
  并产出 Linux/macOS/Windows 五个 target 的 candidate `.dbxp`，最后做跨 target
  一致性检查（`scripts/check_candidates.py`）。
- 依赖树判定：`aws-lc-sys` 经 `opendal → reqwest → rustls` 进入依赖树，
  Windows 使用其内置预编译 NASM 回退（`AWS_LC_SYS_PREBUILT_NASM=1`），Linux
  直接安装 `nasm`；本仓库依赖树没有 keyring/dbus，无需 `libdbus-1-dev`。
- `.github/workflows/release.yml` 仅在 `files-v<manifest.version>` GitHub
  Release 上自包含地构建五个 target 并上传 `.dbxp` 与
  `release-candidates.json`。它不内置 signing key、远端仓库或秘密。
- 当前 `manifest.json` 的 `source`/`homepage` 指向
  `https://github.com/jinpy666/dbx-plugin-files`。创建 GitHub 仓库后该字段即为
  生效值；发布时只创建匹配的 `files-v<version>` tag。

## Bug 回写与发布边界

独立仓库中的 bug 先在本仓库修复并通过 CI；若 DBX Host API、宿主安装器或公共
SDK 需要变更，应另开 DBX host/SDK 变更并在 issue/PR 中记录对应版本。不要把本
仓库重新指向 monorepo 的 `../shared`；需要跨插件复用时应发布公共包（前端适配层
与连接表单契约各一）或同步回各独立仓库的明确版本，随后删除 vendored 副本。
SDK 同样应在 crates.io 或 DBX 官方 Git 依赖稳定后，把 `backend/Cargo.toml` 的
path dependency 切换到锁定版本，并保留协议初始化/二进制帧回归测试。

真实存储容器（MinIO / mod_dav / pyftpdlib / OpenSSH / Samba / russh 目标）与
DBX.app host 安装管线不是离线 CI 的通过条件；没有这些环境时
`scripts/container_smoke.sh`、`smoke_mcp.py`/`smoke_test.py` 的 R1-R7 容器段
必须输出 `SKIP`，不能伪报 `PASS`。
