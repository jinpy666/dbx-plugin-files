# rclone fork（dbx 分支）：为 DBX Files 插件构建多架构二进制

本文档描述如何 fork rclone、打"保守裁剪"补丁、用 `dbx-plugin-build.yml` 产出
5 个架构的二进制并发 GitHub Release，以及 dbx-plugin-files 侧如何消费。

## 1. fork 与分支策略

- fork `rclone/rclone` 到自己的账号（如 `jinpy666/rclone`）。
- 工作分支命名 `dbx/**`（如 `dbx`），基于上游 release 分支或 `master`。
- 自己的改动收敛为少量小提交，方便定期 `git merge upstream/master` 跟进
  （用 merge 不用 rebase；rclone 约每 6–8 周发一个 minor）。
- **坑：release tag 不在 master 上**（如 v1.75.1 是 `v1.75-stable` 分支头），
  `--default-branch-only` 的 fork 不含该分支；建基分支需
  `git fetch --depth 1 upstream refs/tags/v1.75.1` 后把
  `FETCH_HEAD^{commit}` 推为 fork 的 `dbx` 分支。
- 实际部署：`jinpy666/rclone` 的 `dbx` 分支已挂 `dbx-plugin-build.yml`；
  分支 push / 手动 dispatch 只构建并上传工件（artifacts），**只有推
  `dbx-v*` tag 才会创建 GitHub Release**。

## 2. 改造内容（保守裁剪）

| 改动 | 方式 | 收益 |
| --- | --- | --- |
| 去 selfupdate | 构建时 `-tags noselfupdate` | 插件内置二进制不应自我更新（版本由插件统一管理） |
| 体积优化 | `cross-compile.go` 自带 `-trimpath` + `-ldflags "-s -X ...fs.Version=<ver>"`；可选：在该文件 ldflags 处补 `-w` | 二进制更小 |
| mount 支持 | macOS 构建加 `-tags cmount` + macfuse；Linux/Windows 默认即可 | macOS mount（Windows mount 运行时装 WinFsp，无需编译期改动） |
| `bin/make_rc_docs.sh` | `go install` → `go install -tags "${GOTAGS:-}"`（已打补丁） | `make doc` 的 rcdocs 用 PATH 里的 rclone 挂载生成文档，tagless install 在 macOS/Windows 上没有 mount 命令会失败 |

不做深度裁剪（不删 `backend/all/all.go` 的 backend 列表）：同步上游冲突面最小。

## 3. CI（`dbx-plugin-build.yml`）

把本目录的 `dbx-plugin-build.yml` 复制到 fork 仓库 `.github/workflows/` 下。

- 触发：push `dbx/**`、push tag `dbx-v*`、手动、每周上游检查。
- 三个构建 job + 一个聚合 release job：
  - `linux`（ubuntu-24.04）：amd64 + arm64，CGO=0；
  - `windows`（windows-2022）：amd64，CGO=0；
  - `macos`（macos-15）：装 macfuse 后用官方同款参数交叉出 amd64 + arm64（CGO）；
  - `release`：聚合产物、生成 `SHA256SUMS`，在 `dbx-v*` tag 上发 GitHub Release。
- 依赖工具：`make doc` 需要 pandoc（三个 job 均已安装）+ python3（runner 自带）。
- 版本号约定：`dbx-v<上游版本>-<fork 序号>`，如 `dbx-v1.75.1-1`；tag 名同时是
  `fs.Version` 与产物名的一部分（`rclone-dbx-v1.75.1-1-osx-arm64.zip`）。
- 产物命名与官方完全一致（`rclone-<version>-<os>-<arch>.zip`，darwin→osx），
  由 `bin/cross-compile.go` 自动保证。

## 4. 切换 dbx-plugin-files 到 fork 产物

fork 首个 Release 发布后，更新 dbx-plugin-files 的 `scripts/fetch-rclone.sh`：

1. `RCLONE_VERSION="v1.75.1"` → `"dbx-v1.75.1-1"`；
2. `pinned_sha256()` 中 5 个 zip 的 SHA256 → fork Release `SHA256SUMS` 的值；
3. 下载源：临时切换可直接 `RCLONE_REPO=jinpy666/rclone`（自定义仓库没有
   downloads.rclone.org 回退）；长期切换把脚本内 `RCLONE_REPO` 默认值改为
   fork 仓库。

版本冒烟检查 `rclone $RCLONE_VERSION` 无需改动：fork 的 `fs.Version` 即
`dbx-v1.75.1-1`，`rclone --version` 输出 `rclone dbx-v1.75.1-1` 正好匹配。

## 5. 上游同步流程

1. `check-upstream` 定时任务发现上游新 Release 会自动开 issue。
2. 同步：`git remote add upstream https://github.com/rclone/rclone` →
   `git fetch upstream` → `git merge upstream/master`（解决冲突时优先保上游）。
3. 同步后必跑 `go test` 与 `make quicktest`（有条件再跑 racequicktest），
   通过后打新 tag `dbx-v<上游版本>-<序号+1>` 触发构建。
4. 注意：GitHub 会在仓库 60 天无活动后停用 scheduled workflow，
   `check-upstream` 的 cron 需要仓库保持活动（任何 push/issue 均可）。

## 6. 签名与公证（后续可选）

官方 rclone 的二进制不做 macOS codesign/公证、不做 Windows Authenticode，
只对校验和文件做 GPG 签名；fork 初期对齐官方即可。DBX 宿主以进程方式
spawn sidecar 同目录下的 rclone，应用内安装的解包不携带 quarantine 属性，
Gatekeeper 不拦截。若后续需要签名：

- macOS：在 macos job 里用 Developer ID 证书 `codesign` + `xcrun notarytool`
  （或 goreleaser 的 `notarize.macos`，基于 quill，可对单二进制做公证）；
- Windows：Azure Trusted Signing（`Azure/artifact-signing-action`），
  免证书文件与 EV 硬件 token。

## 7. 运行期依赖（产品侧须知）

- mount 功能：macOS 需用户安装 macFUSE；Windows 需 WinFsp。缺省只影响
  mount，copy/sync/校验等传输功能不受影响。
- macOS 上 fork 二进制为动态链接系统库（CGO），仍可直接分发，无第三方 dylib。
