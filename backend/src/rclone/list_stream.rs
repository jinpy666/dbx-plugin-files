//! `files/listStream`：巨型目录的流式列举（issue #49 P1）。
//!
//! rclone rc 的 `operations/list` 没有服务端分页：一个巨型 S3 前缀只能整包
//! 返回（`files/list` 已保序截断、`listPaged` 超限报错）。本模块补上第三
//! 条路：rc 快路径以 5s 软超时先试全量；软超时先到则 spawn 一个短命
//! `rclone lsjson` 子进程逐行流式读取（lsjson 首行 `[`、每条目一行紧凑
//! JSON、末行 `]`，v1.71+ 已核实为流式输出），边读边分帧交付。
//!
//! 契约（与前端共享，字段名逐字固定）：
//! - ack：`{requestId, displayCharset?}`（displayCharset 非空才带）；
//! - 事件 `files/list/chunk`：`{requestId, seq, entries, done, total?,
//!   error?, partialCount?}`。`entries` 形状与 `files/list` 完全一致；
//!   终止帧 `done:true`（带 total）；失败帧 `error` 非空 + `partialCount`
//!   （已交付条数），不置 done。
//!
//! 会话纪律：
//! - 入口 gate 与 `files/list` 完全一致（[`gate_remote`]，同一
//!   `PathPolicy` 装配；`lock_to_root` 语义不可绕过）；
//! - 流式条目只过滤不排序（到达顺序交付；快路径单帧仍走 `ops::list` 的
//!   完整 `filter_and_sort`，两种排序口径均在契约内）；
//! - sidecar 不积累全量：batcher flush 后即弃，只留计数；
//! - listing 子进程按组隔离：复用同组 rcd 的 binary / 0600 私有 config /
//!   代理 env（`RcdSpawnInfo`），代理组连接不得绕过用户代理。

// StreamOutcome 与若干纯 helper 只被本模块测试消费。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;

use super::ops;
use super::proc::RcdSpawnInfo;
use super::rc::RcClient;
use crate::model::FileEntry;

/// rc 快路径软超时：小目录几乎总能在此完成（单帧交付，享受完整排序）。
const SOFT_TIMEOUT: Duration = Duration::from_secs(5);
/// 软超时的冒烟/联调旋钮：本地 fs 的 rc 列举远快于 5s，真机冒烟无法覆盖
/// 「升级到 lsjson 子进程」的路径——`DBX_FILES_LIST_STREAM_SOFT_TIMEOUT_MS`
/// 注入小值强制升级。仅接受 >0 的整数，其余值静默回落生产默认。
const SOFT_TIMEOUT_ENV: &str = "DBX_FILES_LIST_STREAM_SOFT_TIMEOUT_MS";
/// 流式硬上限 watchdog：超时 SIGKILL 子进程并发失败帧（timeout）。
const WATCHDOG: Duration = Duration::from_secs(600);
/// batcher 双条件之二：50ms 先到即成帧（256 条为先到条件之一）。
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// 一帧最多承载的条目数（batcher 双条件之一）。
pub const BATCH_MAX_ENTRIES: usize = 256;
/// 失败帧 error 里 stderr 尾部的保留上限（字节）。
const STDERR_KEEP: usize = 2048;
/// 子进程自然退出的宽限期；超时转 SIGKILL。
const CHILD_WAIT_GRACE: Duration = Duration::from_secs(3);
/// 全局并发上限：sidecar 进程内同时在途的 listStream 会话数。
const MAX_GLOBAL: u64 = 4;
/// 单连接并发上限：同一 connectionId 的在途会话数。
const MAX_PER_CONNECTION: u64 = 2;

// ---------------------------------------------------------------------------
// 逃生门
// ---------------------------------------------------------------------------

/// 逃生门：`DBX_FILES_LIST_STREAM=off` 时 `files/listStream` 一律禁用。
/// 保守回退开关——新特性出问题时宿主改一个环境变量即可关闭，无需重装。
/// 其余任何值（含未设置）均视为开启。
pub fn escape_hatch_off() -> bool {
    escape_hatch_off_with(|key| std::env::var_os(key))
}

