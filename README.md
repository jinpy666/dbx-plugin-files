# Files Studio

[![CI](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml/badge.svg)](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/jinpy666/dbx-plugin-files?display_name=tag)](https://github.com/jinpy666/dbx-plugin-files/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[English](README.en.md) · [产品宣传页](docs/MEDIA.zh-CN.md) · [特性与竞品对比](docs/COMPARISON.zh-CN.md) · [MCP 使用指南](docs/MCP_USAGE.zh-CN.md) · [Files MCP 参考](docs/MCP.zh-CN.md) · [独立仓库迁移说明](docs/REPOSITORY_SPLIT.zh-CN.md) · [发布清单](docs/RELEASE.zh-CN.md)

Files Studio（`io.dbx.files`）是统一的多后端文件工作台：本地文件夹、对象存储和
远程文件服务共用一套浏览、传输和管理体验，把"在多个存储之间来回搬文件"压缩成一个
连贯、可审计、可自动化的工作流。

> Files · Storage · Transfer：fs、S3、OSS、COS、WebDAV、FTP、SFTP、SMB 和传输、归档、自动化
> 集中在一个 DBX 工作台面板中。

![Files Studio 功能演示](docs/media/dbx-files-demo.mp4)

## 为什么值得用

| 你要完成的事 | Files Studio 给你的体验 |
| --- | --- |
| 在本地与远端存储之间迁移数据 | 统一浏览、复制、移动、目录同步，传输任务带进度、取消和历史 |
| 快速检查和整理不同存储里的文件 | 大目录 digest 分页浏览、排序、统计和快捷路径，行为跨协议一致 |
| 处理归档与分享 | zip 压缩/解压/在线浏览，支持签名链接的对象存储可生成 presigned 公开链接 |
| 限制高风险操作 | 只读模式、删除保护、根路径锁定和连接级超时 |
| 把重复工作自动化 | MCP 工具复用连接、策略和权限边界，digest→cursor 翻页、两阶段删除 |

## 适合场景

- 在本地目录、对象存储和远程文件服务之间迁移或同步数据。
- 用统一的操作方式检查、预览和整理不同存储后端中的文件。
- 在受限根路径和只读策略下，为团队提供安全的文件运维入口。

## 核心能力

- 多协议引擎（Apache OpenDAL + 自研适配）：本地文件系统、S3/MinIO、阿里云 OSS、
  腾讯云 COS、WebDAV、FTP、SFTP、SMB/CIFS，以及 gcs/azblob/obs 等其他已编译的 OpenDAL 服务。
- SFTP 双栈：`sftp` 快捷协议（OpenDAL，keyfile 认证）与 `sftp-native`（russh，
  密码或 keyfile）并存；Windows 平台请使用 `sftp-native`。
- 统一浏览、读取、上传、下载、复制、移动、重命名和删除操作。
- 大文件传输支持进度、取消、异步任务和二进制通道；传输历史可按连接查看与清理。
- zip 归档：压缩、解压和在线浏览（archiveList 分页）；支持目录同步（syncDir）、
  对象存储的公开链接（presigned URL）和根路径限制。
- 只读模式、删除保护、Known Hosts 策略和连接级超时控制。
- 连接凭据由 DBX 宿主 secret binding 管理，不写入插件配置。
- 界面支持简体中文、繁体中文、英语、西班牙语、意大利语、日语和葡萄牙语。

完整的定位对比见 [特性与竞品对比](docs/COMPARISON.zh-CN.md)，更多宣传素材见
[产品宣传页](docs/MEDIA.zh-CN.md)。

## MCP 自动化

推荐通过 DBX MCP 桥调用，以复用已保存连接、审批和权限边界。独立 stdio 模式启动：

```bash
backend/target/release/dbx-plugin-files --mcp
```

共 13 个工具：`files_scan_digest`、`files_cursor_next`、`files_ui_focus/search/select/state`、
`files_ui_quick_paths`、`files_write`、`files_mkdir`、`files_rename`、`files_delete`、
`files_purge`、`files_sync`。大目录先 digest 再 cursor 翻页；delete/purge 走两阶段确认；
跨连接目录同步走 `files_sync`（先 dryRun 预演再 sync）。
完整配置、工具调用示例和安全边界见 [MCP 使用指南](docs/MCP_USAGE.zh-CN.md)与
[Files MCP 参考](docs/MCP.zh-CN.md)。

## 安全设计

建议为生产连接启用根路径锁定和只读模式，并谨慎配置删除权限。S3、OSS、COS、WebDAV、FTP、
SFTP 和 SMB 的凭据由宿主 secret binding 管理；插件不会把密钥、私钥或连接导出写入
日志和本地配置。

## 安装

从 [GitHub Releases](https://github.com/jinpy666/dbx-plugin-files/releases) 下载匹配平台的
`.dbxp` 包，在 DBX 插件中心选择本地安装。开发者也可以按照
[迁移与发布说明](docs/REPOSITORY_SPLIT.zh-CN.md) 构建候选包。

## 开发与验证

```bash
pnpm --dir frontend install
pnpm --dir frontend typecheck && pnpm --dir frontend test && pnpm --dir frontend build
cargo test --locked --manifest-path backend/Cargo.toml
python3 scripts/validate_repo.py && node scripts/connection-forms/verify.mjs files
scripts/test.sh
```

仓库契约（manifest/后端身份/连接表单）由 `scripts/validate_repo.py` 与
`scripts/connection-forms/verify.mjs` 校验，CI 在五个 target（linux-x64、linux-arm64、
darwin-arm64、darwin-x64、windows-x64）上构建候选包。协议、后端能力和集成验证说明
位于 `docs/`。

### 本地 Docker 测试环境（可选）

CI 用 `scripts/container_smoke.sh` 起即焚容器做全协议冒烟；本地手工验证可以用
`scripts/docker_env.sh` 起一套持久测试服务端（MinIO / OpenSSH / Samba / mod_dav /
pyftpdlib，凭据运行时随机生成、存在本地状态目录、不进仓库）：

```bash
scripts/docker_env.sh up                      # 初始化并启动（幂等，随 Docker 自动重启）
python3 scripts/docker_env_connect_dbx.py     # 把 docker-* 六条存储连接注入本地 DBX（需先退出 DBX）
scripts/docker_env.sh verify                  # 五协议真实健康检查
scripts/docker_env.sh down [--purge]          # 停止容器；--purge 连凭据/数据一起删
```
