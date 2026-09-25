# 三平台构建指南（Windows / Linux / macOS）

> 2026-09-15 实测沉淀（Phase 4 深度审查批之后的平台验证会话）。
> 三平台的命令、产物形态、实测数字与坑全部来自本机真实构建输出，非纸面推断。
> 上游标准：`docs/standards/architecture.md`（平台层 = cloudkit-platform）。

## 0. 平台支持总览

| | Windows | Linux | macOS |
|---|---|---|---|
| **编译** | ✅ 原生（主力开发平台） | ✅ 原生（CI 双矩阵之一） | ✅ 编译面通过（check + 完整链接均实测过，需交叉工具链） |
| **运行验证** | ✅ 全功能 | ✅ 实测（doctor + sftp 真连 + 单测 16/16） | ❓ 未实机验证（类型/链接过 ≠ 运行正确） |
| **盘符挂载** | ✅ winfsp（本地盘）+ WebDAV(net use) | ✅ gio → davfs2 链 | ❌ 未实现（`mount_webdav` 在计划 YAGNI 清单，stub 只保编译） |
| **凭据库** | Credential Manager | D-Bus Secret Service | Keychain（`apple-native` 已配） |
| **CI** | windows-latest | ubuntu-latest | （矩阵未含） |
| **cargo-dist 目标** | x86_64-pc-windows-msvc | x86_64-unknown-linux-gnu | （未声明） |

**代码面的平台事实**（改动相关文件时保持这些不变量）：

- `cloudkit-platform`：`windows.rs`（真实现）/`windows_stub.rs`（非 Windows 保编译）；`linux.rs`（`cfg(target_os = "linux")`，gio→davfs2 + davfs PID 文件处理）/`linux_stub.rs`。**macOS 走双 stub**——挂载面报 `Unsupported`，其余功能不受影响。
- `cloudkit-winfsp`：整体 `cfg(all(windows, feature = "winfsp"))` 门控，非 Windows 平台是空壳 crate，workspace 构建天然通过。
- keyring 三平台后端在 `cloudkit-cli/Cargo.toml` 的 `[target.'cfg(...)'.dependencies]` 分节声明（windows-native / apple-native / sync-secret-service+crypto-rust）。
- 挂载形态 `mount_backend`：webdav（全平台缺省）/ winfsp（仅 Windows + feature）/ Linux 的 webdav 挂载走 davfs2。

## 1. Windows（原生构建）

```powershell
# 常规（默认七驱动 telegram/baidu/local/sftp/pan115/pan123/webdav + winfsp 挂载；web 仪表盘/WebDAV 无条件编入）
cargo build --release -p cloudkit-cli

# 裁剪（K30 四 feature；缺驱动运行期报可行动错误 K31；--version 显示驱动清单 K32）
cargo build --release -p cloudkit-cli --no-default-features --features sftp            # 18 MB
cargo build --release -p cloudkit-cli --no-default-features --features sftp,winfsp     # + winfsp 挂载

# 尺寸优化档（K68：strip+fat LTO+单编译单元；opt-level=3 与 panic=unwind 不动）
cargo build --release -p cloudkit-cli --no-default-features --features sftp,winfsp --profile release-min   # 15 MB
```

- **winfsp 腿前置**：`LIBCLANG_PATH=D:/Python312/Lib/site-packages/clang/native`（winfsp-sys 的 bindgen 依赖；K89 起 winfsp 进 default，本仓 `.cargo/config.toml` 已内置该路径，命令行前缀可省）；**只能 MSVC 工具链**（对 gnu 工具链 panic——已知陷阱）。
- **winfsp 是 GPL 绑定**（**K89 起默认编译**，2026-09-25 负责人裁决）：默认二进制含 GPL 依赖（私有自用分发）；`--no-default-features` 裁剪构建仍 GPL-free。
- **exe 锁**：运行中的实例锁 `target/release/cydrive.exe`（替换报 os error 5）——先 `cydrive stop` 再 build。
- doctor 的 winfsp 行是 **feature 感知**文案（434a668）：编了 winfsp 说 "ready"，没编才提示重建。

