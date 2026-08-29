# B-FILES-ARCHIVE 路交付报告（dbx-files-plugin 压缩包后端）

> 路线：实现 A-FILES 交接的后端规格（`docs/PROGRESS-A-FILES.zh-CN.md` §3）——
> `files/archiveList` 与 `files/extract`，tar / tar.gz / tgz 支持，zip 明确 Phase 2。
> 范围：`backend/**` + `scripts/smoke_test.py` + 本文档；`frontend/**` 只读未动；
> 无新 cargo 依赖（Cargo.lock 零新增条目）；无 git commit/push。

## 0. 验证终值

| 套件 | 结果 |
|---|---|
| cargo test（backend/ 全量） | **92 passed / 0 failed / 3 ignored**（基线 71 不回归 + 本路新增 21） |
| smoke（debug sidecar，fs + memory 段） | **PASS 42 / SKIP 2 / FAIL 0**——新增 5×2=10 个 archive 场景全 PASS |
| smoke（release sidecar 同套件） | **PASS 42 / SKIP 2 / FAIL 0** |
| cargo build --release | ✅（7.96s 增量 / 打包内全量 1m36s） |
| `dbx-plugin package .` | ✅ `dist/io.dbx.files-0.1.0-darwin-arm64.dbxp` |
| Cargo.lock | 无新增依赖条目（flate2 等 flate 系 crate 不在依赖树，gzip 解码为本路手写） |

## 1. 逐项完成

### ① `files/archiveList`（IMPL_PLAN §8.5 新增小节，契约同 A-FILES §3.1）
- 请求 `{ connectionId, path, page?, pageSize? }` →
  `{ entries: [{ name, path(条目内路径，无前导 /), kind: "file"|"directory", size, modifiedAt? }], total }`。
- `page`/`pageSize` 可选（A-FILES §3.1 契约即 optional）：缺省 `page=1`、`pageSize=200`，
  `pageSize` clamp 到 `[1, 1000]`；分页为内存切片（沿 `listPaged` 语义，越界页返回空
  `entries` + 真实 `total`）。
- 支持格式：`.tar`（手写 512B header 解析）、`.tar.gz`/`.tgz`（gzip → 手写 inflate 解流后按 tar 解析）。
- **zip 明确报 Phase 2**：magic `PK..` → -32000
  `"zip archives are not supported yet (Phase 2); use .tar / .tar.gz / .tgz"`。
- 防呆：条目数上限 50,000（超出报 "tar bomb guard"）；tar 头块迭代上限
  `4×max_entries+64`（防纯 meta 头炸弹）；gzip 解压输出 1 GiB 上限（解码中途即拦）；
  归档源文件本体 1 GiB 上限（stat 后拦截）。
- 路径白名单：归档路径走 `Gate::readable_path`（policy 层 check_read），越界/逃逸拒绝。

### ② `files/extract`
- 请求 `{ connectionId, path, targetPath }`（targetPath 为目录，不存在则 create_dir -p）。
- **双路传输**（响应形状与 copy/move 一致）：
  - 小包（**≤10 个文件且 ≤8 MiB 解压载荷**）同步执行 →
    `{ success: true, transport: "native", jobId: null }`；
  - 超限 → `transfers.rs::enqueue_extract_job`（新 `DirJobKind::Extract`）→
    `{ success: true, transport: "job", jobId }`，进度/取消/状态完全复用 dir-job 表：
    `files/transfer/progress`（`filesDone/filesTotal/bytesDone/bytesTotal` + `state`/`kind:"extract"`）、
    `files/transfer/status`、`files/transfers/list`（unified view）、
    `files/transfer/cancel`、`connection/disconnect` 连带取消（`cancel_connection_jobs`）。
- **门禁（写语义）**：`read_only` 拒绝（`ensure_writable`）；`allow_delete` **不**适用
  （解压只写目标、不删源，遵循 A-FILES §3.2）；targetPath 走
  `Gate::ensure_writable_path(…, is_dir=true)`（root 白名单 + `..`/反斜杠拒绝）；
  job 路入队前再过一次 `validate_dir_job_gates`（双重校验）。
