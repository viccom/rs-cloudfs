# Phase 3 / 3.5-a 深度代码审查报告（终版·经对抗性复核）

- **日期**：2026-09-11 ｜ **方法**：三路并行审查（winfsp crate 全量 / 加密流式栈端到端 / cli 集成与横切安全面）→ 全部发现做对抗性复核（优先尝试推翻；13 个探针测试实际编写并运行复现 + 27 项代码链/本地验证）→ 本报告为复核后终版
- **审查范围**：Phase 3 WinFsp（`849c8b8..2dcae23` + `6b25ede`）与 Phase 3.5-a 加密 Range 流式读（`39b0313..9b4b9e8`）全部代码
- **总体结论**：24 项 bug 类发现中 **18 项坐实**（4 项降级）、**3 项推翻**、其余为建议类；无凭空误报，被推翻项均为漏读守卫/上游契约。凭据红线（R3）三路横查零发现。密码学构造（STREAM：nonce 域分离 + 34B header 全量 AAD + 自定界布局）经攻击面专项核验为教科书级正确。

## 一、坐实发现（按修复优先序；严重度为复核后终判）

### winfsp-C1（High–Critical）仅改大小写的重命名全系破损
- **位置**：`crates/cloudkit-winfsp/src/fs.rs:1236-1277`（rename_entry）
- **机制**：`from==to` 比较原始拼写（大写变体≠规范名→通过）→ `resolve_row` 把 from 规范化 → `row(to)` 大小写不敏感**命中同一行** → 覆盖分支删行删缓存 → `rename_path` 对已删行 no-op（`database.rs:798-803` 零行更新仍 Ok）。
- **三形态**（探针 `verify_probe.rs` 四测试复现）：①目标全大写（最常见的改大小写操作）→ `0xc0000022` ACCESS_DENIED，改名直接失败；②混合大小写变体 → 改名成功但缓存副本被静默删除（NTFS 大小写不敏感 `fs::remove_file` 命中，db 精确匹配 miss）；③行+缓存整行摧毁（需 FSD 追踪名≠db 规范名的前置错位，窄但真实）。
- **上游佐证**：WinFsp 官方论坛确认 rename 源名以大写送达；本仓 `6b25ede` 真机记录同源。
- **修法**：dest 命中后比对 `dest.id == row.id`（同 rowid 即 case-only rename）→ 跳过删除分支直接改名+本地 rename；`from==to` 检查移到规范化之后做大小写不敏感比较（形态①走合法改名路径）。

### winfsp-H1（High）宽限表陈旧复用
- **位置**：`fs.rs:819-858`（acquire_read/take_live）、`fs.rs:1461-1481`（close 无条件 park）
- **证据**：探针复现——8 字节旧文件 open→read→close（park）→ 删除重建为 64 字节新行 → 5s 内重开读出 **8 字节旧 EOF**（新文件被截断）。删除/rename/提交路径均不失效宽限表。
- **修法**：`take_live` 复用时比对行 size（必要时 mtime）不符即弃；`delete_after_cleanup`/`rename_entry`/cleanup 提交成功后 `grace.invalidate(rel)`。

### winfsp-H4（High，挂载面 Medium）cleanup 提交失败语义失实
- **位置**：`fs.rs:1493-1515` + `writer.rs:219-235` + `vfs.rs:363-376`（put_staged 先 rename 后 upsert+enqueue）
- **证据**：探针复现（enqueue 失败注入）——字节停在最终缓存路径、行 pending 无任务、staged sibling 已不存在；日志"the write was discarded"与注释"already removed the staging sibling"均失实。行永久 pending，靠下次启动 `requeue_pending`（vfs.rs:876-878）治愈。
- **修法**：失败保留字节 + 修正日志/注释语义（"committed to cache but upload enqueue failed; row stays pending, requeued at next boot"）。

### winfsp-M1（Medium，pending 变体可 argue High）staging 兄弟名冲突摧毁他人副本
- **位置**：`writer.rs:62-68`（`.{name}.tmp` 原样 join）+ `cache.rs:72-81`
- **证据**：探针复现——`create("/foo")` 把行 `/.foo.tmp` 的唯一本地副本从 6 字节截到 0。触发面：远端存在编辑器临时文件形态名（不罕见）。
- **修法**：sibling 命名加随机段或探测存在即换名。

