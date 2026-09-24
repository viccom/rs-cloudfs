# WebDAV rename 真机缺陷修复批（Phase 7 真机 e2e 必修项）

> 分支：`fix/webdav-rename`（worktree `rs-cloudfs-rn`，基于 main `acb9b16`）
> 发现：2026-09-24 pan115 真机 e2e（`_e2e_pan115`）——负责人指令「BUG 必修」

## 缺陷

两个缺陷同在 L5 WebDAV 层，均**与驱动无关**（pan115 驱动真机全绿）：

### BUG 1：`VolumeRouter::dispatch` 不同步改写 `Destination` 头

- 现象：`/vol/<name>/` 前缀路由下的 **MOVE 恒 409 Conflict**，服务器零日志。
- 根因：`server.rs` 的 `dispatch` 只重写了请求 URI，`Destination` 头仍带
  `/vol/a/...` 前缀；dav-server 内部路径已是 `/dst.txt`，两者命名空间错位
  → `handle_copymove.rs` 的 `has_parent(&dest)` 失败 → 409。
- 影响面：**多卷模式下所有卷的改名完全不可用**（Phase 2.5 引入前缀路由时的漏洞）。

### BUG 2：`CyDriveFs::rename` 只改本地行，从不调驱动

- 现象：单卷模式（无前缀，绕开 BUG 1）MOVE 返回 **201 Created**，但源仍存在、
  目标不存在、远端名字不动——**假成功**。
- 根因：`CyDriveFs::rename` 只调 `db.rename_path()`；`Vfs` 根本没有 rename
  方法，全仓无任何地方从 WebDAV 路径调 `StorageDriver::rename`。与能力位
  `server_side_move: true` 的声明不符（R4）。
- 影响面：改名在本地与远端都不发生但报成功（数据完整性级）。

## 修复

| 位置 | 改动 |
|---|---|
| `cloudkit-core/src/vfs.rs` | 新增 `Vfs::rename_remote_for_row`——远端先行、能力门控。三态：无宽面（`as_driver()` None，telegram/mock）→ `Ok(())` 如实降级（行为与修复前逐字一致）；宽面但无 `server_side_move` → `Ok(())`（降级由驱动自负）；宽面 + 单侧 move → 调 `StorageDriver::rename`，`NotFound` 容忍（pending 行竞态）。失败返回带路径的 `Unavailable`，明示「本地行未动」。附 `vol_rel` 桥接（core 路径带前导 `/`，驱动面禁之——两类型词法不同） |
| `cloudkit-webdav/src/lib.rs` | `rename` 远端先行（`rename_remote_for_row`）再落本地行；覆盖臂的 dest 远端删除同样先行（防孤儿） |
| `cloudkit-webdav/src/server.rs` | `dispatch` 同步改写 `Destination` 头（`strip_destination_prefix`）：仅当 Destination 指向**本卷**时剥前缀（跨卷/外部目标原样保留，交 handler 报真错）；绝对 URI 保 scheme+authority，仅重写路径段 |

## TDD 证据（红→绿）

- **BUG 2**：`fs_adapter.rs::rename_on_a_wide_face_moves_the_remote_object`
  —— 红（`the remote object must exist at the new path after rename` 失败）
  → 绿。
- **BUG 1**：`multivolume.rs::move_through_the_volume_prefix_rewrites_the_destination`
  —— 红（复现真机 409）→ 绿。
- **helper 单测**：`server.rs::destination_prefix_strips_same_volume_only`
  钉 8 个边界（绝对 URI 切分、卷根→`/`、origin-form、跨卷 None、段边界
  `/vol/ab` ≠ `/vol/a`、无路径 None）。修复期实测踩到的
  `split_at` 算术错误（搜索须从 `://` 之后开始）由此钉死。

## 门禁（实跑）

- workspace **1688 passed / 0 failed / 62 ignored**（基线 1685 + 3 新增）
- clippy `-D warnings` 绿 / fmt 绿 / check_layers 绿 / scan_secrets 绿

## 真机复验（pan115，2026-09-24，修复后 exe）