/// 环境注入的判定核心（单测不碰真实进程环境）。
fn escape_hatch_off_with(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    lookup("DBX_FILES_LIST_STREAM")
        .map(|value| value == std::ffi::OsStr::new("off"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 会话入口 gate（与 files/list 完全一致）
// ---------------------------------------------------------------------------

/// 读侧 gate：与 `ops::gate_read`（`files/list` 的入口）逐字同一
/// `PathPolicy` 装配——`lock_to_root` 时路径必须留在 root 内，越界与
/// `..` 逃逸一律拒绝。返回 root-relative remote（"" = 根）。
pub fn gate_remote(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    let policy = crate::policy::PathPolicy::from_parts(root, lock_to_root, false, true);
    Ok(policy.check_read(path)?.relative)
}

/// `fs` + gate 后 remote 拼出 lsjson 的目标参数，语义与 `ops::fs_with_path`
/// 一致：named remote（尾冒号）直连同写，本地 fs 以 `/` 连接。
pub fn lsjson_target(fs: &str, remote: &str) -> String {
    if remote.is_empty() {
        fs.to_string()
    } else if fs.ends_with(':') {
        format!("{fs}{remote}")
    } else {
        format!("{}/{remote}", fs.trim_end_matches('/'))
    }
}

// ---------------------------------------------------------------------------
// lsjson 行解析
// ---------------------------------------------------------------------------

/// lsjson 输出一行的分类结果。
#[derive(Debug)]
pub enum LsLine {
    /// 首行 `[`。
    Open,
    /// 条目行（尾逗号已剥除、JSON 已解析并映射为 FileEntry）。
    Entry(FileEntry),
    /// 末行 `]`（权威终止标记）。
    Close,
}

/// 解析一行 lsjson 输出。`first` 表示是否首行。契约：首行必须为 `[`（否则
/// 视为协议破坏）；条目为单行紧凑 JSON，行尾可带逗号；`]` 表示正常结束。
/// 文件名中的换行由 JSON 字符串转义承载，永不跨行——转义处理完全交给
/// serde_json，不自行展开。
/// 空行不进本函数（调用方跳过；lsjson 实际不输出空行）。
pub fn parse_lsjson_line(first: bool, line: &str) -> Result<LsLine, String> {
    let text = line.trim();
    if first {
        return if text == "[" {
            Ok(LsLine::Open)
        } else {
            Err(format!("stream did not start with '[': {text:.80}"))
        };
    }
    if text == "]" {
        return Ok(LsLine::Close);
    }
    let json_text = text.strip_suffix(',').unwrap_or(text);
    match serde_json::from_str::<Value>(json_text) {
        Ok(item) => Ok(LsLine::Entry(entry_from_item(&item))),
        Err(error) => Err(format!("cannot parse lsjson entry: {error}")),
    }
}

/// rc item → FileEntry，与 `ops::entry_from_item`（`files/list` 的映射）
/// 语义逐字一致：`Path`/`Name`/`Size`/`ModTime`/`IsDir`，缺省回 `None`，
/// 目录不带 size，`path` 恒带前导 `/`。ops 侧的该函数为私有（不允许改动
/// ops.rs），此处是同一映射的镜像实现；`tests::keep_matches_ops_predicate`
/// 用同一输入样例锁定两侧行为不漂移。
fn entry_from_item(item: &Value) -> FileEntry {
    let raw_path = item
        .get("Path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_matches('/')
        .to_string();
    let name = item
        .get("Name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            raw_path
                .rsplit('/')
                .next()
                .filter(|segment| !segment.is_empty())
                .unwrap_or("/")
                .to_string()
        });
    let is_dir = item.get("IsDir").and_then(Value::as_bool).unwrap_or(false);
    let size = if is_dir {
        None
    } else {
        item.get("Size")
            .and_then(Value::as_i64)
            .filter(|size| *size >= 0)
            .map(|size| size as u64)
    };
    let modified_at = modtime_to_millis(item.get("ModTime"));
    FileEntry {
        name,
        path: if raw_path.is_empty() {
            "/".to_string()
        } else {
            format!("/{raw_path}")
        },
        kind: if is_dir { "dir" } else { "file" },
        size,
        modified_at,
    }
}

/// `ModTime` → Unix epoch millis（与 `ops::modtime_to_millis` 同一容错口径：
/// RFC3339 字符串优先，数值按量级归一，不可解析保持 None）。
fn modtime_to_millis(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::String(text) => {
            let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
            let millis = parsed.timestamp_millis();
            Some(millis.max(0) as u64)
        }
        Value::Number(number) => {
            let raw = number.as_f64()?;
            if raw < 0.0 {
                return None;
            }
            let millis = if raw >= 1e17 {
                raw / 1e6 // nanoseconds
            } else if raw >= 1e14 {
                raw / 1e3 // microseconds
            } else if raw >= 1e11 {
                raw // already milliseconds
            } else {
                raw * 1e3 // seconds
            };
            Some(millis as u64)
        }
        _ => None,
    }
}

/// 流式条目过滤：与 `ops::filter_and_sort` 的 retain 谓词一致（丢弃空路径
/// 与列表前缀自身 marker），但不排序——流式按到达顺序交付。
fn keep(entry: &FileEntry, prefix: &str) -> bool {
    let trimmed = entry.path.trim_matches('/');
    !trimmed.is_empty() && trimmed != prefix
}

// ---------------------------------------------------------------------------
// batcher（256 条 / 帧字节预算 / 50ms 三者先到即成帧；flush 后即弃）
// ---------------------------------------------------------------------------

struct Batcher {
    pending: Vec<FileEntry>,
    pending_bytes: usize,
    delivered: u64,
}

impl Batcher {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(BATCH_MAX_ENTRIES),
            pending_bytes: 0,
            delivered: 0,
        }
    }

    /// 满 [`BATCH_MAX_ENTRIES`] 或单帧字节预算（8MB 桥上限的 1/4，超预算
    /// 的帧宿主桥直接拒收——issue #49 深层根因）即刻成帧；否则返回 None
    /// 留待时间触发。
    fn push(&mut self, entry: FileEntry) -> Option<Vec<FileEntry>> {
        let size = serde_json::to_vec(&entry).map(|v| v.len() + 1).unwrap_or(1);
        self.pending_bytes += size;
        self.pending.push(entry);
        if self.pending.len() >= BATCH_MAX_ENTRIES
            || self.pending_bytes >= crate::rclone::ops::LIST_RESPONSE_BUDGET_BYTES
        {
            self.flush()
        } else {
            None
        }
    }

    /// 非空才成帧；成帧即交出所有权（sidecar 不积累全量，只留计数）。
    fn flush(&mut self) -> Option<Vec<FileEntry>> {
        if self.pending.is_empty() {
            return None;
        }
        self.pending_bytes = 0;
        let batch = std::mem::replace(&mut self.pending, Vec::with_capacity(BATCH_MAX_ENTRIES));
        self.delivered += batch.len() as u64;
        Some(batch)
    }

    /// 已交付条目数（失败帧的 partialCount 口径）。
    fn delivered(&self) -> u64 {
        self.delivered
    }
}

// ---------------------------------------------------------------------------
// 取消标志（watch channel：注册/触发/等待零丢失）
// ---------------------------------------------------------------------------

/// 会话取消标志。`listCancel` 的同步 handler 与组 teardown hook 都只能
/// 置位；真正的 SIGKILL + `wait()` 收割由持有子进程的会话任务在它的
/// select 循环里执行（那里才有 async 上下文）。
#[derive(Clone)]
pub struct CancelFlag(Arc<CancelInner>);