### winfsp-M2（Medium）Windows 保留名/尾点空格未消毒
- **位置**：`vpath.rs:31-33`（段校验只拒空/./..）+ `cache.rs:72-81`（原样 join）
- **证据**：本机实测——`/nul.txt` 写"成功"但字节进 NUL 设备永不落盘（is_cached 恒 false → 每次读重新下载死循环）；`/foo.`、`/foo `、`/foo. ` 三个虚拟行共享同一磁盘缓存文件（跨行缓存污染）。
- **修法**：`local_path` 映射层做保留名/尾点编码（不影响 vpath 语义与 db 存储）。

### winfsp-M3（Medium）超长名整目录枚举失败
- **位置**：`fs.rs:1710`（fill_dir_info `?` 传播）+ winfsp-rs `set_name_raw`（>255 宽字符 → `STATUS_INSUFFICIENT_RESOURCES`）
- **证据**：探针复现——一条 300 宽字符名使整目录枚举失败（非跳过）。入库通道：db/sync/telegram caption（sync 只校验合法性不校验长度）。
- **修法**：枚举对超长名单独 warn+跳过（不 fail 整目录）；写入面限段长。

### cli-H1（High）unmount 降级态拒释放自建映射
- **位置**：`main.rs:533-536` + `lib.rs:2943-2951`（仅判 `cfg.mount_backend==Winfsp`）
- **证据链**（闭合）：降级 mount 真建 net use 映射（`main.rs:425`→`windows.rs:135`）→ unmount 打印"映射是你自己挂的"并退出；`current_mount_for` 探测能力存在但 unmount 路径无引用。
- **修法**：unmount 先探测实际映射（根 URL/`/vol/<name>`），有则删、无则出 note。

### stream-H1（High，测试缺口）web 层缺 aead_v2 流式 E2E
- **证据**：`aead_v2`/`DecryptingTransport` 在 cloudkit-web 全 crate 零命中（rg 实证）；`web_e2e.rs:50` base 固定 Gcm。**这正是 2026-09-11 线上 bug（api 面开区间 0 字节）的所在层**——RangeBody×DecryptingTransport×WindowStream 三层嵌套无回归防线。
- **修法**：照 webdav 6h 形状补 web 层 aead_v2 行 Range GET 测试（206 + 明文 Content-Length + mock 调用形态断言）。

### winfsp-H2（Medium，自 High 降级）FSD 回调内 3 处 `.expect()` 依赖 db 不变式
- **位置**：`fs.rs:805/1180/1251`。探针证实 panic 真实发生（脏行 rel_path 含反斜杠）；但 winfsp-rs 全回调 `catch_panic`（interface.rs:70-76）——进程不死，仅该次操作失败。in-tree 写入方全保证合法 vpath，唯一通道是外部/遗留（Python 基线不校验）写入方共享 db。
- **修法**：三处换 `map_err`（一行 each）。

### cli-H2（Medium，自 High 降级）mount_cmd_winfsp 绕过双启守卫
- **位置**：`main.rs:441-493`（无 ensure_not_running；run 两路径有）。降级依据：db 为 WAL+busy_timeout（并发不损坏）；mount 不写 control 文件（守卫防的直接危害不存在）；后果=锁竞争/缓存双写（CacheManager 无跨进程锁）。
- **修法**：build_stack 前加 `ensure_not_running`。

### cli-M1/M2/M3（Medium）横切一致性
- M1：doctor（`platform/windows.rs:98-117` 首个 subkey 即 return）与 mount（`mount.rs:242-276` DLL 缺失 continue + 固定 260 buffer）两套探测行为漂移——双安装残留态下诊断与运行时判定矛盾。修法：对齐降级顺序语义。
- M2：winfsp 挂载任务 join 失败档（`lib.rs:3277-3283`）无 println 且**无降级**（对照 `Ok(Err)` 档有 fallback）；tracing 确实到 stdout（run 初始化了日志），真正缺口是降级行为。修法：补 fallback 或至少补声明性输出。
- M3：CI winfsp 腿缺 `cargo test -p cloudkit-cli --features winfsp`（`ci.yml:153-185`）——`the_capability_of_this_build_is_coherent` 的 feature 断言腿 CI 零执行。修法：加一步。

