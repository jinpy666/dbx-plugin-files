# 发布清单（files-v*）

从零到一次 Files Studio 插件发布的完整流程。发布自动化由
`.github/workflows/release.yml` 承担：在 GitHub 上创建名为
`files-v<manifest.version>` 的 Release（或先推同名 tag 再建 Release）后，
工作流自动完成校验、五平台构建与产物上传；本清单覆盖人工需要把关的部分。

## 1. 版本号（两处必须一致）

- `manifest.json` 的 `"version"`（发布 tag 即 `files-v<该值>`）
- `backend/Cargo.toml` 的 `version`（`Cargo.lock` 随之更新）

`scripts/validate_repo.py` 会拒绝两处不一致的仓库状态。

## 2. 本地发布前验证（全绿再发）

```bash
python3 scripts/validate_repo.py                    # 仓库契约 + 版本一致性
node scripts/connection-forms/verify.mjs            # 连接表单契约（含防冲突不变式）
pnpm --dir frontend install --frozen-lockfile
pnpm --dir frontend typecheck && pnpm --dir frontend test
pnpm --dir frontend build                           # 产物写入 ui/，必须一并提交
cargo test --locked --manifest-path backend/Cargo.toml   # 含表单×引擎组合矩阵
cargo build --locked --release --manifest-path backend/Cargo.toml
python3 scripts/smoke_mcp.py --binary backend/target/release/dbx-plugin-files
```

可选但推荐（需要 Docker；对 MinIO/OpenSSH/Samba/mod_dav/pyftpdlib 真实容器
跑七协议全链路冒烟）：

```bash
DBX_PLUGIN_SIDECAR="$PWD/backend/target/release/dbx-plugin-files" \
  scripts/container_smoke.sh
```

## 3. 提交与打标

1. 提交全部改动（含 `ui/` 构建产物，CI 有 freshness 门禁）。
2. push 到 `main`，确认 CI 全绿（validate / frontend / backend /
   container-smoke / 五平台 candidate / candidates-check）。
3. 创建 tag `files-v<version>`（例：`files-v0.1.57`）并创建同名 GitHub
   Release（`release.yml` 只响应 `files-v` 前缀）。

## 4. 发布后核验

- Release 页面出现 **5 个平台**的 `*.dbxp` 与 `release-candidates.json`
  （linux-x64 / linux-arm64 / darwin-arm64 / darwin-x64 / windows-x64）。
- `release-candidates.json` 内五平台摘要齐全（由
  `scripts/check_candidates.py --write-release-candidates` 生成并校验）。
- Release 工作流运行记录为绿色；若 build 矩阵有平台失败，产物不会上传
  （publish 依赖 build 全量成功且数量校验）。

## 5. 常见问题

- **tag 建了但工作流没跑**：tag 必须以 `files-v` 开头且与 manifest 版本一致。
- **CI 的 ui/ freshness 门禁失败**：重新执行 `pnpm --dir frontend build`
  并提交 `ui/` 产物。
- **container-smoke 失败**：先本地复现
  `scripts/container_smoke.sh`（`--keep` 保留容器排查），多为测试容器镜像
  上游变更所致，与插件代码无关时可固定镜像 digest。