struct CancelInner {
    sender: tokio::sync::watch::Sender<bool>,
    /// 常驻 receiver：保证 watch 通道永不 closed。tokio `Sender::send` 在
    /// 通道关闭（所有 receiver 都已 drop）时既返回 Err 也**不存储值**——
    /// 没有它，wait() 首次 subscribe 之前发生的 cancel() 会静默丢失。
    _receiver: tokio::sync::watch::Receiver<bool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        Self(Arc::new(CancelInner {
            sender,
            _receiver: receiver,
        }))
    }

    /// 置位并唤醒所有 wait()。幂等。
    pub fn cancel(&self) {
        let _ = self.0.sender.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.sender.borrow()
    }

    /// 异步等待取消。`borrow_and_update` + `changed` 组合保证取消发生在
    /// 任何时刻都不丢失。
    pub async fn wait(&self) {
        let mut receiver = self.0.sender.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 并发闸 + 会话注册表
// ---------------------------------------------------------------------------

/// 全局/单连接两级并发闸 + requestId 会话注册表（同一把 std-Mutex 短临界
/// 区纪律：不持锁 await）。
pub struct ListStreams {
    limits: std::sync::Mutex<Limits>,
    sessions: std::sync::Mutex<HashMap<String, SessionHandle>>,
    max_global: u64,
    max_per_connection: u64,
}

struct Limits {
    global: u64,
    per_connection: HashMap<String, u64>,
}

struct SessionHandle {
    group: String,
    cancel: CancelFlag,
}

impl ListStreams {
    pub fn new() -> Self {
        Self::with_limits(MAX_GLOBAL, MAX_PER_CONNECTION)
    }

    /// 上限可注入（busy 边界单测用小值，不必真的开 4 个会话）。
    fn with_limits(max_global: u64, max_per_connection: u64) -> Self {
        Self {
            limits: std::sync::Mutex::new(Limits {
                global: 0,
                per_connection: HashMap::new(),
            }),
            sessions: std::sync::Mutex::new(HashMap::new()),
            max_global,
            max_per_connection,
        }
    }

    fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 占一个并发名额（全局 + 单连接原子检查）。超限返回以 `listing busy:`
    /// 开头的错误（契约错误前缀）。检查在拉起 rcd 之前，busy 请求零成本。
    pub fn try_acquire(self: &Arc<Self>, connection_id: &str) -> Result<ListSlot, String> {
        let mut limits = Self::lock(&self.limits);
        if limits.global >= self.max_global {
            return Err(format!(
                "listing busy: global limit of {} concurrent listings reached",
                self.max_global
            ));
        }
        let used = limits
            .per_connection
            .get(connection_id)
            .copied()
            .unwrap_or(0);
        if used >= self.max_per_connection {
            return Err(format!(
                "listing busy: connection '{connection_id}' already has {used} concurrent listings"
            ));
        }
        limits.global += 1;
        *limits
            .per_connection
            .entry(connection_id.to_string())
            .or_insert(0) += 1;
        Ok(ListSlot {
            streams: Arc::clone(self),
            connection_id: connection_id.to_string(),
        })
    }

    fn release(&self, connection_id: &str) {
        let mut limits = Self::lock(&self.limits);
        limits.global = limits.global.saturating_sub(1);
        if let Some(used) = limits.per_connection.get_mut(connection_id) {
            *used = used.saturating_sub(1);
            if *used == 0 {
                limits.per_connection.remove(connection_id);
            }
        }
    }

    /// 登记一个会话（ack 前调用：流尚未开始到达时 `listCancel` 就能命中）。
    /// 返回会话持有的取消标志 + 终态注销 guard。
    pub fn register(self: &Arc<Self>, request_id: &str, group: &str) -> (CancelFlag, SessionGuard) {
        let cancel = CancelFlag::new();
        Self::lock(&self.sessions).insert(
            request_id.to_string(),
            SessionHandle {
                group: group.to_string(),
                cancel: cancel.clone(),
            },
        );
        (
            cancel,
            SessionGuard {
                streams: Arc::clone(self),
                request_id: request_id.to_string(),
            },
        )
    }

    /// `files/listCancel`：置位取消标志（幂等）。子进程的 SIGKILL 与
    /// `wait()` 收割由会话任务的 select 循环执行；未知/已结束 id 返回
    /// false。
    pub fn cancel(&self, request_id: &str) -> bool {
        match Self::lock(&self.sessions).get(request_id) {
            Some(handle) => {
                handle.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// teardown hook 目标（`RcdSupervisor::shutdown_group` 在拆 rcd 前调
    /// 用）：让该组全部在途会话终止其 lsjson 子进程。同步上下文里只能置
    /// 位取消标志——会话任务被调度后立即 SIGKILL + 收割。残余竞态窗口是
    /// 子进程尚未 open config 的毫秒级瞬间；Unix 上已打开的 fd 不受
    /// unlink 影响，故配置删除不会造成读到半个文件的后果。
    pub fn kill_group(&self, group: &str) -> usize {
        let sessions = Self::lock(&self.sessions);
        let mut killed = 0;
        for handle in sessions.values() {
            if handle.group == group {
                handle.cancel.cancel();
                killed += 1;
            }
        }
        killed
    }
}

impl Default for ListStreams {
    fn default() -> Self {
        Self::new()
    }
}

/// 并发占位：Drop 释放两级计数。随会话任务持有，任何终态路径都释放。
pub struct ListSlot {
    streams: Arc<ListStreams>,
    connection_id: String,
}

impl std::fmt::Debug for ListSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListSlot")
            .field("connection_id", &self.connection_id)
            .finish()
    }
}

impl Drop for ListSlot {
    fn drop(&mut self) {
        self.streams.release(&self.connection_id);
    }
}

/// 会话注册 RAII：Drop 从注册表注销（任何终态）；此后 `listCancel` 返回
/// false。
pub struct SessionGuard {
    streams: Arc<ListStreams>,
    request_id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        ListStreams::lock(&self.streams.sessions).remove(&self.request_id);
    }
}

// ---------------------------------------------------------------------------
// 分帧交付
// ---------------------------------------------------------------------------

/// chunk 帧交付通道。生产实现桥接 `PluginEmitter`（`files/list/chunk`）；
/// 测试用收集 sink 断言帧序列，无需真实宿主管道。
pub trait ChunkSink: Send + Sync {
    fn emit(&self, payload: Value);
}

/// 普通帧：`{requestId, seq, entries, done, total?}`。
fn chunk_frame(
    request_id: &str,
    seq: u64,
    entries: Vec<FileEntry>,
    done: bool,
    total: Option<u64>,
) -> Value {
    let mut frame = json!({
        "requestId": request_id,
        "seq": seq,
        "entries": entries,
        "done": done,
    });
    if let (Some(total), Some(object)) = (total, frame.as_object_mut()) {
        object.insert("total".into(), json!(total));
    }
    frame
}

/// 取消终帧：后端侧取消（组 teardown、快路径期取消竞态）也必须给前端
/// 一个可结算的终态——静默结束曾让 ack 已登记的会话永久悬挂（M3）。
/// 前端自己发起的取消已把会话置终态，该帧按契约被丢弃，无副作用。
fn emit_cancelled_frame(request_id: &str, prefix: &str, sink: &dyn ChunkSink) {
    sink.emit(error_frame(
        request_id,
        1,
        format!("Failed to list '{prefix}': listing cancelled"),
        0,
    ));
}

/// 失败帧：`error` 非空 + `partialCount`（已交付条数），不置 done。
fn error_frame(request_id: &str, seq: u64, error: String, partial_count: u64) -> Value {
    json!({
        "requestId": request_id,
        "seq": seq,
        "entries": [],
        "done": false,
        "error": error,
        "partialCount": partial_count,
    })
}

// ---------------------------------------------------------------------------
// 会话状态机
// ---------------------------------------------------------------------------

/// 会话时序参数。生产默认即 [`Self::default`]；watchdog 与 flush 间隔必须
/// 可在集成测试中注入，否则 600s/50ms 无法验证。
#[derive(Clone, Copy)]
pub struct StreamOpts {
    pub soft_timeout: Duration,
    pub watchdog: Duration,
    pub flush_interval: Duration,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            soft_timeout: soft_timeout_from_env(),
            watchdog: WATCHDOG,
            flush_interval: FLUSH_INTERVAL,
        }
    }
}

/// 解析 `DBX_FILES_LIST_STREAM_SOFT_TIMEOUT_MS`：纯函数便于单测（并发测试
/// 进程里改真实 env 会互相踩）。
fn parse_soft_timeout_ms(value: Option<&str>) -> Duration {
    match value.and_then(|text| text.parse::<u64>().ok()).filter(|ms| *ms > 0) {
        Some(ms) => Duration::from_millis(ms),
        None => SOFT_TIMEOUT,
    }
}

fn soft_timeout_from_env() -> Duration {
    parse_soft_timeout_ms(std::env::var(SOFT_TIMEOUT_ENV).ok().as_deref())
}

/// 一次 listStream 会话的全部输入。`slot`/`guard` 随规格 move 进会话任务：
/// 函数体任何 return 都触发 Drop（释放并发计数 + 注销注册表）。
pub struct SessionSpec {
    pub request_id: String,
    pub client: RcClient,
    pub fs: String,
    /// 原始请求路径（快路径 `ops::list` 会重新 gate，幂等）。
    pub path: String,
    /// 入口 gate 后的 root-relative remote（升级路径拼 lsjson 目标）。
    pub remote: String,
    /// 过滤前缀（remote trim 后）。
    pub prefix: String,
    pub root: String,
    pub lock_to_root: bool,
    pub spawn_info: RcdSpawnInfo,
    pub sink: Arc<dyn ChunkSink>,
    pub cancel: CancelFlag,
    pub opts: StreamOpts,
    pub slot: ListSlot,
    pub guard: SessionGuard,
    /// 升级决策缓存键的一半（连接维度）+ 成功回写归属。
    pub connection_id: String,
    /// P2：升级决策缓存（已知巨型目录免软超时直通；成功完成回写计数）。
    pub cache: Arc<super::list_decision::DecisionCache>,
}

