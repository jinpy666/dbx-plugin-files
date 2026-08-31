# F-SFTP-NATIVE 交付报告（files 插件 russh 双栈 SFTP 适配层）

> 方案：`IMPL_PLAN_SFTP_NATIVE.zh-CN.md`（russh 0.63 + russh-sftp 2.4 +
> OpenDAL 0.57 自定义 Access 适配层，第 7 个快捷协议 `sftp-native`，双栈
> 决策：OpenDAL `sftp` 保留）。实施日期：2026-08-31。
> 由 mft-int-test.int.kn 真机实例测试驱动：OpenDAL sftp service 仅支持
> keyfile 认证，密码型 MFT 账号无法接入。

## 0. 验证终值

| 套件 | 结果 |
|---|---|
| `cargo test`（backend/ 全量） | **131 passed / 0 failed / 3 ignored**（新增 11 个 sftp-native 单测：endpoint 解析表、host-key 策略、kv 解析、离线建 Operator + 能力契约、user 优先级、坏 key 拒绝、绝对路径映射、deleter 携带 root） |
| 前端 vitest | **84 passed**（新增 sftp-native 模板/secrets 分离用例） |
| 前端 typecheck（vue-tsc） | ✅ |
| **真机实例**（mft-int-test.int.kn，密码认证，root=/pub） | **smoke sftp-native 段 20 场景全绿；整体 PASS 69 / SKIP 3 / FAIL 0**（62.2s）。覆盖 capabilities 契约、mkdir/write/list/read/copy/move/rename/**rename-dir-degrade**、delete、purge 拒根、purge-directory、stat、rmdir、二进制通道 round-trip、transfers list/status/cancel、publicLink 拒绝、read-only 门禁。只动自建 `/pub/smoke-native-*` 沙盒，收尾 purge 恢复原状 |
| 传输回归（真机，64KiB 包 / 窗口 4） | 147 KiB→2 MiB 上传+下载全部通过 |
| 容器回归（linuxserver/openssh-server，密码形态，本地） | 200 KiB→600 KiB 上传+下载全过 |

## 1. 逐任务完成

| # | 任务 | 状态 |
|---|---|---|
| F-SFTP-N-1 | `engine/sftp_native/` 三件套（Builder/Pool/Access，deleter 强制 root 映射） | ✅ |
| F-SFTP-N-2 | `engine/mod.rs` 路由 + `validate_endpoints` 卫生 + `model.rs` PROTOCOLS 扩容至 8 | ✅ |
| F-SFTP-N-3 | manifest：协议选项、字段可见性（endpoint/password/key/known_hosts_strategy）、七语 localizations | ✅ |
| F-SFTP-N-4 | 前端 `opendalServices.ts` sftp-native quick 模板（置尾对齐 PROTOCOLS 顺序）+ spec 用例 | ✅ |
| F-SFTP-N-5 | smoke `run_sftp_native_section`（密码形态，env-gated，只动自建沙盒） | ✅ |
| F-SFTP-N-6 | mft-int-test.int.kn 真机实例验证 + 传输兼容性调参 | ✅ |

## 2. 真机实例测试发现并修复的问题（本轮增量）

1. **share 名需拆分（SMB 侧配置面）**：`Projects/GCN_CostEntry/SHAAE_Report/Report`
   直接作 share 名 TreeConnect 报 `STATUS_BAD_NETWORK_NAME`；真实 share 为
   `Projects`，子路径应配 `root`。另：smb 的 `connection/test` 是假阳性
   （`check()` 对根 NotFound 宽容且适配器根 stat 本地应答），坏配置要到首个
   真实操作才暴露。
2. **SMB SmbDeleter root 逃逸（已定位、未修，独立任务）**：
   `engine/smb/access.rs` 的 deleter 不经 `smb_path()` 映射，配置 root 的
   连接上 delete/rmdir 静默无效（NotFound 被幂等吞掉）、purge 报错、
   rename-dir-degrade 源目录不删；且实际作用于 share 根同名路径——存在
   误删隐患。真机复现 + 无 root 连接验证根因。
3. **sftp-native create_dir 相对路径缺陷（本地容器确定性复现后修复）**：
   逐级 mkdir 的前缀丢失前导 `/`，目录按服务器端 cwd 解析建到别处。修复后
   前缀恒为绝对路径，与 §2 红线一致。
4. **MFT 服务器大包/管线兼容性（真机调参）**：单个 147 KiB SSH_FXP_WRITE
   挂死（128 KiB 通过）；russh-sftp 默认 8 深写管线偶发卡死。定参
   `max_packet_len=64KiB`、`max_concurrent_writes=4`（方案 §3）。
5. **russh 版本钉扎**：0.60.x 的 aes-gcm rc 精确依赖与 `smb2 =0.20.1`
   不可共存，钉 0.63.1（稳定 crypto 链，MSRV 1.85）。

## 3. 遗留与后续

- SMB `SmbDeleter` root 缺陷修复 + smoke root 变体（独立任务，见上）。
- 加密私钥（passphrase）支持：需 manifest 新增 secret 字段，暂与 OpenDAL
  sftp 能力对齐（拒绝并报清晰错误）。
- sftp-native 的 `connection/test` 已通过根 stat 真实探测（Access::stat 对
  root 走网络），后续可为 smb 补同款真实探测。
- russh 0.63.1 与 ssh 插件 0.60.3 版本暂不一致（smb2 约束所致）；russh
  升级时优先评估 smb2 是否有稳定 aes-gcm 的新版本。
