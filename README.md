# dbx-files-plugin（io.dbx.files）

DBX 的多协议文件管理插件：统一 **Apache OpenDAL** 引擎访问
fs/s3(MinIO)/webdav/ftp/sftp + 全部已编译服务（`opendal-custom` 透传）；
浏览/读写/复制/移动/删除、二进制通道大文件传输（异步 job + 进度 + 取消）、
目录同步、公开链接（presign）。能力覆盖 Fs 22 方法，
能力缺口显式声明。

## 状态

**待实施**（v3 方案定稿：弃 rclone+Go，改 OpenDAL + Rust）。里程碑：
M0（公共基线）→ M2（核心）→ M3（sftp + 云盘模板）→ M4（MCP 工具）。

## 技术形态

- sidecar：**Rust**（dbx-plugin-sdk，`stdio-framed` + **二进制通道**，
  与 ssh-sftp 插件同栈；上传/下载复用 `sftp/upload|download/*` 帧语义）
- 引擎：`opendal` crate 0.58.x（Apache-2.0），feature 白名单
  fs/s3/webdav/ftp/sftp/gcs/azblob/oss/memory
- 凭据：Builder 全内存（无 obscure 层/无配置文件/不进环境变量）；
  secret 由宿主 binding 管理
- SDK 引用：照 ssh-sftp `backend/Cargo.toml` 的 `[patch.crates-io]`
  模式指向 `~/btroot/dbx-plugin-host-worktree` 的 Rust SDK

## 文档

- 实施文档（唯一工作来源）：[docs/IMPL_PLAN_DBX_FILES.zh-CN.md](docs/IMPL_PLAN_DBX_FILES.zh-CN.md)
  ——v3 决策依据、迁移映射、manifest 字段表、方法契约（含二进制传输槽）、
  异步 job 设计、能力缺口表 §5.5、任务表 F2/F3/F4
- 公共基线：`../shared/IMPL_PLAN_M0_COMMON.zh-CN.md`（注意：其 Go SDK
  部分仅适用 LDAP；本插件走 Rust 栈，验证并入 F2-1）

## 脚手架入口

M0 期间先做 F2-0/F2-1：Cargo 骨架 + opendal feature 白名单 + manifest +
framed sidecar 装配（参照 ssh-sftp `backend/src/main.rs` 模式），
安装联调对测试宿主（`~/btroot/dbx-plugin-host-worktree` 构建产物）。