- **zip-slip 防护**（`archive.rs::sanitize_entry_path`，表驱动单测）：
  - 拒绝：`..` 任意层级分量、绝对路径（`/x`）、Windows 盘符（`C:/x`）、
    反斜杠分隔（`a\b`）、空路径/仅 `.`；
  - 归一：`./a` → `a`、`a//b` → `a/b`、`dir/` → `dir`；
  - **符号/硬链接条目（typeflag 1/2）整体拒绝解压**（报错含
    "symbolic/hard link entries are rejected to prevent path escape"）——链接可把后续
    写重定向到目标之外；listing 时链接按 `kind:"file"`、size 0 呈现。
- 执行：逐条目 read（tar payload）→ `operator.write` 到
  `targetPath/entry.path`，父目录懒创建（`create_dir` 去重集合）；job 路逐条目间
  检查取消 flag、短锁更新 DirJob 计数、`Throttle`（200ms/1%）节流发进度。

### ③ 前端对接核对（只读，`frontend/**` 未改动）
- `App.vue::onConfirm` extract 分支：传参 `{ path, targetPath }` +
  `callFor` 注入 `connectionId` —— 与实现一致 ✅。
- 返回形状：前端判定 `result.transport === "job" && result.jobId` 后走
  `trackSidecarJob`（轮询 `files/transfer/status`、`jobId`/`status` 字段）——与
  DirJob camelCase 序列化一致 ✅；同步路径返回多带的 `success/transport/jobId:null`
  不影响前端（`jobId` 为 null 即走「立即刷新」分支）。
- `trackSidecarJob(jobId, "copy", …)` 的 kind 标签是前端既有常量，extract job 在
  传输面板会以 "copy" 标签显示（`files/transfer/progress` 事件里后端给出
  `kind:"extract"`，事件驱动分类可用）；如需精确标签改前端一处即可（遗留 ③）。
- `files/archiveList`：前端 PreviewPane 目前仍是占位文案
  （`archivePreviewUnsupported`），尚未消费该方法——返回形状按 A-FILES §3.1
  交接规格实现（`kind` 用 `"file"|"directory"`，注意与 FileEntry 的 `"dir"` 不同，
  这是交接文档原文约定）；`path` 为条目内路径（无前导 `/`）。后续前端接入时
  把占位替换为分页渲染即可，无需改后端。

### ④ 实现要点（`backend/src/archive.rs`，新增）
- **tar 解析（手写）**：
  - 512B header：name(0..100)/size(124..136)/mtime(136..148)/typeflag(156)/
    magic(257..263)；POSIX ustar（magic `ustar\0`）启用 prefix(345..500) 拼接；
    GNU（magic `ustar `）不用 prefix（长名走 `L`）。
  - 长名：GNU `L`（LongName）条目 + PAX `x`（下一生效）记录
    `<len> key=value\n`（支持 `path=` / `size=` 覆盖，`size` 覆盖在切 payload 前
    生效以支持 >8 GiB）；`g`（global）记录作为后续默认；`K`（linkname）消费忽略。
  - 数值字段：八进制 ASCII + GNU base-256（首字节高位标记，大端）。
  - 校验：header checksum（148..156 按 ASCII 空格签名约定）逐头验证，
    不匹配报 "not a valid tar archive: header checksum mismatch at offset N"
    （垃圾输入不会误入数值解析错误）；两个全零块终止；末尾截断报
    "truncated tar archive"。
- **gzip/DEFLATE（手写，无 flate crate）**：RFC 1951 inflate
  （stored / fixed / dynamic Huffman 全支持，puff 式 canonical 表 +
  15-bit 解码）；RFC 1952 gzip 封装（FEXTRA/FNAME/FCOMMENT/FHCRC 头字段、
  CRC32 编译期表校验、ISIZE 校验）；输出上限在解码中途强制。
- **响应/事件全 camelCase**；错误统一 `String` → `main.rs::to_plugin_error` -32000。

