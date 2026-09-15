# Files Studio 产品宣传素材

本页集中放置可直接用于 GitHub、发布说明和产品介绍的素材。全部为文字内容，
与插件当前真实能力一致；截图与演示视频待后续版本补充，本页不引用任何不存在的媒体文件。

## 一句话定位

`Files Studio：把本地文件夹、S3 对象存储和远程文件服务放进同一个文件工作台。`

## 长版文案

`面向日常文件运维与跨存储迁移的 Files Studio，打开一个连接即可完成浏览、上传下载、
复制移动、传输任务、zip 归档和受控删除。多协议引擎覆盖本地 fs、S3/MinIO、阿里云 OSS、
WebDAV、FTP、SFTP（双栈：keyfile 与密码认证）和 SMB/CIFS，凭据由宿主 secret binding
管理，并提供面向自动化的 MCP 工具接口与七语界面。`

## 三句卖点

- **从一个连接到多种存储**：fs、S3、WebDAV、FTP、SFTP、SMB 共用同一套浏览与传输
  体验，换存储不换操作习惯。
- **从能用到敢用**：根路径锁定、只读模式、删除保护和宿主 secret binding 把高风险
  操作放进明确的权限边界。
- **从手动到自动化**：MCP 工具复用已保存连接与策略，digest→cursor 分页和两阶段
  删除让脚本化运维同样安全。

## 能力速览

- 浏览与管理：列表/分页、排序、统计、stat、size、quickPaths。
- 传输：上传、下载、复制、移动、目录同步（syncDir）、进度、取消与历史记录。
- 归档：zip 压缩、解压、在线分页浏览（archiveList）。
- 分享与链接：S3 对象 presigned 公开链接（publicLink）。
- 安全：read_only、allow_delete、lock_to_root、known_hosts 策略、连接级超时。
- 自动化：12 个 MCP 工具，stdio 独立模式与 DBX MCP 桥双通道。
- 平台：linux-x64、linux-arm64、darwin-arm64、darwin-x64、windows-x64 五平台候选包。

## 使用边界

本页描述的是插件 UI 与离线可验证的能力。真实 S3/WebDAV/FTP/SFTP/SMB 后端、
DBX.app 宿主桥和平台安装流程需要对应运行环境；请以 CI、smoke（容器类用例按环境
SKIP）和发布说明中的实际验证结果为准。Windows 平台的 `sftp` 快捷协议不可用，
请使用 `sftp-native`。