/// 会话终态（sink 已发对应帧；取消不发帧）。sidecar 日志 + 测试断言用。
#[derive(Debug, PartialEq)]
pub enum StreamOutcome {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// 会话状态机：由 main.rs 在同步 ack 之后 `tokio::spawn`。
pub async fn run_session(spec: SessionSpec) {
    let SessionSpec {
        request_id,
        client,
        fs,
        path,
        remote,
        prefix,
        root,
        lock_to_root,
        spawn_info,
        sink,
        cancel,
        opts,
        slot: _slot,
        guard: _guard,
        connection_id,
        cache,
    } = spec;

    // P2 升级决策：记忆中的巨型目录免软超时直通（误触发只付一次冷启动，
    // 漏触发用户每次导航干等——代价不对称，允许缓存粗糙）。
    let cached_escalate = cache.escalate_hint(&connection_id, &remote);
    if cached_escalate {
        eprintln!("[io.dbx.files] listStream '{prefix}' ({request_id}): escalate (cached huge dir)");
    }

    // 快路径：软超时包住 ops::list（与 files/list 同一入口，内部完成
    // gate_read）。它走无总超时的 rc 客户端——timeout 中断即放弃该 future。
    // 取消优先于快路径结果；缓存直通（Skipped）必须落入下方升级段——
    // 三者折叠成同一个值会让缓存命中静默零帧（B1 评审修复）。
    enum FastAttempt {
        Cancelled,
        Skipped,
        Done(Vec<FileEntry>),
        Failed(String),
    }
    let fast = if cached_escalate {
        FastAttempt::Skipped
    } else {
        tokio::select! {
        _ = cancel.wait() => FastAttempt::Cancelled,
        result = tokio::time::timeout(
            opts.soft_timeout,
            ops::list(&client, &fs, &path, false, &root, lock_to_root),
        ) => match result {
            Ok(Ok(entries)) => FastAttempt::Done(entries),
            Ok(Err(error)) => FastAttempt::Failed(error),
            Err(_elapsed) => FastAttempt::Skipped,
        },
        }
    };
    match fast {
        // 已取消：终帧显式告知（后端组 teardown 等静默取消也曾让前端
        // 永久悬挂——M3 评审修复）。前端对自身发起的取消已置终态，
        // 该帧会被 requestId 会话的终态检查丢弃。
        FastAttempt::Cancelled => {
            emit_cancelled_frame(&request_id, &prefix, sink.as_ref());
            return;
        }
        FastAttempt::Skipped => {} // 缓存直通/软超时 → 升级为 lsjson 流式
        FastAttempt::Done(entries) => {
            // 快路径成功：filter_and_sort 完整排序后按帧预算切块交付。
            // 小目录仍是一帧 done（尾帧）；大响应若单帧梭哈会被 8MB 桥
            // 上限直接拒收（帧静默丢失、前端悬挂）——issue #49 的深层根因。
            let total = entries.len() as u64;
            cache.record(&connection_id, &remote, total);
            let mut seq: u64 = 0;
            let mut batcher = Batcher::new();
            for entry in entries {
                if let Some(batch) = batcher.push(entry) {
                    seq += 1;
                    sink.emit(chunk_frame(&request_id, seq, batch, false, None));
                }
            }
            seq += 1;
            let tail = batcher.flush().unwrap_or_default();
            sink.emit(chunk_frame(&request_id, seq, tail, true, Some(total)));
            return;
        }
        FastAttempt::Failed(error) => {
            // rc 真实失败（路径不存在等）：失败帧即终态，不再升级。
            sink.emit(error_frame(&request_id, 1, error, 0));
            return;
        }
    }

    // 升级：spawn 短命 lsjson。stdin=null（无输入），stdout/stderr=piped。
    let mut command = std::process::Command::new(&spawn_info.binary);
    command
        .arg("lsjson")
        .arg(format!("--config={}", spawn_info.config_path.display()))
        .arg("--no-mimetype")
        .arg("--log-level=NOTICE")
        .arg(lsjson_target(&fs, &remote))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 同组代理 env：代理组连接的 listing 子进程不得绕过用户代理。
    spawn_info.apply_env_to(&mut command);
    let child = tokio::process::Command::from(command)
        // 兜底：会话任务异常退出时也不能遗留孤儿 lsjson。
        .kill_on_drop(true)
        .spawn();
    match child {
        Ok(child) => {
            let (outcome, delivered) =
                stream_child(&request_id, &prefix, child, &cancel, &opts, sink.as_ref()).await;
            if outcome == StreamOutcome::Completed {
                if let Some(total) = delivered {
                    cache.record(&connection_id, &remote, total);
                }
            }
            eprintln!("[io.dbx.files] listStream '{prefix}' ({request_id}): {outcome:?}");
        }
        Err(error) => {
            sink.emit(error_frame(
                &request_id,
                1,
                format!("Failed to list '{prefix}': cannot spawn rclone lsjson: {error}"),
                0,
            ));
        }
    }
}

/// 流式读循环终态驱动。
enum LoopEnd {
    /// 已见 `]`（权威终止）。
    Closed,
    /// stdout 结束但未见 `]`（输出被截断，协议不完整）。
    Eof,
    Parse(String),
    Read(String),
}

/// 读循环：逐行解析 → 过滤 → batcher；select 监听取消（biased 优先）、
/// watchdog、flush tick。sink 消费者即帧序列。
pub(crate) async fn stream_child(
    request_id: &str,
    prefix: &str,
    mut child: tokio::process::Child,
    cancel: &CancelFlag,
    opts: &StreamOpts,
    sink: &dyn ChunkSink,
) -> (StreamOutcome, Option<u64>) {
    // 第二个值：成功完成时 done 帧的 total（P2 决策缓存回写用）。
    use tokio::io::AsyncBufReadExt;

    let stdout = child.stdout.take().expect("lsjson stdout is piped");
    let stderr_pipe = child.stderr.take().expect("lsjson stderr is piped");
    // stderr 独立收集（失败帧只带尾部 2KB）：kill/退出后管道 EOF，任务
    // 自然结束。
    let mut stderr_task = tokio::spawn(stderr_tail(stderr_pipe));

    let mut lines = BufReader::new(stdout).lines();
    let mut batcher = Batcher::new();
    let mut seq: u64 = 0;
    let mut first = true;
    let mut flush_ticker = tokio::time::interval(opts.flush_interval);
    flush_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let watchdog_deadline = tokio::time::Instant::now() + opts.watchdog;

    let end = loop {
        tokio::select! {
            biased;
            _ = cancel.wait() => {
                kill_and_reap(&mut child).await;
                let _ = take_stderr(&mut stderr_task).await;
                // 终帧显式告知：后端侧静默取消（组 teardown）曾让 ack 已
                // 登记的前端会话永久悬挂（M3）。前端自己发起的取消早已把
                // 会话置终态，该帧按契约被丢弃，无副作用。
                seq += 1;
                sink.emit(error_frame(
                    request_id,
                    seq,
                    format!("Failed to list '{prefix}': listing cancelled"),
                    batcher.delivered(),
                ));
                return (StreamOutcome::Cancelled, None);
            }
            _ = tokio::time::sleep_until(watchdog_deadline) => {
                kill_and_reap(&mut child).await;
                let stderr = stderr_suffix(take_stderr(&mut stderr_task).await);
                seq += 1;
                sink.emit(error_frame(
                    request_id,
                    seq,
                    format!(
                        "Failed to list '{prefix}': streaming listing timed out after {}s{stderr}",
                        opts.watchdog.as_secs()
                    ),
                    batcher.delivered(),
                ));
                return (StreamOutcome::TimedOut, None);
            }
            _ = flush_ticker.tick() => {
                // 50ms 先到即成帧；batcher 空 = 无帧（interval 首个 tick
                // 立即触发，落在空 batcher 上无副作用）。
                if let Some(entries) = batcher.flush() {
                    seq += 1;
                    sink.emit(chunk_frame(request_id, seq, entries, false, None));
                }
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let text = line.trim();
                    if text.is_empty() {
                        continue; // 防御：lsjson 不输出空行
                    }
                    match parse_lsjson_line(first, text) {
                        Ok(LsLine::Open) => first = false,
                        Ok(LsLine::Close) => break LoopEnd::Closed,
                        Ok(LsLine::Entry(entry)) => {
                            if keep(&entry, prefix) {
                                if let Some(entries) = batcher.push(entry) {
                                    seq += 1;
                                    sink.emit(chunk_frame(request_id, seq, entries, false, None));
                                }
                            }
                        }
                        Err(error) => break LoopEnd::Parse(error),
                    }
                }
                Ok(None) => break LoopEnd::Eof,
                Err(error) => break LoopEnd::Read(error.to_string()),
            }
        }
    };

