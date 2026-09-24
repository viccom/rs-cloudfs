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

## 未覆盖

- **BUG 1 的跨卷 Destination 拒绝**：单测覆盖「跨卷前缀不剥」，但真机未
  构造「MOVE 到另一卷」用例（客户端一般不这么发）。
- **目录改名**真机腿未跑（本轮只测文件改名；目录走同一 `rename` 路径，
  单测 `rename_file_moves_row_cache_and_chunks` 旁有目录覆盖）。
- **已存在的 `Destination` 绝对 URI host 校验**由 dav-server 自行履行，
  本修复不介入（保 scheme+authority 原样）。