## 2. tar 解析边界说明

1. **压缩比上限/炸弹防护是双层的**：gzip 层限解压总字节（1 GiB，解码中途拦），
   tar 层限条目数（50k）与解压载荷总和（extract 1 GiB）；archiveList 不累计
   payload（只读 header），因此大 tar 的 listing 是 O(条目数) 而非 O(字节数)。
2. **PAX `size=` 覆盖**：POSIX 中大文件头内 size 字段装不下时由 `x` 记录给出
   真实 size——解析器在切片 payload 前应用覆盖，避免按错误 size 切错后续块。
3. **PAX 记录长度自含**（`<len>` 计自身），解析按长度逐条推进；脏记录
   （非法长度/越界）安全终止为「无覆盖」而不是 panic。
4. **GNU vs POSIX 长名**：`L` 条目（GNU）优先于 PAX `path=`？实现顺序为
   `pending_longname` 先取、PAX `path=` 覆盖之——与 GNU tar 实测行为一致
   （两者不同时出现；同时出现属非规范输入）。
5. **typeflag 语义**：`0`/`\0`/`7`（regular）、`5` 或 name 以 `/` 结尾（directory）、
   `1`/`2`（link，仅 listing 可见、extract 拒绝）、`L`/`K`/`x`/`X`/`g`（meta，消费）、
   其余 typeflag 按 regular 处理 size 但不带数据（不影响 listing）。
6. **归档顺序即返回顺序**：archiveList 不排序（保留 tar 顺序），分页切片基于
   该稳定顺序；前端如需目录置顶可自行复用 `lib/sorting.ts`。
7. **checksum 前置**：checksum 校验先于 size/mtime 数值解析——对随机垃圾输入
   给出确定性的 "not a valid tar archive"，而非八进制解析噪声。

## 3. 测试与 smoke 证据

### Rust 单测（`archive.rs` tests，21 个新增；fixtures 全部测试内运行时构造）
- tar：普通文件/目录条目（kind/size/mtime/camelCase 序列化）、ustar prefix 长名、
  GNU `L` 长名、PAX `path=` 覆盖、base-256 数值字段、checksum 损坏拒绝、
  非 tar/过短输入拒绝。
- zip-slip 表驱动：11 个拒绝用例（`../`、绝对、`..`、盘符、反斜杠、空…）+
  5 个归一用例；整档含 `../` 条目在解析期拒绝；链接条目 extract 拒绝 /
  listing 放行。
- 防呆：条目数上限（小 limits 注入）、extract 载荷上限。
- gzip/inflate：stored 块往返、**fixed Huffman 手工比特流往返**、CRC 损坏拒绝、
  解压上限中途拦截、zip Phase 2 拒绝、gzip 包装 tar 端到端 listing、
  plain tar Cow 借用通路。
- 分页边界表：`paginate(total,page,page_size)` 8 组边界（含越界页、page 0、
  page_size 0）。
- 调试期间另做与 zlib 的差分验证（11 组 raw deflate 语料：random/repeat/text/
  zeros × level 1/6/9，全部字节级一致）+ Python `tarfile w:gz` 样本——该验证为
  临时探针，未留在代码内；修复的两个 inflate 缺陷见 §5 附记。

### smoke_test.py（`scenario_archive`，fs + memory 双段各 5 场景）
- 构造：测试内用 python `tarfile`（`w:gz`，动态 Huffman deflate）生成
  tar.gz，经 `files/upload` 二进制通道上传。
- `archive-list`：3 条目（1 目录 + 2 文件）断言 path/kind/size/name/modifiedAt。
- `archive-list-paged`：`page=2&pageSize=2` → 最后一条 + `total=3`。
- `archive-extract-sync`：3 条目小包 → `{success}` 非 job → list/read 断言解出
  内容逐字节一致。
- `archive-extract-job`：11 文件包 → `transport:"job"` → `wait_job` 至 completed →
  `transfers/list` 可见 → 目标 11 文件齐全。
- `archive-zip-phase2-refusal`：zip 包 archiveList/extract 双双 -32000 且含
  "Phase 2"。