    let stderr = stderr_suffix(take_stderr(&mut stderr_task).await);
    match end {
        LoopEnd::Closed => {
            // `]` 是权威终止标记；进程通常已在退出中，宽限收割即可。
            let _ = wait_gracefully(&mut child).await;
            if let Some(entries) = batcher.flush() {
                seq += 1;
                sink.emit(chunk_frame(request_id, seq, entries, false, None));
            }
            seq += 1;
            sink.emit(chunk_frame(
                request_id,
                seq,
                Vec::new(),
                true,
                Some(batcher.delivered()),
            ));
            (StreamOutcome::Completed, Some(batcher.delivered()))
        }
        LoopEnd::Eof => {
            // stdout 先于 `]` 结束：输出被截断。退出码非 0 时优先报告真实
            // 退出原因。已成功解析的 pending 条目先尽力交付，partialCount
            // 与实际交付严格一致。
            let status = wait_gracefully(&mut child).await;
            if let Some(entries) = batcher.flush() {
                seq += 1;
                sink.emit(chunk_frame(request_id, seq, entries, false, None));
            }
            let detail = match status {
                Some(status) if !status.success() => format!("lsjson exited with {status}{stderr}"),
                _ => format!("stream ended before the closing ']' bracket{stderr}"),
            };
            seq += 1;
            sink.emit(error_frame(
                request_id,
                seq,
                format!("Failed to list '{prefix}': {detail}"),
                batcher.delivered(),
            ));
            (StreamOutcome::Failed, None)
        }
        LoopEnd::Parse(error) | LoopEnd::Read(error) => {
            kill_and_reap(&mut child).await;
            // 同 Eof：中断前已解析成功的条目先交付再失败。
            if let Some(entries) = batcher.flush() {
                seq += 1;
                sink.emit(chunk_frame(request_id, seq, entries, false, None));
            }
            seq += 1;
            sink.emit(error_frame(
                request_id,
                seq,
                format!("Failed to list '{prefix}': {error}{stderr}"),
                batcher.delivered(),
            ));
            (StreamOutcome::Failed, None)
        }
    }
}

/// SIGKILL + 收割。tokio `Child::kill` 语义即 SIGKILL：lsjson 无持久状态
/// 可刷（不做 flush，也不写任何远端），立即终止无副作用；`wait()` 保证
/// 不遗留僵尸进程。取消与 watchdog、解析中断路径都走这里。
async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(CHILD_WAIT_GRACE, child.wait()).await;
}

/// 等待子进程自然退出（宽限 [`CHILD_WAIT_GRACE`]）；超时转 SIGKILL 并
/// 收割，返回 None 表示未能取得退出码。
async fn wait_gracefully(child: &mut tokio::process::Child) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(CHILD_WAIT_GRACE, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => {
            kill_and_reap(child).await;
            None
        }
    }
}

/// 收割 stderr 收集任务（管道 EOF 后立即结束；500ms 兜底防异常卡死）。
async fn take_stderr(task: &mut tokio::task::JoinHandle<String>) -> String {
    match tokio::time::timeout(Duration::from_millis(500), &mut *task).await {        Ok(Ok(text)) => text,
        _ => String::new(),
    }
}

/// 收集子进程 stderr 尾部（≤[`STDERR_KEEP`] 字节）。滚动缓冲：总量超过
/// 2×上限后丢前段保尾段——巨型 WARNING 流不会撑爆失败帧。
async fn stderr_tail(pipe: tokio::process::ChildStderr) -> String {
    use tokio::io::AsyncReadExt;

    let mut reader = BufReader::new(pipe);
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > STDERR_KEEP * 2 {
                    let keep_from = tail.len() - STDERR_KEEP;
                    tail.drain(..keep_from);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).trim().to_string()
}

