#[cfg(not(target_os = "windows"))]
fn main() {
    tauri_build::build()
}

#[cfg(target_os = "windows")]
fn main() {
    ensure_webview2_loader();
    tauri_build::build()
}

/// 把 `WebView2Loader.dll` 复制到 `src-tauri/`，供 `tauri.windows.conf.json` 的 NSIS 资源声明使用。
///
/// `tauri-build` 在资源缺失时会直接让构建失败，而 `cargo check` / debug 构建不会生成
/// `target/release`，所以这里必须按当前 profile 的 `OUT_DIR` 定位 `webview2-com-sys`
/// 的输出目录；找不到时再回退到 cargo registry 中的 crate 源码。
#[cfg(target_os = "windows")]
fn ensure_webview2_loader() {
    use std::fs;
    use std::path::{Path, PathBuf};

    const DLL: &str = "WebView2Loader.dll";

    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") | Ok("arm64ec") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        other => {
            println!("cargo:warning=未知目标架构 {other:?}，跳过 {DLL} 复制");
            return;
        }
    };

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"));
    let dest = manifest_dir.join(DLL);

    let out_dir = std::env::var_os("OUT_DIR").map(PathBuf::from);
    // cargo 1.100 的目录布局是 <target>/<profile>/build/<pkg>/<hash>/out，
    // 旧布局是 <target>/<profile>/build/<pkg>-<hash>/out；统一向上找到 `build` 根。
    let build_root = out_dir.as_deref().and_then(find_build_root);
    let target_root = build_root
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf);

    let mut searched = Vec::new();
    let mut found = None;

    // 1. 当前 profile 的构建目录：<target>/<profile>/build/webview2-com-sys*/.../out/<arch>/
    if let Some(build_dir) = build_root {
        searched.push(build_dir.display().to_string());
        found = find_in_build_dir(build_dir, arch, DLL);
    }

    // 2. 兼容旧的固定路径与自定义 profile：<target>/{release,debug}/build/
    if found.is_none() {
        if let Some(target_root) = target_root.as_ref() {
            for profile in ["release", "debug"] {
                let build_dir = target_root.join(profile).join("build");
                searched.push(build_dir.display().to_string());
                if let Some(path) = find_in_build_dir(&build_dir, arch, DLL) {
                    found = Some(path);
                    break;
                }
            }
        }
    }

    // 3. 兜底：直接从 cargo registry 的 crate 源码目录取（crate 根目录自带各架构 DLL）
    if found.is_none() {
        if let Some(cargo_home) = cargo_home() {
            let registry_src = cargo_home.join("registry").join("src");
            searched.push(registry_src.display().to_string());
            found = find_in_registry(&registry_src, arch, DLL);
        }
    }

    let Some(src) = found else {
        println!(
            "cargo:warning=未找到 {DLL}（arch={arch}），已搜索：{}；NSIS 打包会失败",
            searched.join("; ")
        );
        return;
    };

    if files_equal(&src, &dest) {
        return;
    }

    match fs::copy(&src, &dest) {
        // 成功信息走 stderr：`cargo:warning` 会被 cargo 缓存并在后续构建中反复回放。
        Ok(_) => eprintln!("{DLL} copied from {}", src.display()),
        Err(err) => println!(
            "cargo:warning=复制 {DLL} 失败（{} -> {}）：{err}",
            src.display(),
            dest.display()
        ),
    }
}

#[cfg(target_os = "windows")]
fn find_build_root(out_dir: &std::path::Path) -> Option<&std::path::Path> {
    out_dir.ancestors().find(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy() == "build")
    })
}

#[cfg(target_os = "windows")]
fn find_in_build_dir(
    build_dir: &std::path::Path,
    arch: &str,
    dll: &str,
) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(build_dir).ok()?;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("webview2-com-sys")
        {
            continue;
        }
        let path = entry.path();
        let direct = path.join("out").join(arch).join(dll);
        if direct.is_file() {
            return Some(direct);
        }
        // 部分 cargo 布局会多一层 hash 目录：build/webview2-com-sys/<hash>/out
        if let Ok(nested) = std::fs::read_dir(&path) {
            for child in nested.flatten() {
                let candidate = child.path().join("out").join(arch).join(dll);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn find_in_registry(
    registry_src: &std::path::Path,
    arch: &str,
    dll: &str,
) -> Option<std::path::PathBuf> {
    let indexes = std::fs::read_dir(registry_src).ok()?;
    for index in indexes.flatten() {
        let Ok(crates) = std::fs::read_dir(index.path()) else {
            continue;
        };
        for krate in crates.flatten() {
            if !krate
                .file_name()
                .to_string_lossy()
                .starts_with("webview2-com-sys-")
            {
                continue;
            }
            let candidate = krate.path().join(arch).join(dll);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn cargo_home() -> Option<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Some(std::path::PathBuf::from(home));
    }
    let user_home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(std::path::PathBuf::from(user_home).join(".cargo"))
}

#[cfg(target_os = "windows")]
fn files_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
