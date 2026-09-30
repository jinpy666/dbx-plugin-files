# Files Studio

[![CI](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml/badge.svg)](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/jinpy666/dbx-plugin-files?display_name=tag)](https://github.com/jinpy666/dbx-plugin-files/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[English](README.en.md) · [产品宣传页](docs/MEDIA.zh-CN.md) · [特性与竞品对比](docs/COMPARISON.zh-CN.md) · [MCP 使用指南](docs/MCP_USAGE.zh-CN.md) · [Files MCP 参考](docs/MCP.zh-CN.md) · [独立仓库迁移说明](docs/REPOSITORY_SPLIT.zh-CN.md) · [发布清单](docs/RELEASE.zh-CN.md)

**把本地磁盘和几十种远程存储装进同一个 DBX 面板。** Files Studio（`io.dbx.files`）
是面向日常文件运维与跨存储迁移的文件工作台：双栏对照浏览、跨栏拖拽传输、目录同步、
zip 归档和 MCP 自动化共用一套体验——换存储，不换操作习惯。

> Files · Storage · Transfer：fs、S3、OSS、COS、WebDAV、FTP、SFTP、SMB——
> 浏览、传输、归档、自动化，集中在一个工作台面板。

![Files Studio 功能演示](docs/media/dbx-files-demo.mp4)

**85 种存储后端 · 13 个 MCP 工具 · 7 种界面语言 · 5 个发布平台**

## 为什么值得用

| 你要完成的事 | Files Studio 给你的体验 |
| --- | --- |
| 像 FileZilla 一样对照搬运 | 双栏本地 ↔ 远端并排，跨栏复制、移动、拖拽自带冲突预检与覆盖确认 |
| 在本地与远端存储之间迁移数据 | 统一浏览、复制、移动、目录同步，传输任务带进度、取消和历史 |
| 快速检查和整理不同存储里的文件 | 大目录 digest 分页浏览、排序、统计和快捷路径，行为跨协议一致 |
| 处理归档与分享 | zip 压缩/解压/在线浏览，支持签名链接的对象存储可生成 presigned 公开链接 |
| 限制高风险操作 | 只读模式、删除保护、根路径锁定和连接级超时 |
| 把重复工作自动化 | 13 个 MCP 工具复用连接、策略和权限边界，digest→cursor 翻页、两阶段删除 |

## 支持的后端（85 种）

由托管 rclone rcd 引擎驱动，一份连接表单通吃：32 个主流后端有专属表单，
其余 53 个通用后端以通用字段加 JSON 选项接入（未知选项会被拒绝并列出有效名称）。

| 类别 | 后端 |
| --- | --- |
| **本地与网络文件系统（8）** | 本地磁盘（fs）、SMB / CIFS、WebDAV、FTP、SFTP（OpenDAL）、SFTP 原生（russh，支持密码）、HTTP、HDFS |
| **对象存储（31）** | AWS S3 / MinIO、Google Cloud Storage、Azure Blob、Azure Files、阿里云 OSS、腾讯云 COS、华为云 OBS、七牛云 Kodo、Cloudflare R2、Backblaze B2、Wasabi、DigitalOcean Spaces、Scaleway、IDrive e2、UCloud US3、移动云 EOS、网易数帆 NOS、百度智能云 BOS、火山引擎 TOS、金山云 KS3、Oracle 对象存储、QingStor、OpenStack Swift、Storj、Tardigrade、Sia、Internet Archive、Akamai NetStorage、ImageKit、Cloudinary、DOI |
| **国际网盘（23）** | Google Drive、Google 相册（只读）、Dropbox、OneDrive、iCloud Drive、Box、pCloud、Mega、Proton Drive、Yandex Disk、Koofr、Seafile、Jottacloud、Mail.ru Cloud、HiDrive、Filen、Internxt、OpenDrive、SugarSync、Zoho WorkDrive、premiumize.me、Put.io、Drime |
| **国内网盘（2）** | 阿里云盘（资源盘 / 共享盘 / 备份盘）、华为云盘 |
| **文件传输与网盘中转（8）** | PikPak、Files.com、1Fichier、FileLu、Gofile、Linkbox、Pixeldrain、Uloz.to |
| **企业与专用服务（4）** | Enterprise File Fabric、Citrix ShareFile、Maytech Quatrix、Shade |
| **组合与工具后端（8）** | Alias、Union、Combine、Archive、Chunker、Crypt（客户端加密）、Compress、Hasher |
| **测试后端（1）** | Memory（内存盘，rclone 测试用，数据不落盘） |

> SFTP 双栈并存：`sftp` 快捷协议（OpenDAL，keyfile 认证）与 `sftp-native`
> （russh，密码或 keyfile）；Windows 平台请使用 `sftp-native`。FTP 支持非 UTF-8
> 旧编码文件名的显示转换（GBK / Big5 / Shift-JIS 等）。S3 兼容厂商的专属端点
> 格式已内置提示，R2 / Wasabi / Spaces 等照抄即可连上。

## 双栏工作台

- 左栏默认本地文件系统，右栏保持当前远端连接；单栏/双栏随点随切，不打断当前浏览。
- 跨栏复制、移动与拖拽传输；左栏切到另一条已保存连接时直接跨连接对拷，
  连接失活会自动回退并按需自愈重连，长任务不因一次切换中断。
- 每栏独立排序与浏览状态，跨协议行为一致。

## 核心能力

- 85 个后端选项一个引擎打尽（完整清单见上表）：本地文件系统、S3/MinIO、
  阿里云 OSS、腾讯云 COS、华为云 OBS、Cloudflare R2、WebDAV、FTP、SFTP、
  SMB/CIFS 等 32 个主流后端有专属表单，其余 53 个通用 rclone 后端以
  通用字段 + JSON 选项接入，未知选项会被拒绝并列出有效名称。
- 统一浏览、读取、上传、下载、复制、移动、重命名和删除操作。
- 大文件传输支持进度、取消、异步任务和二进制通道；传输历史可按连接查看与清理。
- zip 归档：压缩、解压和在线浏览（archiveList 分页）；支持目录同步（syncDir）、
  对象存储的公开链接（presigned URL）和根路径限制。
- 安全边界内建在连接里：只读模式、删除保护、Known Hosts 策略和连接级超时；
  凭据由 DBX 宿主 secret binding 管理，不写入插件配置或日志。
- 界面支持简体中文、繁体中文、英语、西班牙语、意大利语、日语和葡萄牙语，
  明暗主题跟随宿主。

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
