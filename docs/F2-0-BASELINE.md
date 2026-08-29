# F2-0 基线：Cargo 骨架 / 体积 / 许可清单

> 任务来源：`IMPL_PLAN_DBX_FILES.zh-CN.md` §11 F2-0（Cargo 骨架 + opendal
> feature 白名单 + 体积基线 + 许可清单）。记录日期：2026-08-28。

## 1. 工具链与依赖决策

| 项 | 值 | 说明 |
|---|---|---|
| rustc / cargo | **1.88.0**（固定，不升级） | 宿主 rust-version 要求，同 ssh-sftp |
| `opendal` | **0.57.0**（crates.io 最新锁定） | **0.58.x 无法使用**：`opendal 0.58.2` 的 `rust-version = 1.91`（`cargo info` 实测），高于宿主固定的 1.88.0；0.57 的 rust-version 为 1.85。升级 0.58 需与宿主工具链升级联动，走对标清单回归 |
| `dbx-plugin-sdk` | 0.1.0 + `[patch.crates-io]` path | 照 ssh-sftp 同款，指向 `../../../dbx-plugin-host-worktree/plugins/sdk/rust/dbx-plugin-sdk` |
| tokio / serde / serde_json / uuid | full / derive / v4 | 与 SDK 对齐 |
| dev-dependencies | tempfile | store 单测 |

opendal feature 白名单（`backend/Cargo.toml`）：
`services-fs, services-s3, services-webdav, services-ftp, services-sftp,
services-gcs, services-azblob, services-oss, services-memory`。

> 注：0.57 已拆分为 facade crate（opendal-core + 各 service crate），
> `Operator::via_iter(scheme, kv)` / `check()` / `info().full_capability()`
> API 与实施文档 §5 假设一致；`remove_all` 已废弃，递归删除用
> `delete_with(path).recursive(true)`（已写入 F-B 契约注释）。

## 2. 体积基线（darwin-arm64，2026-08-28）

| 产物 | 体积 |
|---|---|
| `backend/target/release/dbx-plugin-files` | **20,726,496 B（≈19.8 MiB）** |
| `backend/target/debug/dbx-plugin-files` | 56,203,584 B（≈53.6 MiB，仅开发参照） |

release 体积落在实施文档 §0 预估的 10-20MB 区间上沿（feature 白名单含
s3/gcs/azblob/oss 全套 TLS+HTTP 栈）。后续如需压缩：
`[profile.release] lto = "thin"` / `strip = "symbols"` / `codegen-units = 1`
可再缩（未启用——与 ssh-sftp 保持同款默认 profile）。

复现：

```
cd backend && cargo build --release
ls -l target/release/dbx-plugin-files
```

## 3. 许可（M2-T0 归档用）

- 本插件：Apache-2.0（`backend/Cargo.toml` `license` 字段）。
- opendal 0.57.0：Apache-2.0；dbx-plugin-sdk：宿主仓随源。
- 依赖全量清单：`backend/Cargo.lock`（直接依赖 819 个树节点 / 379 个唯一
  crate，含 feature 展开的对齐记录）。归档时执行
  `cargo tree --prefix none > docs/F2-0-DEPS.txt` 即可快照。

## 4. F2-0 验证记录

| 检查 | 结果 |
|---|---|
| `cargo build --release` | 通过（Finished release profile） |
| `cargo test`（31 个单测：lifecycle/model/engine(memory)/store/transfers/main 门禁） | 31 passed, 0 failed |
| sidecar framed 冒烟（`scripts/smoke_test.py`，release 二进制） | initialize/connect/capabilities/purge 拒根/read-only 门禁全部 ok；未实现方法（F-B/F-C 桩）按约定 SKIP；遗留 1 个 harness 侧 FAIL（见 §5） |

## 5. 已知遗留（交接 F-B/F-C 与 scripts 负责人）

1. **smoke harness 连接 id 不一致**（`scripts/smoke_test.py`，非本波文件）：
   `run_core_sections` 连接 `smoke-<section>-readonly`，而
   `scenario_read_only` 用 `<section>-readonly` 调用 →
   `fs/read-only-list-ok` 报 `Unknown connectionId`。修法一行：
   `runner.connection_id = f"smoke-{section}-readonly"`。
2. F-B/F-C 桩方法（ops/transfers）被调用时返回
   `Method not found: <method> is not implemented yet (sidecar panic: …)`
   （main.rs 的 panic 安全网把 `todo!()` 转成业务错误，避免宿主挂死）；
   smoke 依赖该措辞保持 SKIP 语义。
3. `connection/disconnect` 在 F-C 桩上吞 panic（job 表为空时语义等价）；
   F-C 落地 `cancel_connection_jobs` 后该分支自然走真实现。