/// stderr 文本拼进失败帧 error 的后缀：空 → 原样；非空 → 带 `; stderr:`
/// 分隔标签（错误主因与子进程输出不粘连）。
fn stderr_suffix(stderr: String) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; stderr: {stderr}")
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(path: &str, kind: &'static str) -> FileEntry {
        FileEntry {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: format!("/{path}"),
            kind,
            size: Some(0),
            modified_at: None,
        }
    }

    // -- 逃生门 ---------------------------------------------------------------

    #[test]
    fn escape_hatch_only_honors_off() {
        assert!(escape_hatch_off_with(|_| Some("off".into())));
        assert!(!escape_hatch_off_with(|_| None));
        for value in ["on", "OFF", "0", "false", ""] {
            assert!(
                !escape_hatch_off_with(move |key| {
                    if key == "DBX_FILES_LIST_STREAM" {
                        Some(value.into())
                    } else {
                        None
                    }
                }),
                "'{value}' must not disable the stream"
            );
        }
    }

    // -- 会话入口 gate（与 ops::list 同一装配） --------------------------------

    #[test]
    fn gate_remote_resolves_relative_and_absolute_forms() {
        assert_eq!(gate_remote("", false, "").unwrap(), "");
        assert_eq!(gate_remote("", false, "/").unwrap(), "");
        assert_eq!(gate_remote("", false, "sub/x").unwrap(), "sub/x");
        assert_eq!(gate_remote("/srv/data", true, "sub/x").unwrap(), "sub/x");
        assert_eq!(
            gate_remote("/srv/data", true, "/srv/data/sub").unwrap(),
            "sub"
        );
        assert_eq!(gate_remote("/srv/data", true, "").unwrap(), "");
    }

    #[test]
    fn gate_remote_rejects_lock_to_root_escapes() {
        // lock_to_root 下绝对路径越界与 .. 逃逸必须拒绝——错误文本即
        // PathPolicy 原文（files/list 同款）。
        assert!(gate_remote("/srv/data", true, "/etc/x").is_err());
        assert!(gate_remote("/srv/data", true, "../x").is_err());
    }

    /// 死 client（gate 在任何 HTTP 流量之前失败）：断言 listStream 的入口
    /// gate 与 files/list 的 ops::list 入口 gate 对同一输入产出逐字相同的
    /// 错误——两处装配漂移即失败。
    #[tokio::test]
    async fn gate_remote_matches_ops_list_gate() {
        let client = RcClient::new(
            "http://127.0.0.1:1".to_string(),
            "u".to_string(),
            "p".to_string(),
        );
        for (root, path) in [("/srv/data", "/etc/x"), ("/srv/data", "../x")] {
            let ops_error =
                ops::list(&client, "fs:", path, false, root, true).await.unwrap_err();
            let gate_error = gate_remote(root, true, path).unwrap_err();
            assert_eq!(ops_error, gate_error, "gate drift for '{path}'");
        }
    }

    // -- lsjson 目标拼接 -------------------------------------------------------

    #[test]
    fn lsjson_target_joins_colon_and_local_forms() {
        assert_eq!(lsjson_target("dbxAb12:", ""), "dbxAb12:");
        assert_eq!(lsjson_target("dbxAb12:", "a/b"), "dbxAb12:a/b");
        assert_eq!(lsjson_target("/", "sub"), "/sub");
        assert_eq!(lsjson_target("/local/root", "sub/deep"), "/local/root/sub/deep");
        assert_eq!(lsjson_target("/local/root/", "x"), "/local/root/x");
    }

    // -- lsjson 行解析 ---------------------------------------------------------

    #[test]
    fn parse_first_line_accepts_bracket_only() {
        assert!(matches!(parse_lsjson_line(true, "["), Ok(LsLine::Open)));
        assert!(matches!(parse_lsjson_line(true, " [\r"), Ok(LsLine::Open)));
        let error = parse_lsjson_line(true, "garbage").unwrap_err();
        assert!(error.contains("did not start with '['"), "{error}");
        // 首行也不接受条目。
        assert!(parse_lsjson_line(true, "{\"Path\":\"a\"}").is_err());
    }

    #[test]
    fn parse_entry_line_with_and_without_trailing_comma() {
        let raw = json!({"Path": "a", "Name": "a", "IsDir": false, "Size": 3});
        let line = serde_json::to_string(&raw).unwrap();
        for text in [format!("{line},"), line] {
            match parse_lsjson_line(false, &text).unwrap() {
                LsLine::Entry(entry) => {
                    assert_eq!(entry.name, "a");
                    assert_eq!(entry.path, "/a");
                    assert_eq!(entry.kind, "file");
                    assert_eq!(entry.size, Some(3));
                }
                other => panic!("expected entry, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_close_line_and_entry_shape_parity() {
        assert!(matches!(parse_lsjson_line(false, "]"), Ok(LsLine::Close)));
        // 目录条目：不带 size（与 files/list 完全一致），path 恒带前导 /。
        let dir = parse_lsjson_line(
            false,
            r#"{"Path":"sub","Name":"sub","IsDir":true,"Size":4096,"ModTime":"2024-01-01T00:00:00.000000000Z"},"#,
        )
        .unwrap();
        match dir {
            LsLine::Entry(entry) => {
                assert_eq!(entry.kind, "dir");
                assert_eq!(entry.size, None);
                assert_eq!(entry.path, "/sub");
                assert_eq!(entry.modified_at, Some(1704067200000));
            }
            other => panic!("expected entry, got {other:?}"),
        }
    }

    /// 换行文件名的安全性完全由 JSON 字符串转义承载：单行内的 `\n` 转义
    /// 解析为真实换行，永不跨行破坏行协议。
    #[test]
    fn parse_entry_with_newline_in_name() {
        let line = r#"{"Path":"a\nb","Name":"a\nb","IsDir":false,"Size":1},"#;
        match parse_lsjson_line(false, line).unwrap() {
            LsLine::Entry(entry) => {
                assert_eq!(entry.name, "a\nb");
                assert_eq!(entry.path, "/a\nb");
            }
            other => panic!("expected entry, got {other:?}"),
        }
    }

    #[test]
    fn parse_bad_entry_lines_fail() {
        assert!(parse_lsjson_line(false, "not-json").is_err());
        assert!(parse_lsjson_line(false, "{\"Path\":").is_err());
    }

    // -- 条目过滤与 filter_and_sort 谓词一致性 ---------------------------------

    /// 参照谓词逐字复制自 `ops::filter_and_sort` 的 retain 分支（ops.rs 不
    /// 在本任务允许的修改边界内，故以镜像 + 样例锁定语义）。漂移即失败。
    fn ops_filter_and_sort_predicate(entry: &FileEntry, prefix: &str) -> bool {
        let trimmed = entry.path.trim_matches('/');
        !trimmed.is_empty() && trimmed != prefix
    }

    #[test]
    fn keep_matches_ops_predicate() {
        let entries = vec![
            entry("", "dir"),               // 空 path（根 marker 的 path="/a/" trim 形态之一）
            entry("sub", "dir"),            // 前缀自身 marker
            entry("sub/", "dir"),           // 前缀 marker 带尾斜杠
            entry("sub/a.txt", "file"),
            entry("other/b.txt", "file"),
        ];
        for prefix in ["", "sub", "deep/prefix"] {
            for entry in &entries {
                assert_eq!(
                    keep(entry, prefix),
                    ops_filter_and_sort_predicate(entry, prefix),
                    "predicate drift for path={} prefix={prefix}",
                    entry.path
                );
            }
        }
        // 语义样例（与 ops.rs filter_and_sort 测试同款断言）：根 marker 与
        // 前缀 marker 被丢弃，子条目保留。
        assert!(!keep(&entry("", "dir"), ""));
        assert!(!keep(&entry("sub", "dir"), "sub"));
        assert!(keep(&entry("sub/a.txt", "file"), "sub"));
    }

    // -- batcher ---------------------------------------------------------------

    #[test]
    fn batcher_flushes_at_256_and_keeps_count() {
        let mut batcher = Batcher::new();
        let mut frames = 0;
        for index in 0..255 {
            assert!(
                batcher.push(entry(&format!("f{index}"), "file")).is_none(),
                "no frame before 256 entries"
            );
        }
        assert_eq!(batcher.delivered(), 0);
        if batcher.push(entry("f255", "file")).is_some() {
            frames += 1;
        }
        assert_eq!(frames, 1, "the 256th push must flush");
        assert_eq!(batcher.delivered(), BATCH_MAX_ENTRIES as u64);
        // 剩余不满一帧：显式 flush 交付，计数继续累计。
        batcher.push(entry("tail", "file"));
        let rest = batcher.flush().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(batcher.delivered(), (BATCH_MAX_ENTRIES + 1) as u64);
        // 空的 flush 无帧（tick 落在空 batcher 上无副作用）。
        assert!(batcher.flush().is_none());
    }

    #[test]
    fn batcher_flushes_at_frame_byte_budget_before_entry_cap() {
        // 单条 ~16KB 名称（序列化后 name+path ≈ 32KB）：帧字节预算（8MB 桥
        // 上限的 1/4）必然先于 256 条触发——超预算的帧宿主桥直接拒收
        // （issue #49 深层根因）。
        let long = "n".repeat(16 * 1024);
        let mut batcher = Batcher::new();
        let mut pushed = 0usize;
        let frame = loop {
            match batcher.push(entry(&format!("{long}-{pushed}"), "file")) {
                Some(batch) => break batch,
                None => pushed += 1,
            }
            assert!(pushed < 256, "byte budget must fire before the entry cap");
        };
        assert!(pushed >= 32, "2MB 预算应容纳 32KB 级条目数十条以上");
        let serialized = serde_json::to_vec(&frame).unwrap().len();
        assert!(
            serialized < crate::rclone::ops::LIST_RESPONSE_BUDGET_BYTES * 2,
            "成帧体积必须留在桥上限量级内（含信封余量）"
        );
    }


    // -- busy 并发边界 ----------------------------------------------------------

    #[test]
    fn busy_enforces_global_and_per_connection_limits() {
        let streams = Arc::new(ListStreams::with_limits(2, 1));
        let slot_c1 = streams.try_acquire("c1").expect("first c1 slot");
        // 单连接上限 1：第二个 c1 slot busy，但 c2 仍可用。
        let error = streams.try_acquire("c1").unwrap_err();
        assert!(error.starts_with("listing busy:"), "{error}");
        assert!(error.contains("c1"), "{error}");
        let slot_c2 = streams.try_acquire("c2").expect("first c2 slot");
        // 全局上限 2：第三个连接 busy。
        let error = streams.try_acquire("c3").unwrap_err();
        assert!(error.starts_with("listing busy:"), "{error}");
        assert!(error.contains("global"), "{error}");
        // 释放后全部恢复（Drop 即释放两级计数）。
        drop(slot_c1);
        drop(slot_c2);
        assert!(streams.try_acquire("c1").is_ok());
        assert!(streams.try_acquire("c3").is_ok());
    }

    // -- 取消标志 ---------------------------------------------------------------

    #[tokio::test]
    async fn cancel_flag_wakes_waiters_and_is_idempotent() {
        let flag = CancelFlag::new();
        let waiter = tokio::spawn({
            let flag = flag.clone();
            async move {
                flag.wait().await;
            }
        });
        assert!(!flag.is_cancelled());
        flag.cancel();
        flag.cancel(); // 幂等
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must wake")
            .expect("waiter join");
        assert!(flag.is_cancelled());
    }

    // -- 注册表：register / cancel / kill_group / 终态注销 -----------------------

    #[test]
    fn registry_cancel_is_idempotent_and_unknown_ids_are_false() {
        let streams = Arc::new(ListStreams::new());
        let (cancel, guard) = streams.register("req-1", "direct");
        assert!(streams.cancel("req-1"));
        assert!(cancel.is_cancelled());
        assert!(streams.cancel("req-1"), "repeat cancel still finds the session");
        assert!(!streams.cancel("nope"));
        // 终态注销：guard drop 后 cancel 变 false（幂等契约的另一半）。
        drop(guard);
        assert!(!streams.cancel("req-1"));
    }

    #[test]
    fn registry_kill_group_targets_group_members_only() {
        let streams = Arc::new(ListStreams::new());
        let (cancel_a, guard_a) = streams.register("a", "group-1");
        let (_cancel_b, guard_b) = streams.register("b", "group-1");
        let (cancel_c, guard_c) = streams.register("c", "group-2");
        assert_eq!(streams.kill_group("group-1"), 2);
        assert!(cancel_a.is_cancelled());
        assert!(!cancel_c.is_cancelled(), "other group must stay untouched");
        assert_eq!(streams.kill_group("ghost"), 0);
        drop((guard_a, guard_b, guard_c));
        assert_eq!(streams.kill_group("group-1"), 0, "guards unregistered");
    }

    // -- 帧组装 -----------------------------------------------------------------

    #[test]
    fn frames_match_the_wire_contract() {
        let entries = vec![entry("a", "file")];
        let chunk = chunk_frame("req", 7, entries.clone(), true, Some(9));
        assert_eq!(chunk["requestId"], "req");
        assert_eq!(chunk["seq"], 7);
        assert_eq!(chunk["entries"][0]["name"], "a");
        assert_eq!(chunk["done"], true);
        assert_eq!(chunk["total"], 9);
        assert!(chunk.get("error").is_none());
        assert!(chunk.get("partialCount").is_none());

        // 未终止的普通帧不带 total。
        let mid = chunk_frame("req", 1, entries, false, None);
        assert_eq!(mid["done"], false);
        assert!(mid.get("total").is_none());

        let failed = error_frame("req", 2, "boom".to_string(), 12);
        assert_eq!(failed["requestId"], "req");
        assert_eq!(failed["seq"], 2);
        assert_eq!(failed["entries"].as_array().unwrap().len(), 0);
        assert_eq!(failed["done"], false, "失败帧不置 done");
        assert_eq!(failed["error"], "boom");
        assert_eq!(failed["partialCount"], 12);
    }

    // ---------------------------------------------------------------------------
    // 集成测试（unix）：用 `sh -c` 伪造 lsjson 子进程，验证 stream_child 的
    // 完整读循环——帧序列、失败帧、watchdog 与取消，无需真实 rclone。
    // ---------------------------------------------------------------------------

    /// 帧收集 sink：断言 batch 序列用，无需真 emitter。
    #[derive(Clone, Default)]
    struct VecSink(Arc<std::sync::Mutex<Vec<Value>>>);

    impl VecSink {
        fn frames(&self) -> Vec<Value> {
            self.0.lock().unwrap().clone()
        }
    }

    impl ChunkSink for VecSink {
        fn emit(&self, payload: Value) {
            self.0.lock().unwrap().push(payload);
        }
    }

    fn spawn_sh(script: &str) -> tokio::process::Child {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .expect("sh spawn")
    }

    /// 确定性时序：关掉时间 flush（1h tick）与 watchdog（1h），只让条目数
    /// 与脚本行为驱动帧；需要 watchdog 的测试单独注入短时长。
    fn deterministic_opts() -> StreamOpts {
        StreamOpts {
            soft_timeout: Duration::from_secs(5),
            watchdog: Duration::from_secs(3600),
            flush_interval: Duration::from_secs(3600),
        }
    }

    #[test]
    fn soft_timeout_env_knob_parses_strictly() {
        assert_eq!(parse_soft_timeout_ms(None), SOFT_TIMEOUT);
        assert_eq!(parse_soft_timeout_ms(Some("")), SOFT_TIMEOUT);
        assert_eq!(parse_soft_timeout_ms(Some("abc")), SOFT_TIMEOUT);
        assert_eq!(parse_soft_timeout_ms(Some("0")), SOFT_TIMEOUT, "0 disables the guard rail semantics — rejected");
        assert_eq!(parse_soft_timeout_ms(Some("-5")), SOFT_TIMEOUT);
        assert_eq!(parse_soft_timeout_ms(Some("1")), Duration::from_millis(1));
        assert_eq!(parse_soft_timeout_ms(Some("750")), Duration::from_millis(750));
    }


    fn entry_paths(frame: &Value) -> Vec<String> {
        frame["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_string())
            .collect()
    }

    /// 299 条带尾逗号 + 1 条不带 + `]`：300 条目 → 256 触发 1 帧，EOF 后
    /// flush 尾帧，再 done 帧；seq 连续、到达顺序、total 正确。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_flushes_by_entry_count_and_finishes_with_done() {
        let mut script = String::from("printf '[\\n'\n");
        for index in 0..299 {
            script.push_str(&format!(
                "printf '{{\"Path\":\"f{index}\",\"Name\":\"f{index}\",\"IsDir\":false,\"Size\":{index},\"ModTime\":\"2024-01-01T00:00:00.000000000Z\"}},\\n'\n"
            ));
        }
        script.push_str("printf '{\"Path\":\"last\",\"Name\":\"last\",\"IsDir\":false,\"Size\":0,\"ModTime\":\"2024-01-01T00:00:00.000000000Z\"}\\n'\nprintf ']\\n'\n");
        let child = spawn_sh(&script);
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Completed);

        let frames = sink.frames();
        assert_eq!(frames.len(), 3, "256-flush + tail-flush + done: {frames:?}");
        assert_eq!(frames[0]["seq"], 1);
        assert_eq!(entry_paths(&frames[0]).len(), 256);
        assert_eq!(frames[0]["done"], false);
        assert_eq!(frames[1]["seq"], 2);
        assert_eq!(entry_paths(&frames[1]).len(), 44);
        assert_eq!(entry_paths(&frames[1])[0], "/f256");
        // done 帧：空 entries + total。
        assert_eq!(frames[2]["seq"], 3);
        assert_eq!(frames[2]["done"], true);
        assert_eq!(frames[2]["total"], 300);
        assert_eq!(frames[2]["entries"].as_array().unwrap().len(), 0);
    }

    /// 流式条目只过滤不排序：乱序到达按原顺序交付（路径字典序被刻意打乱）。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_delivers_entries_in_arrival_order_unsorted() {
        let script = "printf '[\\n'\n\
            printf '{\"Path\":\"z\",\"Name\":\"z\",\"IsDir\":false,\"Size\":1},\\n'\n\
            printf '{\"Path\":\"a\",\"Name\":\"a\",\"IsDir\":false,\"Size\":2},\\n'\n\
            printf '{\"Path\":\"m\",\"Name\":\"m\",\"IsDir\":false,\"Size\":3}\\n'\n\
            printf ']\\n'\n";
        let child = spawn_sh(script);
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Completed);
        let frames = sink.frames();
        assert_eq!(frames.len(), 2, "one data frame + done: {frames:?}");
        assert_eq!(
            entry_paths(&frames[0]),
            vec!["/z", "/a", "/m"],
            "到达顺序，不排序"
        );
    }

    /// 前缀 marker 与根 marker 被过滤，其余保留（与 filter_and_sort 的过滤
    /// 语义一致）。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_filters_marker_entries() {
        let script = "printf '[\\n'\n\
            printf '{\"Path\":\"\",\"Name\":\"\",\"IsDir\":true},\\n'\n\
            printf '{\"Path\":\"sub\",\"Name\":\"sub\",\"IsDir\":true},\\n'\n\
            printf '{\"Path\":\"sub/file\",\"Name\":\"file\",\"IsDir\":false,\"Size\":1}\\n'\n\
            printf ']\\n'\n";
        let child = spawn_sh(script);
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "sub",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Completed);
        let frames = sink.frames();
        assert_eq!(entry_paths(&frames[0]), vec!["/sub/file"]);
        assert_eq!(frames[1]["total"], 1);
    }

    /// stdout 结束但未见 `]`、退出码非 0：失败帧带退出码与 stderr 尾部，
    /// partialCount 反映已交付条数，且不置 done。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_nonzero_exit_yields_failure_frame() {
        let script = "printf '[\\n'\n\
            printf '{\"Path\":\"kept\",\"Name\":\"kept\",\"IsDir\":false,\"Size\":1},\\n'\n\
            echo 'boom from lsjson' >&2\n\
            exit 3\n";
        let child = spawn_sh(script);
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Failed);
        let frames = sink.frames();
        assert_eq!(frames.len(), 2, "one data frame + failure: {frames:?}");
        assert_eq!(frames[1]["done"], false);
        assert_eq!(frames[1]["partialCount"], 1);
        let error = frames[1]["error"].as_str().unwrap();
        assert!(error.contains("exited with"), "{error}");
        assert!(error.contains("boom from lsjson"), "{error}");
        assert!(error.starts_with("Failed to list"), "{error}");
    }

    /// 首行不是 `[`：协议破坏 → 立即失败帧。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_garbage_first_line_yields_failure_frame() {
        let child = spawn_sh("echo garbage");
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Failed);
        let frames = sink.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["partialCount"], 0);
        assert!(
            frames[0]["error"]
                .as_str()
                .unwrap()
                .contains("did not start with '['"),
            "{}",
            frames[0]["error"]
        );
    }

    /// 条目行损坏（解析中断）→ 失败帧。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_broken_entry_line_yields_failure_frame() {
        let child = spawn_sh("printf '[\\nnot-json\\n]\\n'");
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Failed);
        let frames = sink.frames();
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0]["error"]
                .as_str()
                .unwrap()
                .contains("cannot parse lsjson entry"),
            "{}",
            frames[0]["error"]
        );
    }

    /// 退出码 0 但没有 `]`：输出被截断 → 失败帧。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_truncated_output_yields_failure_frame() {
        let child = spawn_sh("printf '[\\n'");
        let sink = VecSink::default();
        let (outcome, _total) = stream_child(
            "req",
            "",
            child,
            &CancelFlag::new(),
            &deterministic_opts(),
            &sink,
        )
        .await;
        assert_eq!(outcome, StreamOutcome::Failed);
        let frames = sink.frames();
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0]["error"]
                .as_str()
                .unwrap()
                .contains("closing ']' bracket"),
            "{}",
            frames[0]["error"]
        );
    }

    /// watchdog：挂起子进程被 SIGKILL，失败帧注明 timeout（时长可注入）。
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_kills_hung_child() {
        // exec：printf 后 sh 被 sleep 替换，SIGKILL 杀掉的就是我们跟踪的
        // pid，不会遗留孤儿。
        let child = spawn_sh("printf '[\\n'; exec sleep 30");
        let pid = child.id();
        let mut opts = deterministic_opts();
        opts.watchdog = Duration::from_millis(150);
        let sink = VecSink::default();
        let (outcome, _total) = stream_child("req", "", child, &CancelFlag::new(), &opts, &sink).await;
        assert_eq!(outcome, StreamOutcome::TimedOut);
        let frames = sink.frames();
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0]["error"].as_str().unwrap().contains("timed out"),
            "{}",
            frames[0]["error"]
        );
        assert_eq!(frames[0]["partialCount"], 0);
        // OS 层确认子进程已被收割（非僵尸）。
        assert!(!unix_process_alive(pid.expect("pid")), "pid {pid:?} still alive");
    }

    /// 取消：SIGKILL 子进程 + 收割，静默结束（不发任何帧）。
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_child_and_stays_silent() {
        let child = spawn_sh("printf '[\\n'; exec sleep 30");
        let pid = child.id();
        let cancel = CancelFlag::new();
        let sink = VecSink::default();
        // 等 `[` 落地、子进程进入挂起态，再触发取消。
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
        let mut opts = deterministic_opts();
        opts.watchdog = Duration::from_secs(3600);
        let (outcome, _total) = stream_child("req", "", child, &cancel, &opts, &sink).await;
        assert_eq!(outcome, StreamOutcome::Cancelled);
        // M3：取消必须发恰好一帧 cancelled 终帧——后端侧静默取消（组
        // teardown）曾让 ack 已登记的前端会话永久悬挂。前端自己发起的
        // 取消已置终态，该帧按契约被丢弃。
        let frames = sink.frames();
        assert_eq!(frames.len(), 1, "取消发且仅发一帧终态: {frames:?}");
        assert!(frames[0].get("error").and_then(Value::as_str).unwrap_or("").contains("listing cancelled"));
        assert!(!unix_process_alive(pid.expect("pid")), "pid {pid:?} still alive");
    }

    /// `ps` 报告 pid 存活且非僵尸（与 proc.rs 测试同一判定）。
    #[cfg(unix)]
    fn unix_process_alive(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps must exist on unix");
        if !output.status.success() {
            return false;
        }
        let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
        !stat.is_empty() && !stat.starts_with('Z')
    }
}