## 2. Linux（原生构建，WSL2 实测）

```bash
# WSL2 Ubuntu-24.04（rustup stable + cc + dbus）；9p 慢——源码先拷到原生路径
cp -r /mnt/e/Rs_Codes/rs-cloudfs/crates Cargo.toml Cargo.lock ~/rs-cloudfs/
cd ~/rs-cloudfs && cargo build --release -p cloudkit-cli --features local,sftp
# → ELF 64-bit x86-64, 36 MB（未 strip）；四驱动清单齐全
```

实测验证链（2026-09-15）：

- `cydrive doctor` 对 u18 sftp 服务器：`[OK] sftp connectivity — connected, authenticated, and the host-key fingerprint matches`。
- `cargo test -p ck-sftp --lib`：**16/16**。
- workspace `cargo build`（含 winfsp 空壳 member）：绿，仅 4 条 winfsp dead-code 警告（非 Windows 必然，存量已知）。
- 挂载：gio → davfs2（`linux.rs`）；headless（无 D-Bus session）时 keyring 优雅降级为 WARN + 指引（写 config.toml 或 env）。
- 部署：`deploy/cydrive.service`、`deploy/cydrive-sync.service`（systemd 单元已备）。

## 3. macOS

### 3.1 编译面验证（无 Mac 也能做）

```bash
# WSL 里（Windows 侧 rustup 被镜像源挡时会 404，WSL 的 rustup 不受影响）
rustup target add x86_64-apple-darwin
cargo check --workspace --target x86_64-apple-darwin   # 30s 全绿：所有 cfg gate / keyring apple-native / unix 信号路径
```

### 3.2 完整链接出二进制（Zig + macOS SDK，实测成功）

**原理**：拦路的不是 Mac 硬件，是 Apple SDK（framework 头文件 + tbd 桩）。Zig 充当 C 交叉编译器（解决 bundled SQLite 的 `-arch` 等标志），SDK 补齐 Security/Foundation/CoreFoundation 等框架。

```bash
# ① Zig（0.13 实测）
curl -sL -o zig.tar.xz https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz
tar -xJf zig.tar.xz -C /opt && ln -sf /opt/zig-linux-x86_64-0.13.0/zig /usr/local/bin/zig

# ② macOS SDK（自行获取；见下方法律边界）
tar -xJf MacOSX14.5.sdk.tar.xz -C /opt/    # → /opt/MacOSX14.5.sdk

# ③ CC/链接器包装（四个坑的解都在这里）
cat > /tmp/zig-cc.sh <<'WRAP'
#!/bin/bash
SDK=/opt/MacOSX14.5.sdk
args=(--target=x86_64-macos -isysroot "$SDK" -F "$SDK/System/Library/Frameworks" -L "$SDK/usr/lib")
skip_next=0
for a in "$@"; do
  if [ $skip_next -eq 1 ]; then skip_next=0; continue; fi
  case "$a" in
    --target=x86_64-apple-macosx|--target=x86_64-apple-darwin|--target=x86_64-macos) ;;  # 统一已在行首
    -arch) skip_next=1 ;;            # 坑2：zig 不认 -arch，target 已含
    -lobjc|-liconv) ;;               # 坑4：SDK usr/lib 的 tbd 桩覆盖
    -nodefaultlibs) ;;               # zig 自管默认库
    *) args+=("$a") ;;
  esac
done
exec /opt/zig/zig cc "${args[@]}"
WRAP
chmod +x /tmp/zig-cc.sh

# ④ 构建
export CC_x86_64_apple_darwin=/tmp/zig-cc.sh
export CXX_x86_64_apple_darwin=/tmp/zig-cc.sh
export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=/tmp/zig-cc.sh
cargo build --release -p cloudkit-cli --features local,sftp --target x86_64-apple-darwin
# → Mach-O 64-bit x86_64 executable, 30 MB（实测 2026-09-15）
```

**四个坑**（按踩中顺序，均有解）：

