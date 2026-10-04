# 计划任务（scheduler）：cron 驱动的定时备份

> 日期：2026-10-04。阶段 1（rclone 三任务类型 + 调度骨架）实现于分支
> `codex/files/scheduler-rclone-jobs`；阶段 2（restic 快照备份引擎，以
> rclone 为存储驱动）另行开工。本文是调度子系统的行为契约。

## 0. 一句话

插件补上「定时备份」能力：按五字段 cron 周期性执行 rclone 目录作业
（镜像 sync / 增量 copy / 双向 bisync），任务定义持久化、运行历史可查、
执行复用现有作业通道（传输面板可见、可取消），并把任务管理暴露为
MCP 工具。**运行边界：任务只在 DBX 运行期间（sidecar 存活时）触发；
关闭期间错过的时点顺延到下个周期，不补跑。**

## 1. 架构

```
scheduler（backend/src/scheduler/）
  cron.rs   五字段 cron 解析器（Vixie 语义：dom/dow 双受限取 OR；
            `7` 折叠为周日；本地时区；400 天扫描上限）
  model.rs  ScheduleTask / RunRecord / 请求结构（serde camelCase）
  mod.rs    调度器：专用线程 + current_thread runtime（keepalive 模式），
            30s tick，单任务单飞 + 全局并发 2（Semaphore）
```

- **执行零重复**：作业启动钩子（`JobHooks`）由 `main.rs` 注入，逐字复用
  `rclone_start_dir_job` / `rclone_start_bisync_job` /
  `rclone_start_check_job`——定时运行与工作台 `files/syncDir`、MCP
  `files_sync` 落进同一个作业镜像，进度走同一条
  `files/transfer/progress`，取消走同一条 `files/transfer/cancel` 臂
  （`files/schedule/cancel` 仅解析出 jobId 后原路转发）。
- **持久化**（`Store`，`store.rs`）：`schedules.json`（任务定义，原子写）、
  `runs.json`（运行历史环形上限 500，按 run_id 原位更新）。凭据红线不变：
  任务只存连接 id 与路径，零 secret 落盘。
- **emitter 槽**：请求路径每次刷新（`handle_request` 顶部
  `scheduler.set_emitter`），后台运行据此推送 UI 事件；首个请求前事件
  丢失可接受（作业镜像仍可轮询）。

## 2. 运行语义

| 场景 | 行为 |
| --- | --- |
| 触发时连接未注册（sidecar 重启后未重连） | 记 `skipped`（区别于 `failed`——存储从未可达） |
| DBX 关闭/休眠错过时点 | 无记录；恢复后从「现在」重算 `next_run_at`，不补跑 |
| 同任务已有在途运行 | 拒绝（单飞）；手动运行同样受约束 |
| 全局并发满（>2 个任务同时在跑） | 排队等 Semaphore，运行记录停留在 `running` |
| bisync 首跑 | 自动带 `resync`；成功后落 `bisyncResyncDone`，之后走普通 run |
| sidecar 中途被杀 | 重启时 hydrate 把孤儿 `running` 记录结算为 `failed`（interrupted） |

### 后置钩子（均为 best-effort，失败只降级为「成功带告警」）

- **`verifyAfter`**：每次运行后对同源/目标发起 `operations/check` 比较
  作业，失败注记 `verify failed: …`。
- **`retentionDays`**（需 `backupDir`）：运行后对备份目录执行
  rc `operations/delete` + `_filter {"MinAge": "Nd"}` 清理超龄备份，
  120s 超时，失败注记 `retention: …`。

## 3. RPC：`files/schedule/*`

| 方法 | 请求 | 应答 |
| --- | --- | --- |
| `files/schedule/list` | – | `{ tasks: ScheduleTask[] }` |
| `files/schedule/create` | `ScheduleCreateRequest` | `{ task }`（400 语义：cron 非法/只读目标/retention 无 backupDir/重名） |
| `files/schedule/update` | `ScheduleUpdateRequest`（全量替换，运行时簿记字段保留） | `{ task }` |
| `files/schedule/delete` | `{ id }` | `{ removed }`（在途运行继续并照常结算） |
| `files/schedule/history` | `{ id?, limit? }`（limit 默认 100，clamp 1..500） | `{ runs: RunRecord[] }`（最新在前） |
| `files/schedule/runNow` | `{ id }` | `{ run }`（status=running；disabled 任务允许手动运行） |
| `files/schedule/cancel` | `{ id }` | `{ success }`（转发 `files/transfer/cancel`） |

`ScheduleTask`（camelCase）：`id, name, kind(sync|copy|bisync),
sourceConnectionId, sourcePath, targetConnectionId, targetPath, cron,
enabled, options{…DirJobRequest 同名选项 + verifyAfter, retentionDays,
backupDir, suffix…}, bisyncResyncDone, createdAt, lastRunAt,
lastRunStatus, nextRunAt`（unix 毫秒）。

`RunRecord`：`runId, taskId, jobId?, trigger(schedule|manual),
status(running|success|failed|canceled|skipped), startedAt, finishedAt,
bytes, files?, error?`。

### create/update 的校验（先于落盘）

cron 可解析；两侧连接已注册；sync 目标需可写可删、copy 目标可写、
bisync 两侧可写可删；源过读门、目标与 backupDir 过写门（与
`rclone_start_dir_job` 同一 policy 门）；`retentionDays` 必须搭配
`backupDir`；任务名唯一。**backupDir 与目标子树重叠**的拒绝在运行期由
sync.rs 给出（既有语义）。

## 4. 事件（sidecar → 前端）

| 事件 | 载荷 | 时机 |
| --- | --- | --- |
| `files/schedule/changed` | `{ tasks: ScheduleTask[] }` | 任务增删改、触发时刻 next_run 推进、终态簿记后 |
| `files/schedule/run` | `{ run: RunRecord }` | 认领（running）与终态各一次，run_id 原位覆盖 |

前端（dock 第 5 页签「计划任务」）：任务卡（cron 人类可读描述、下次
运行、结果徽章、启停开关）、创建/编辑对话框（预设 + 自定义 cron +
下次运行预览）、逐任务历史展开。运行中任务的字节进度在传输面板
（共享作业镜像）。

## 5. MCP 工具（5 个）

| 工具 | 相位 | 说明 |
| --- | --- | --- |
| `schedule_list` / `schedule_history` | 只读 | 任务清单 / 运行历史（`{id?, limit?}`） |
| `schedule_create` | 两阶段 confirmToken | preview → confirm；引擎侧校验同 RPC |
| `schedule_run_now` | 两阶段 confirmToken | 单飞约束与手动运行一致 |
| `schedule_delete` | 两阶段 confirmToken | 运行历史保留 |

审计：`files/schedule/create|runNow|delete` 记 `source:"mcp"`；定时触发
记 `source:"scheduler"`（`files/schedule/run`）。五个工具不依赖
connectionId（stdio 清单原样透出）。

## 6. 已知边界（写进 UI 文案）

- DBX 关闭/休眠不执行；系统级（launchd/systemd）常驻调度涉及凭据落盘，
  明确排除。
- restic 快照备份（去重/加密/快照浏览）为阶段 2：`ScheduleKind` 将增
  `restic` 变体，仓库以 `rclone:<remote>:<path>` 落在现有 86 后端上，
  仓库密码走 connection secret 绑定，不落盘。
- 测试：后端 24 项（cron 表驱动、store 环形/原位更新、假钩子端到端：
  完成/单飞/skipped/bisync 首跑/verify 告警），前端 23 项
  （schedules.ts 纯函数 + 面板 + 对话框）。
