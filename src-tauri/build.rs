#[cfg(target_os = "windows")]
use std::fs;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

fn main() {
    // 检测 Windows 平台，复制 WebView2Loader.dll 到 src-tauri/ 目录
    // tauri-build v2 在 MSVC 目标下不会自动打包此 DLL 到 NSIS 安装程序
    #[cfg(target_os = "windows")]
    {
        let build_dir = PathBuf::from("target/release/build");
        if let Ok(entries) = fs::read_dir(&build_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.to_string_lossy().contains("webview2-com-sys") {
                    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
                        Ok("x86_64") => "x64",
                        Ok("x86") => "x86",
                        Ok("aarch64") => "arm64",
                        _ => continue,
                    };
                    let dll_path = path.join("out").join(arch).join("WebView2Loader.dll");
                    if dll_path.exists() {
                        let _ = fs::copy(&dll_path, "WebView2Loader.dll");
                        println!("cargo:warning=WebView2Loader.dll copied to src-tauri/");
                        break;
                    }
                }
            }
        }
    }

    tauri_build::build()
}
