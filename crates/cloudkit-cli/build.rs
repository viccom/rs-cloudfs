use std::process::Command;

/// One git subprocess answer, trimmed; `None` on any failure (git absent,
/// not a repo, non-UTF-8) — every consumer degrades, nothing panics in a
/// build script.
fn git_out(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `x.y.z` → the comparable triple (an optional `v` prefix and a
/// `-suffix` are tolerated). `None` for anything else — the caller then
/// keeps the crate version instead of guessing.
fn semver_triple(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.trim_start_matches('v').split('.');
    let mut next = || {
        parts
            .next()?
            .split('-')
            .next()?
            .split('+')
            .next()?
            .parse::<u64>()
            .ok()
    };
    let triple = (next()?, next()?, next()?);
    parts.next().is_none().then_some(triple)
}

fn main() {
    // `cydrive.exe` is the artifact that actually links winfsp-sys: the
    // WinFsp DLL import has to be delay-loaded for the binary to start on
    // a machine where `winfsp-x64.dll` is not on the search path (it
    // lives in the WinFsp install dir, which is not on PATH). rustc-link-arg
    // does not travel across library boundaries, so every crate that
    // produces a linkable artifact in the winfsp graph emits these two
    // lines itself — cloudkit-winfsp/build.rs does the same for its tests.
    // See phase 3 K38/K40 (docs/plans/2026-09-10-phase3-winfsp.md).
    //
    // Hand-written instead of winfsp::build::winfsp_link_delayload():
    // that helper sits behind the winfsp `delayload` feature, and an
    // optional build-dependency cannot be cfg'd from within the build
    // script (build scripts do not see their own package's features).
    //
    // Until WF4 gives the CLI an actual call into cloudkit-winfsp, the
    // link prints `LNK4199: /DELAYLOAD:winfsp-x64.dll ignored; no
    // imports found` — that warning is the *correct* report of an exe
    // with no WinFsp import yet, not a broken link.
    if std::env::var_os("CARGO_FEATURE_WINFSP").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }

    // ---- build-time git identity (K32 版本面 v2, 2026-09-24) ----
    //
    // `--version` 的身份段 = `<版本>-<短哈希>[-dirty]`。版本段取
    // **git tag 与 crate 版本的 semver 较新者**：tag 是发布标记，但本仓
    // tag 血统停在 0.7.2（fork 前），严格用 tag 会把 0.10.0 的构建降显
    // 成 0.7.2——取新者让 tag 追上后自然接管。零依赖纪律：走 git 子进程
    // 而非 vergen（其 git2 路线拖 libgit2 C 依赖）；git 不可用/非仓
    // （tarball 导出）时回退裸 crate 版本，绝不 fail build。
    let crate_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into());
    let identity = match (
        git_out(&["describe", "--tags", "--abbrev=0"]),
        git_out(&["rev-parse", "--short=12", "HEAD"]),
    ) {
        (Some(tag), Some(hash)) => {
            let base = match (semver_triple(&tag), semver_triple(&crate_version)) {
                (Some(tag_v), Some(crate_v)) if tag_v > crate_v => tag,
                _ => crate_version.clone(),
            };
            // `-uno`：`git describe --dirty` 同款口径——只看已跟踪改动。
            // 裸 porcelain 会把常驻未跟踪文件（.zcodeignore 等）永久
            // 误标 dirty（81cc175 提交后实跑揭出）。
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .map(|out| !out.stdout.is_empty())
                .unwrap_or(false);
            format!("{base}-{hash}{}", if dirty { "-dirty" } else { "" })
        }
        _ => crate_version,
    };
    println!("cargo:rustc-env=CYDRIVE_GIT_VERSION={identity}");

    // ---- 嵌入值的新鲜度 ----
    //
    // 显式列出 rerun-if-changed 即**替代** cargo 的默认策略（包内任意
    // 文件变更重跑），因此必须同时覆盖两类翻转：
    // ①哈希/引用变化——git 管理文件（HEAD、分支 ref、packed-refs、
    //   refs/tags）；
    // ②dirty 翻转——`src/`（源码编辑不改 git 元数据；只盯 .git 会嵌
    //   陈旧 dirty 位）。
    // worktree 下 `.git` 是文件，故路径全部经 `--absolute-git-dir` /
    // `--git-common-dir` 解析而非拼字符串。仍有的缺口（如实记录）：
    // 非 `src/` 的包内文件（Cargo.toml 等）单独编辑会翻 dirty 而不重跑
    // ——下次任何 src 编辑或提交都会对齐。
    if let Some(git_dir) = git_out(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        if let Ok(head) = std::fs::read_to_string(format!("{git_dir}/HEAD")) {
            if let Some(reference) = head.strip_prefix("ref: ") {
                let reference = reference.trim();
                if let Some(common) = git_out(&["rev-parse", "--git-common-dir"]) {
                    // `--git-common-dir` 可能回相对路径（相对 build.rs 的
                    // cwd=包目录）——canonicalize 锚成绝对，rerun-if-changed
                    // 才指对地方。
                    let common = std::fs::canonicalize(&common)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or(common);
                    // refs/heads 与 packed-refs/refs/tags 住在 common dir
                    //（worktree 共享）。
                    println!("cargo:rerun-if-changed={common}/{reference}");
                    println!("cargo:rerun-if-changed={common}/packed-refs");
                    println!("cargo:rerun-if-changed={common}/refs/tags");
                }
            }
        }
    }
    println!("cargo:rerun-if-changed=src");
}