1. cc-rs 传 `--target=x86_64-apple-macosx`，zig 只认 `x86_64-macos`——包装脚本改写；rustc 直调链接器时**不传 target**，包装脚本兜底补。
2. `-arch x86_64` / `-mmacosx-version-min` Linux cc 不认——换 zig + 丢弃 `-arch`。
3. framework 找不到——`-isysroot $SDK -F $SDK/System/Library/Frameworks`（zig 官方文档也确认 SDKROOT 是该问题的正解）。
4. 新版 SDK 不再随附 `/usr/lib/libobjc.A.dylib`（进了 dyld 共享缓存），tbd 引用解析失败——`-L $SDK/usr/lib` 让链接器找到 `libobjc.A.tbd`。

### 3.3 官方 Docker 路径（同原理的封装，未在本机走通）

```bash
docker run --rm -v $PWD:/io -w /io ghcr.io/rust-cross/cargo-zigbuild \
  cargo zigbuild --release --target x86_64-apple-darwin
```

镜像预装 macOS SDK + cargo-zigbuild（官方维护，省掉 ③ 的手工包装）。本机卡在 ghcr.io 拉取（WSL 内到该 registry 网络不通，Windows 侧代理 7897 在 WSL 不可达）——**网络通的环境下这是首选路径**。

### 3.4 必须知悉的边界（法律 + 签名）

- **Apple SDK EULA**：*"not to install, use or run the Apple SDKs on any non-Apple-branded computer"*——在 Linux/Windows 上使用 SDK 交叉编译严格讲违反其许可。实践上大量开源项目（含上述官方 Docker 镜像）在用，属普遍灰色地带；**自用风险低，对外分发需自行评估**。
- **产物未签名**：交叉编译出的 Mach-O 无代码签名，macOS Gatekeeper 会拦（用户需右键打开或 `xattr -d com.apple.quarantine`）。正式分发必须 Apple 开发者账号 + Mac 上签名公证——**这一步绕不过 Mac**。
- **运行未验证**：类型检查与链接通过 ≠ 运行正确（keyring Keychain 后端、unix 信号路径、时区等需实机确认）。挂载功能 macOS 本来就没有（stub）。
- 如需系统性支持 macOS：给 CI 矩阵加 `macos-latest` 一行即可获得真机构建 + 全量测试（成本最低的验证路径）。

## 4. 跨平台坑速查（实测沉淀）

| 坑 | 现象 | 解 |
|---|---|---|
| Windows rustup 镜像缺 apple target | `rustup target add x86_64-apple-darwin` 404（清华镜像无该组件） | 在 WSL 侧 rustup 操作（其镜像配置独立）；或换 `RUSTUP_DIST_SERVER` 官方源（注意系统级环境变量会盖过临时覆盖） |
| WSL 访问 Windows 代理 | `127.0.0.1:7897` 在 WSL 里指向 WSL 自身 → 000 | 代理监听 `0.0.0.0` 或用 Windows 主机 IP；ghcr.io 拉取因此失败 |
| 运行实例锁 release exe | 替换构建报 os error 5 | 先 `cydrive stop` |
| 9p 文件系统慢 | `/mnt/e` 上编译极慢 | 源码拷进 WSL 原生路径构建 |
| 共享 CARGO_TARGET_DIR 跨 worktree 污染 | 同名包 rmeta 互喂 → E0308 | worktree 用独立 target；merge 后 `cargo clean` 共享 target |
| winfsp-sys 构建失败 | bindgen 找不到 libclang / gnu 工具链 panic | `LIBCLANG_PATH` 指向 pip 的 clang/native；只用 MSVC |
| wsl.exe 直传复杂命令 | 引号吞噬/变量丢失 | 一律 `.sh` 脚本路线（写 Windows temp → `cp` 进 WSL → `sed -i 's/\r$//'` → bash 执行） |
| Git Bash MSYS 路径改写 | `/srv/...` 变 `C:/Program Files/Git/srv/...` | `MSYS_NO_PATHCONV=1` |