| 验证项 | 结果 |
|---|---|
| 多卷 MOVE（`/vol/plain` 前缀） | **201**（修复前 409）；源 404 / 目标 207 |
| 远端真相（115 直连 API） | **`verify-renamed.bin`**——改名物理抵达远端（修复前远端仍旧名） |
| 加密卷上传→下载 | 100000B **逐字节**（SHA256 一致） |
| 加密卷远端核验 | 远端 100050B（+50B AEAD 容器），密文非明文 |
| 加密卷改名 | MOVE 201；改名后重新下载仍逐字节（密文容器完好） |
| 加密卷 Range 读（aead_v2） | `bytes=1000-1999` → 206，1000B，**与明文切片逐字节相符** |
| 加密卷删除 | DELETE 204 → PROPFIND 404 |

工作台：`E:\Rs_Codes\pan115-verify`（plain + enc 双卷，aead_v2）。
远端已清理回负责人原始 5 项；实例已停。

## BUG 3（追加批，2026-09-24 晚）：MKCOL 假成功

pan123 真机 e2e 第二轮（新账号，无配额限制）在项目 4 边界测试中揭出：
`CyDriveFs::create_dir` 只 upsert 本地行并标 `is_uploaded: true`（假声明），
**从不调驱动 `mkdir`**（全 L3+ 零调用点——与 BUG 2 的 `rename` 完全同构）。
真机实证：MKCOL 201 → 123 远端无此目录；本地行随后被 read-through
reconcile 正确剪除。**影响所有后端**（create_dir 为共享实现）。

### 修复

- `cloudkit-core/src/vfs.rs` 新增 `Vfs::mkdir_remote_for_row`——远端先行、
  门控与 rename 同源：无宽面（telegram/mock）→ `Ok(())` 如实降级（行为与
  修复前逐字一致）；宽面 → 调 `StorageDriver::mkdir`；`Exists` 上抛
  （事实性冲突，本地行不写——沿用远端既有目录才是真态）；其他失败带路径
  上抛且明示「本地行未写」。
- `cloudkit-webdav/src/lib.rs` `create_dir` 在 upsert 前先走
  `mkdir_remote_for_row`。

### TDD + 真机证据

- 红：`fs_adapter.rs::mkcol_on_a_wide_face_creates_the_remote_directory`
  —— `the remote directory must exist after MKCOL: NotFound` → 绿。
- **pan123 真机**：MKCOL 201 → 本地 207 → **123 远端出现 `zc-fix-dir`
  (Type=1)**；嵌套 `zc-fix-dir/sub/` + 上传 `inside.bin`(4096B) 全落地。
- **pan115 真机**：MKCOL 201 → 115 远端出现 `vr-fix-dir`（fc=0 目录）。
- **目录改名真机腿补齐**（原「未覆盖」项）：pan115 `vr-fix-dir/` →
  `vr-fix-dir-renamed/` MOVE 201，源 404/目标 207，115 远端确认为新名。

### 门禁

workspace **1689/0/62**（+1 新增）、clippy/fmt/layers/secrets 绿。
过程注记：workspace 首跑撞 ck-webdav `concurrent_401s_with_the_same_nonce`
flake 一次（L7 已记录的两个 pre-existing flake 之一，与改动无关——隔离
重跑 3/3 绿，全量重跑 1689/0）。

两处远端均已清理回负责人原始内容；实例已停。

## winfsp 盘符面追修批（2026-09-24 深夜，BUG 2/3 类残留）

负责人问询「baidu/sftp/webdav 是否存在最近审查发现的 BUG」→ 复核结论：
**驱动面与 WebDAV 服务面（8485/8486）干净，但盘符挂载面（winfsp——
Explorer 主用面）BUG 2/3 类缺陷仍在**。修复批 861c33c/285f32c 只碰了
cloudkit-core/cloudkit-webdav；本批补齐 FSD 面（对一切宽面后端成立：
baidu/sftp/webdav/pan115/pan123/local；telegram 窄面为正确降级）。

### 缺陷（复核确证）

- `fs.rs::rename_entry` 三臂（普通 / 覆盖 / 大小写改名）都只调
  `db.rename_path`，从不调 `rename_remote_for_row`；覆盖臂的
  `db.delete_file(to)` 也无远端删除先行——Explorer F2 = 同款假成功
  （本地行走、远端留旧路径，rebuild 后回退）。删除路径无此问题
  （`delete_entry` 早有 `delete_remote_for_row` 先行 + `bridge.block_on`
  先例——修复可行性由它证明，非新增机制）。
