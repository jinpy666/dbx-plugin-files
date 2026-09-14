# DBX Files

[![CI](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml/badge.svg)](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/jinpy666/dbx-plugin-files?display_name=tag)](https://github.com/jinpy666/dbx-plugin-files/releases)

[English](README.en.md) · [独立仓库迁移说明](docs/REPOSITORY_SPLIT.zh-CN.md) · [Files MCP 参考](docs/MCP.zh-CN.md)

DBX Files（`io.dbx.files`）是统一的多后端文件工作台。它让本地目录、对象存储和
远程文件服务使用一致的浏览、传输和管理体验，适合日常文件运维、跨存储迁移和
受控共享。

## 适合场景

- 在本地目录、对象存储和远程文件服务之间迁移数据。
- 用统一的操作方式检查、预览和整理不同存储后端中的文件。
- 在受限根路径和只读策略下，为团队提供安全的文件运维入口。

## 核心能力

- 支持本地文件系统、S3/MinIO、阿里云 OSS、WebDAV、FTP、SFTP、SMB/CIFS 以及
  其他已编译的 OpenDAL 服务。
- 统一浏览、读取、上传、下载、复制、移动、重命名和删除操作。
- 大文件传输支持进度、取消、异步任务和二进制通道。
- 支持目录同步、公开链接（presigned URL）和根路径限制。
- 支持只读模式、删除保护、Known Hosts 策略和连接级超时控制。
- 连接凭据由 DBX 宿主 secret binding 管理，不写入插件配置。
- 界面支持简体中文、繁体中文、英语、西班牙语、意大利语、日语和葡萄牙语。

## MCP 自动化

独立 stdio 模式启动：

```bash
backend/target/release/dbx-plugin-files --mcp
```

常用工具包括 `files_scan_digest`、`files_cursor_next`、`files_write`、
`files_mkdir`、`files_rename` 和 `files_delete`。大目录先 digest 再 cursor，
删除和 purge 需要两阶段确认。完整工具契约见 [Files MCP 参考](docs/MCP.zh-CN.md)。

## 安全设计

建议为生产连接启用根路径锁定和只读模式，并谨慎配置删除权限。S3、OSS、
WebDAV、FTP、SFTP 和 SMB 的凭据由宿主安全存储管理；插件不会把密钥、私钥或
连接导出写入日志和本地配置。

## 开发与验证

```bash
pnpm --dir frontend install
pnpm --dir frontend typecheck && pnpm --dir frontend test && pnpm --dir frontend build
cargo test --manifest-path backend/Cargo.toml
scripts/test.sh
```

仓库契约（manifest/后端身份/连接表单）由 `scripts/validate_repo.py` 与
`scripts/connection-forms/verify.mjs` 校验，CI 在五个 target 上构建候选包。
协议、后端能力和集成验证说明位于 `docs/`。