### stream-M1（Medium，潜伏债务）三面 inner 短读语义漂移
- `enc_stream.rs:177`（Err）vs `web/lib.rs:1031-1038`（静默终止）vs `webdav fill_window`（不校验）。三驱动现均自钳制（local/baidu/telegram 逐一带证据），今天无实际错答；未来驱动短读时分叉。修法：统一"短读=错误"，webdav `fill_window` 至少记日志。

### 其余坐实（Low）
- winfsp-M5：`writer_slot` std Mutex guard 持锁跨 `block_on(hydrate)`（`fs.rs:948-965`；clippy 盲因=同步 fn 内 block_on 无 await 关键字）——同句柄读写串扰非死锁；模块注释"only ever held synchronously"漂移。
- winfsp-M6（自 Medium 降）：缺父目录 open/get_security 报 NAME_NOT_FOUND 而非 PATH_NOT_FOUND（create 臂正确）。
- winfsp-L6：`with_stream_window` 无上限（测试缝误用面；生产恒 4MiB）。
- stream-M3（自 Medium 降）：`ciphertext_span`/`decrypt_chunk` 边界检查是 debug_assert（全部调用方恒界内，最坏 panic）。
- stream-L2：row.size 与容器不一致时错误文案漏第三种原因。
- cross-L7：deny `[graph] exclude` 豁免了 winfsp 的 advisories/bans（注释只提 license）。
- 原 Low 级信息项（卷标取配置字母、空 claims 误导提示、LetterInUse 文案、mount.rs INIT 注释与路径不符、VOLUME_SERIAL 固定、delete_pending 半死代码、unix_to_filetime 精度）——见各路原始审查记录，随修复批顺手处理或不处理。

## 二、推翻项（3）

| 原发现 | 推翻依据 |
|---|---|
| winfsp-L4"打开后 rename 致懒取旧路径" | `open_with_read` 在 open 时**急取**读状态（fs.rs:807-813）；仅 create 句柄残留极窄面 |
| winfsp-L5"病态后端无上限 fetch" | 循环 `n≥1` 恒推进，上界 = `buf.len()`（reader.rs:152-176 空窗 break + 三项 min 守卫） |
| cross-L5"gnu+winfsp 静默硬依赖 exe" | winfsp-sys build script 对 gnu 直接 panic（build.rs:117-124），指控产物不可达 |

## 三、正面确认（复核过且成立的设计与实现）

凭据红线零泄漏（全部日志/错误/Debug/CI 输出面横查）；K38 GPL 隔离三层设防无缝隙；K40 降级三档主路径完备且 stop 序列正确覆盖降级混合态（`lib.rs:1251-1255` 谓词含 WebDavFallback）；K44 元数据零网络；K45 错误映射穷尽且数值钉死；STREAM 密码学构造对重排/截断/换头 fail-closed 且有攻击测试；明密文坐标换算逐点验算无错位；锁中毒恢复 14 处模式一致；94 项 winfsp 测试全绿（审查期实跑）。

## 四、测试覆盖缺口（修复批应补）

1. case-only rename 三形态（现探针底稿在 `verify/review-findings` worktree：`crates/cloudkit-winfsp/tests/verify_probe.rs`，13 测试全绿）。
2. 宽限表陈旧复用（删除重建/覆盖写后重开）。
3. web 层 aead_v2 流式 E2E（=stream-H1）。
4. staging 兄弟名冲突；保留名/尾点；超长名枚举。
5. cleanup 提交失败路径（enqueue 失败注入）。
6. unmount 降级混合态；mount 双启互斥；CI winfsp 腿 cli 测试步。
7. WindowStream 多窗中途错误/inner 短读分支；`rel_from_winfsp` 未配对代理拒绝。

## 五、挂账（明确不在本修复批）

- 每请求 header RTT + PBKDF2 100k 的线性放大（设计已声明接受；可选 header/key 短 TTL LRU，seek 密集场景收益）。
- fs.rs 拆分（intent/handle/grace/ops）；窗口/坐标数学五处重复下沉为 `AeadV2Window` helper。
- Phase 3.6 运行态卷管理（K48-K51，已另有计划）。