- `fs.rs::prepare_create` 目录腿走 core `Vfs::create_dir`（同步签名，
  只写本地行 + `is_uploaded: true` 假声明）——「新建文件夹刷新后消失」
  同款。

### 修复（fs.rs 四处，与 WebDAV 适配器逐臂同构）

| 臂 | 改动 |
|---|---|
| 普通改名 | `bridge.block_on(rename_remote_for_row)` 先于 `db.rename_path`；失败保行（Unavailable→EIO+error! 日志） |
| 覆盖改名 | dest 的 `delete_remote_for_row` 先于 `db.delete_file`（防远端孤儿） |
| 大小写改名 | 同普通臂——大小写翻转落到远端（rebuild 不再回退拼写） |
| 目录创建 | `require_dir_parent`（保 ParentMissing 序，防远端孤儿）→ `mkdir_remote_for_row`（远端已有时 Exists→`STATUS_OBJECT_NAME_COLLISION`）→ core `create_dir` |

### TDD 证据（红→绿，`cloudkit-winfsp/tests/wide_mutations.rs`）

宽面桩镜像生产 transport_face 形态（`delete_remote` = 句柄映射后
`driver.delete`，能力位镜像驱动声明 + `remote_delete`——不照实现抄）：

- `rename_on_a_wide_face_moves_the_remote_object`——红
  （`the remote object must be gone from the old path`）→ 绿
- `rename_overwrite_on_a_wide_face_replaces_the_remote_destination`——
  红（`the source must be gone from the remote`；旧远端目标 3B 留存）→ 绿
- `case_only_rename_on_a_wide_face_flips_the_remote_spelling`——红
  （`the old spelling must be gone`）→ 绿
- `explorer_new_folder_on_a_wide_face_creates_the_remote_directory`——
  红（`NotFound`）→ 绿
- `explorer_new_folder_collides_when_the_remote_already_has_it`——红
  （假成功返回 Ok）→ 绿（COLLISION 且零行写入）

5/5 红（失败原因逐臂精确对应缺陷）→ 5/5 绿，断言零漂移。

### 门禁（实跑）

- winfsp 特性腿 **123/0/1**（+5；窄面 118 测试全绿——降级臂零漂移）
- workspace **1700/0/62**（基线零漂移——winfsp 改动全在特性门内）
- clippy（winfsp+feature 与 workspace 两跑）绿 / fmt 绿 / check_layers 绿
  / scan_secrets 绿

### 观察项销账（复核后不修）

CLI `cydrive push` 祖先目录行（cli/lib.rs:4657 走 core `create_dir`，
同假声明形态）：**五宽面驱动上传腿均 `ensure_parents` 自愈**（baidu
upload.rs:357/693、pan115 upload.rs:825、pan123 upload.rs:824、sftp
K75-3 `create_dir_all`、webdav stager.rs:231）——行声明随上传落地变真；
上传失败中断的孤儿行由 read-through 剪除，无用户可见危害。挂账不修。

### 未覆盖

- **winfsp 面真机腿**（Explorer F2 / 新建文件夹于真卷）未跑——桩级
  5 测试钉死；同一 `bridge.block_on` + 远端先行机制在删除路径已过
  RV3 真机矩阵。
- **大小写改名 × 大小写不敏感远端服务器**（webdav 后端指向
  case-insensitive remote 时 MOVE `Overwrite:F` 可能 412）：WebDAV
  适配器面同语义已随车（无大小写特判），真机 fixture（rclone/apache
  on Linux）均大小写敏感，未构造不敏感腿。

## 未覆盖（原批遗留）

- **BUG 1 的跨卷 Destination 拒绝**：单测覆盖「跨卷前缀不剥」，但真机未
  构造「MOVE 到另一卷」用例（客户端一般不这么发）。
- ~~**目录改名**真机腿未跑~~ **已补齐**（见上：pan115 真机 201 + 远端确认）。
- **已存在的 `Destination` 绝对 URI host 校验**由 dav-server 自行履行，
  本修复不介入（保 scheme+authority 原样）。