- 结果：`PASS 42 / SKIP 2 / FAIL 0`（debug 与 release sidecar 各跑一遍全绿）。

## 4. 文件清单

| 文件 | 变更 |
|---|---|
| `backend/src/archive.rs` | 新增（tar/手写 inflate/防呆/extract 原语 + 21 单测） |
| `backend/src/main.rs` | `mod archive` + `files/archiveList` / `files/extract` 分支（§8.2 后新增小节） |
| `backend/src/model.rs` | `ArchiveListRequest` / `ExtractRequest`（camelCase 反序列化） |
| `backend/src/transfers.rs` | `DirJobKind::Extract` + `enqueue_extract_job` + `run_extract_job` |
| `scripts/smoke_test.py` | `scenario_archive` 5 场景 + 头注更新 |
| `docs/PROGRESS-B-ARCHIVE.zh-CN.md` | 本文档（新建） |
| `dist/io.dbx.files-0.1.0-darwin-arm64.dbxp` | 打包产物（release sidecar 内含） |

## 5. 遗留

1. **zip 后端（Phase 2）**：central directory 解析 + deflate64/加密分支；当前 zip
   输入在两个方法上均返回明确 -32000（前端 `archiveKind` 已把 `.zip` 标为 Phase 2）。
2. **内存占用模型**：gzip 全量解压驻留内存（上限 1 GiB）；如后续要支持超大包，
   需改为流式 inflate + 逐条目落盘（跨方法重构，本期不做）。
3. **前端两处可选微调**（后端已兼容，不改也通）：extract job 在传输面板的 kind
   标签当前显示为 "copy"（`trackSidecarJob` 硬编码）；PreviewPane 压缩包占位可
   替换为 `files/archiveList` 分页渲染（形状见 §1③）。
4. archiveList 对非归档文件（如 .txt）返回明确解析错误（"not a valid tar archive"）；
   前端可先用 `lib/archive.ts` 扩展名识别避免触发（既有行为）。

### 附记：调试期修复的两个 inflate 缺陷（差分验证发现）
- `read_dynamic_tables` 未按 RFC 1951 §3.2.7 的 CL_ORDER 置换读取 HCLEN 段
  （顺序写入）——导致 >2KB 的动态块流全部失步；
- `Huffman::build` 的 offsets 数组按 16 长度声明但循环越界写 `offsets[16]`——
  改为 17 槽（原先截断到 `1..15` 的写法同样错误）。

## 6. 阻塞

无。


## 关单记录（patrol 2026-08-29）

- 遗留 ③：✅ 前端 PreviewPane archive 模式接 `files/archiveList`（200 条/页 + 加载更多 + total 计数行；后端未含该方法时回退占位文案）；`transferKind.extract` 七语补齐（传输面板 kind 标签正确显示）。
- 新增 i18n：transferKind.extract / archiveEntriesLabel / archiveLoadMore ×7 语（i18n.spec 递归键集断言护航）。
- 验证：前端 47/47、typecheck/build 全绿；cargo test 93/93 无回归；.dbxp 重打。
- 剩余：zip Phase 2、超大包流式重构（1 GiB 驻留上限）。

## 对标补齐记录（2026-08-29 任务轮）

- 文件名搜索/过滤（对标 tiny-rdm FileBrowserPane，缺口 #7 关单）：工具栏新增过滤框
  （`FileToolbar.vue` wb-search-box + lucide Search），`lib/searchFilter.ts` 大小写不敏感
  子串过滤（仅影响左栏展示，不动选择/删除语义）+ 3 用例 spec；i18n `searchPlaceholder` ×7。
- 只读门禁信号：`files/capabilities` 新增策略层 `readOnly` 字段；App.vue `canWrite` 与
  只读徽章并入（表单 read_only ∥ 宿主标准 read_only，后端为权威）。详见
  shared/PROGRESS-PROD-READONLY.zh-CN.md。
- 验证：cargo 94/94、前端 typecheck/build 全绿、vitest 50/50（+3）。
