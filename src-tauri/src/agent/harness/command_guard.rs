use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::types::ToolConfig;

use super::jail::WorkspaceSet;

/// 硬编码的敏感程序黑名单
///
/// **优先级高于用户配置**：即使用户把 `cmd` 加进允许列表，这里依然会拦截。
/// 用户配置只能收窄可执行集合，永远不能放宽黑名单。
const DENY_PROGRAMS: &[&str] = &[
    // shell 与脚本宿主
    "cmd", "command", "powershell", "pwsh", "wscript", "cscript", "mshta", "rundll32",
    "regsvr32", "installutil", "mofcomp", "bash", "sh", "zsh", "dash", "ksh", "fish", "csh",
    "tcsh", "osascript", "expect",
    // 系统/服务/注册表/计划任务
    "reg", "regedit", "regini", "sc", "net", "net1", "netsh", "schtasks", "at", "taskkill",
    "tasklist", "wmic", "wevtutil", "gpupdate", "gpresult", "bcdedit", "vssadmin", "fsutil",
    "diskpart", "format", "chkdsk", "defrag", "cacls", "icacls", "takeown", "attrib", "subst",
    "runas", "sudo", "doas", "pkexec", "su", "psexec", "psexesvc", "wsl", "wslconfig",
    // 关机/重启/破坏性
    "shutdown", "restart", "reboot", "halt", "poweroff", "init", "mkfs", "dd", "fdisk", "parted",
    "shred", "wipefs", "mount", "umount", "systemctl", "service", "launchctl", "kill", "killall",
    "pkill", "rm", "rmdir", "del", "erase", "rd", "mv", "move", "ren", "rename", "copy", "xcopy",
    "robocopy", "chmod", "chown", "chgrp", "ln", "mklink", "truncate",
    // 网络下载 / 外联 / 隧道
    "curl", "wget", "ftp", "sftp", "scp", "ssh", "sshd", "telnet", "nc", "ncat", "netcat", "socat",
    "bitsadmin", "certutil", "certreq", "makecab", "expand", "extrac32", "ftpget", "tftp",
    "nslookup", "dig", "host", "ping", "tracert", "arp", "ipconfig", "ifconfig", "route",
    // 包管理器之外的"安装器"与远程执行
    "winget", "choco", "scoop", "msiexec", "install", "uninstall",
    // 环境变量/进程注入
    "setx", "set", "export", "env", "printenv", "eval", "exec", "source",
];

/// 这些扩展名的程序本身就是脚本/快捷方式，直接执行等于绕过白名单
const DENY_EXTENSIONS: &[&str] = &[
    "bat", "cmd", "com", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi", "msp",
    "scr", "pif", "lnk", "reg", "inf", "hta", "cpl", "jar", "application",
];

/// 解释器：禁止用"求值参数"把一行代码直接喂进去
const INTERPRETERS: &[&str] = &[
    "python", "python3", "pythonw", "node", "nodejs", "deno", "bun", "perl", "ruby", "php", "lua",
    "luajit", "rscript", "groovy", "scala", "julia", "elixir", "iex",
];

/// 解释器的求值类参数（一律拒绝）
const EVAL_FLAGS: &[&str] = &[
    "-c", "-e", "--eval", "--exec", "--execute", "-p", "--print", "-i", "--interactive", "-r",
    "--require", "--import", "-Command", "-EncodedCommand", "-eC", "/c", "/k",
];

/// `python -m` / `node --eval` 之类：模块名必须落在安全清单内
const SAFE_MODULES: &[&str] = &[
    "pytest", "unittest", "json.tool", "compileall", "venv", "black", "ruff", "mypy", "flake8",
    "isort", "pip.index", "py_compile",
];

/// 参数中不允许出现的敏感路径片段
const SENSITIVE_PATH_FRAGMENTS: &[&str] = &[
    ".ssh", ".aws", ".gnupg", ".kube", "id_rsa", "id_ed25519", "id_ecdsa", ".netrc",
    "appdata/roaming", "appdata\\roaming", "appdata/local", "appdata\\local", "/etc/passwd",
    "/etc/shadow", "/etc/sudoers", "config.json", "credentials", "data.db", ".env",
    "authorized_keys", "known_hosts", ".git-credentials", "cookies", "login data",
];

/// 参数中不允许出现的危险开关
const DENY_ARG_FLAGS: &[&str] = &[
    "--no-preserve-root",
    "--global",
    "--system",
    "-runas",
    "-encodedcommand",
    "--dangerously-skip-permissions",
    "--yolo",
];

const MAX_ARGS: usize = 64;
const MAX_ARG_CHARS: usize = 4096;
const MAX_PROGRAM_CHARS: usize = 512;

/// 已通过全部检查、可以安全执行的命令
#[derive(Debug, Clone)]
pub struct CheckedCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// 人类可读的完整命令（用于 UI 与落库）
    pub display: String,
}

/// 命令守卫：白名单 + 硬黑名单 + 参数审查 + 目录约束
pub struct CommandGuard {
    allow: HashSet<String>,
}

impl CommandGuard {
    /// 从配置构造（测试与外部调用用；工具内部走 `from_allowlist`）
    #[allow(dead_code)]
    pub fn new(cfg: &ToolConfig) -> Self {
        Self::from_allowlist(&cfg.command_allowlist)
    }

    /// 从允许列表构造（内部会合并内置默认列表并剔除硬黑名单）
    pub fn from_allowlist(items: &[String]) -> Self {
        let mut allow: HashSet<String> = items
            .iter()
            .map(|p| normalize_program(p))
            .filter(|p| !p.is_empty())
            .collect();
        // 内置默认允许列表始终有效，用户即使清空配置也不会把常用开发工具一起清掉
        for p in crate::config::types::DEFAULT_COMMAND_ALLOWLIST {
            allow.insert(normalize_program(p));
        }
        // 黑名单优先级最高：从允许集合中剔除
        for p in DENY_PROGRAMS {
            allow.remove(*p);
        }
        Self { allow }
    }

    /// 允许列表（用于 UI 展示与错误提示）
    #[allow(dead_code)]
    pub fn allowed_programs(&self) -> Vec<String> {
        let mut list: Vec<String> = self.allow.iter().cloned().collect();
        list.sort();
        list
    }

    /// 是否被硬黑名单拦截（供 UI 显示"敏感命令已禁用"）
    #[allow(dead_code)]
    pub fn is_sensitive(program: &str) -> bool {
        let base = normalize_program(program);
        DENY_PROGRAMS.contains(&base.as_str())
            || extension_of(program)
                .map(|ext| DENY_EXTENSIONS.contains(&ext.as_str()))
                .unwrap_or(false)
    }

    pub fn check(
        &self,
        program: &str,
        args: &[String],
        cwd_raw: Option<&str>,
        workspaces: &WorkspaceSet,
    ) -> Result<CheckedCommand, String> {
        // ─── 1. 程序名静态检查（先做，保证拒绝决策不依赖宿主环境） ───
        let program = program.trim();
        if program.is_empty() {
            return Err("program 不能为空".to_string());
        }
        if program.chars().count() > MAX_PROGRAM_CHARS {
            return Err("program 过长".to_string());
        }
        if program.contains('\0') {
            return Err("program 包含非法字符".to_string());
        }
        if let Some(ext) = extension_of(program) {
            if DENY_EXTENSIONS.contains(&ext.as_str()) {
                return Err(format!(
                    "敏感命令已被阻止：不允许执行 .{} 脚本/快捷方式（请使用受信任的可执行程序）",
                    ext
                ));
            }
        }

        let base = normalize_program(program);
        if DENY_PROGRAMS.contains(&base.as_str()) {
            return Err(format!(
                "敏感命令已被阻止：{}（系统/网络/破坏性命令不在允许范围内）",
                base
            ));
        }
        if !self.allow.contains(&base) {
            return Err(format!(
                "程序「{}」不在允许列表中。可在「设置 → 工具 → 命令允许列表」中添加（敏感命令无法添加）",
                base
            ));
        }

        // ─── 2. 参数静态检查 ───
        if args.len() > MAX_ARGS {
            return Err(format!("参数过多（上限 {} 个）", MAX_ARGS));
        }
        for arg in args {
            if arg.contains('\0') {
                return Err("参数包含非法字符".to_string());
            }
            if arg.chars().count() > MAX_ARG_CHARS {
                return Err("单个参数过长".to_string());
            }
        }
        check_sensitive_args(&base, args)?;

        // ─── 3. 解析工作目录 ───
        let cwd = match cwd_raw {
            Some(raw) if !raw.trim().is_empty() => workspaces.resolve_dir(raw)?.abs_path,
            _ => workspaces.default_path(),
        };
        if !cwd.is_dir() {
            return Err(format!("工作目录不可用：{}", cwd.display()));
        }

        // ─── 4. 解析程序真实路径 ───
        let program_path = resolve_program(program, &cwd, workspaces)?;

        // ─── 5. 参数中的路径必须在工作区内 ───
        check_path_args(args, &cwd, workspaces)?;

        let display = std::iter::once(program_path.display().to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");

        Ok(CheckedCommand {
            program: program_path,
            args: args.to_vec(),
            cwd,
            env: sanitized_env(),
            display,
        })
    }
}

fn normalize_program(program: &str) -> String {
    // 统一分隔符后再取末段：这样 `C:\\Windows\\System32\\cmd.exe` 在任意平台上
    // 都会归一化成 `cmd`，黑名单不会因为平台差异被绕过
    let trimmed = program.trim().trim_matches('"').replace('\\', "/");
    let base = trimmed.rsplit('/').next().unwrap_or(&trimmed).to_string();
    let lower = base.to_ascii_lowercase();
    // 去掉可执行后缀，"python.exe" 与 "python" 视为同一个程序
    for ext in [".exe", ".com", ".cmd", ".bat", ".ps1"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    lower
}

fn extension_of(program: &str) -> Option<String> {
    let normalized = program.trim().trim_matches('"').replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or(&normalized);
    base.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|e| !e.is_empty())
}

/// 解释器求值参数、敏感路径片段、危险开关的统一检查
fn check_sensitive_args(base: &str, args: &[String]) -> Result<(), String> {
    let is_interp = INTERPRETERS.contains(&base);
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let lower = arg.to_ascii_lowercase();

        if DENY_ARG_FLAGS.contains(&lower.as_str()) {
            return Err(format!("参数「{}」属于敏感开关，已被阻止", arg));
        }

        // 解释器求值参数：python -c / node -e / ruby -e ...
        if is_interp {
            if EVAL_FLAGS.contains(&arg) {
                return Err(format!(
                    "已阻止解释器求值参数「{}」：不允许把代码字符串直接交给 {} 执行",
                    arg, base
                ));
            }
            // -m / --module 只允许安全模块
            if arg == "-m" || arg == "--module" {
                let module = args.get(i + 1).ok_or_else(|| "-m 缺少模块名".to_string())?;
                if !SAFE_MODULES.contains(&module.as_str()) {
                    return Err(format!(
                        "已阻止模块「{}」：{} -m 只允许白名单模块（{}）",
                        module,
                        base,
                        SAFE_MODULES.join(", ")
                    ));
                }
            }
        }

        // 特权提升关键字
        let bare = lower.trim_start_matches('-');
        if matches!(bare, "sudo" | "runas" | "doas" | "pkexec" | "su") {
            return Err(format!("已阻止特权提升参数「{}」", arg));
        }

        // 敏感路径片段
        let normalized = lower.replace('\\', "/");
        for frag in SENSITIVE_PATH_FRAGMENTS {
            if normalized.contains(&frag.replace('\\', "/")) {
                return Err(format!(
                    "已阻止参数「{}」：涉及敏感路径「{}」",
                    arg, frag
                ));
            }
        }

        i += 1;
    }
    Ok(())
}

/// 参数中的绝对路径与 `..` 逃逸检查
fn check_path_args(args: &[String], cwd: &Path, workspaces: &WorkspaceSet) -> Result<(), String> {
    for arg in args {
        if arg.starts_with('-') && !arg.contains('=') {
            continue; // 纯开关
        }
        // 支持 `--flag=/abs/path` 形式
        let candidate = arg.split_once('=').map(|(_, v)| v).unwrap_or(arg.as_str());
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }

        let looks_absolute = candidate.starts_with('/')
            || candidate.starts_with('~')
            || candidate.starts_with(r"\\")
            || is_windows_absolute(candidate);

        if looks_absolute {
            if candidate.starts_with('~') {
                return Err(format!("已阻止参数「{}」：不允许访问用户主目录", arg));
            }
            match workspaces.resolve(candidate) {
                Ok(_) => {}
                Err(e) => {
                    return Err(format!("已阻止参数「{}」：{}", arg, e));
                }
            }
            continue;
        }

        // 相对路径：不允许通过 .. 逃出工作目录
        if candidate.contains("..") {
            let mut probe = cwd.to_path_buf();
            for comp in Path::new(candidate).components() {
                match comp {
                    std::path::Component::Normal(c) => probe.push(c),
                    std::path::Component::ParentDir => {
                        probe.pop();
                        if !workspaces
                            .list()
                            .iter()
                            .any(|r| probe.starts_with(&r.path))
                        {
                            return Err(format!(
                                "已阻止参数「{}」：路径越出工作区",
                                arg
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes: Vec<char> = value.chars().collect();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == ':'
        && (bytes[2] == '\\' || bytes[2] == '/')
}

/// 解析程序路径：带路径分隔符时必须在工作区内，否则在 PATH 中查找
fn resolve_program(
    program: &str,
    cwd: &Path,
    workspaces: &WorkspaceSet,
) -> Result<PathBuf, String> {
    let has_separator = program.contains('/') || program.contains('\\');

    if has_separator {
        let candidate = if Path::new(program).is_absolute() {
            PathBuf::from(program)
        } else {
            cwd.join(program)
        };
        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|_| format!("找不到可执行文件：{}", candidate.display()))?;
        let inside = workspaces
            .list()
            .iter()
            .any(|r| canonical.starts_with(&r.path));
        if !inside {
            return Err(format!(
                "可执行文件必须位于工作区内：{}",
                canonical.display()
            ));
        }
        return Ok(canonical);
    }

    // PATH 查找
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let mut names = vec![program.to_string()];
    if cfg!(windows) {
        let pathext = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for ext in pathext.split(';') {
            let ext = ext.trim().to_ascii_lowercase();
            if ext.is_empty() {
                continue;
            }
            if !program.to_ascii_lowercase().ends_with(&ext) {
                names.push(format!("{}{}", program, ext));
            }
        }
    }

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in &names {
            let full = dir.join(name);
            if full.is_file() {
                // 命中的如果是不允许的脚本类型，直接拒绝
                if let Some(ext) = extension_of(&full.display().to_string()) {
                    if DENY_EXTENSIONS.contains(&ext.as_str()) {
                        continue;
                    }
                }
                return Ok(full);
            }
        }
    }

    Err(format!(
        "在 PATH 中找不到程序「{}」。可填写工作区内的相对路径（如 ./scripts/build.sh 需先加入允许列表）",
        program
    ))
}

/// 只传递白名单环境变量，绝不把 API Key / Token 交给子进程
pub fn sanitized_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH",
        "PATHEXT",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "TEMP",
        "TMP",
        "TMPDIR",
        "HOME",
        "USERPROFILE",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    ];

    let mut env: Vec<(String, String)> = Vec::new();
    for key in KEEP {
        if let Ok(value) = std::env::var(key) {
            if is_secret_like(key) {
                continue; // 防御性：将来有人往 KEEP 里加了敏感项也会被拦下
            }
            env.push((key.to_string(), value));
        }
    }
    env
}

fn is_secret_like(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ToolConfig;

    fn test_set() -> (WorkspaceSet, tempdir::Temp) {
        let tmp = tempdir::Temp::new("cmdguard");
        let cfg = ToolConfig::with_single_root(tmp.path(), true, "测试");
        (WorkspaceSet::from_config(&cfg, tmp.path()), tmp)
    }

    /// 极简临时目录（避免引入额外依赖）
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Temp(PathBuf);
        impl Temp {
            pub fn new(tag: &str) -> Self {
                let dir = std::env::temp_dir()
                    .join(format!("konata-{}-{}", tag, uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn guard() -> CommandGuard {
        CommandGuard::new(&ToolConfig::default())
    }

    #[test]
    fn blocks_sensitive_programs_even_if_user_allowlists_them() {
        let mut cfg = ToolConfig::default();
        cfg.command_allowlist.push("cmd".to_string());
        cfg.command_allowlist.push("certutil".to_string());
        cfg.command_allowlist.push("curl".to_string());
        cfg.command_allowlist.push("rm".to_string());
        let guard = CommandGuard::new(&cfg);

        let (ws, _tmp) = test_set();
        for program in ["cmd", "cmd.exe", "C:\\Windows\\System32\\cmd.exe", "certutil", "curl", "rm", "powershell", "shutdown", "reg", "schtasks"] {
            let err = guard.check(program, &[], None, &ws).unwrap_err();
            assert!(err.contains("敏感命令已被阻止"), "{program} => {err}");
        }
    }

    #[test]
    fn blocks_script_extensions() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        for program in ["build.bat", "run.ps1", "evil.vbs", "x.js", "setup.msi", "a.lnk"] {
            let err = guard.check(program, &[], None, &ws).unwrap_err();
            assert!(
                err.contains("敏感命令已被阻止") || err.contains("不允许执行"),
                "{program} => {err}"
            );
        }
    }

    #[test]
    fn blocks_unknown_programs() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        let err = guard.check("totally-unknown-tool", &[], None, &ws).unwrap_err();
        assert!(err.contains("不在允许列表中"), "{err}");
    }

    #[test]
    fn blocks_interpreter_eval_flags() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        let cases = [
            ("python", vec!["-c".to_string(), "import os".to_string()]),
            ("python3", vec!["-c".to_string(), "print(1)".to_string()]),
            ("node", vec!["-e".to_string(), "require('fs')".to_string()]),
            ("ruby", vec!["-e".to_string(), "puts 1".to_string()]),
            ("php", vec!["-r".to_string()]),
        ];
        for (program, args) in cases {
            let err = guard.check(program, &args, None, &ws).unwrap_err();
            assert!(err.contains("求值参数"), "{program} {args:?} => {err}");
        }
    }

    #[test]
    fn blocks_unsafe_python_module() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        let err = guard.check(
            "python",
            &["-m".to_string(), "pip".to_string(), "install".to_string()],
            None,
            &ws,
        )
        .unwrap_err();
        assert!(err.contains("模块"), "{err}");

        // 白名单模块允许
        let ok = guard.check(
            "python",
            &["-m".to_string(), "pytest".to_string()],
            None,
            &ws,
        );
        assert!(ok.is_ok(), "{:?}", ok.err());
    }

    #[test]
    fn blocks_privilege_escalation_args() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        let err = guard
            .check("git", &["-runas".to_string()], None, &ws)
            .unwrap_err();
        assert!(err.contains("敏感"), "{err}");
    }

    #[test]
    fn blocks_sensitive_path_arguments() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        for arg in [
            "/home/user/.ssh/id_rsa",
            "C:\\Users\\x\\AppData\\Roaming\\com.konata\\config.json",
            ".env",
        ] {
            let err = guard
                .check("cat", &[arg.to_string()], None, &ws)
                .unwrap_err();
            assert!(err.contains("敏感") || err.contains("阻止"), "{arg} => {err}");
        }
    }

    #[test]
    fn blocks_absolute_paths_outside_workspace() {
        let guard = guard();
        let (ws, _tmp) = test_set();
        let outside = if cfg!(windows) { "C:\\Windows\\win.ini" } else { "/etc/hosts" };
        let err = guard
            .check("cat", &[outside.to_string()], None, &ws)
            .unwrap_err();
        assert!(err.contains("阻止"), "{err}");
    }

    #[test]
    fn blocks_parent_escape_argument() {
        let guard = guard();
        let (ws, tmp) = test_set();
        let err = guard
            .check("ls", &["../../".to_string()], None, &ws)
            .unwrap_err();
        assert!(err.contains("越出工作区"), "{err}");
        drop(tmp);
    }

    #[test]
    fn allows_safe_command_with_relative_argument() {
        let guard = CommandGuard::new(&ToolConfig::default());
        let (ws, _tmp) = test_set();
        std::fs::create_dir_all(ws.default_path().join("src")).unwrap();
        let checked = guard
            .check("git", &["status".to_string(), "src".to_string()], None, &ws)
            .expect("git status 应当被允许");
        assert_eq!(checked.cwd, ws.default_path());
        assert!(checked.env.iter().all(|(k, _)| !is_secret_like(k)));
    }

    #[test]
    fn environment_is_scrubbed() {
        std::env::set_var("KONATA_TEST_API_KEY", "sk-secret");
        let env = sanitized_env();
        assert!(env.iter().all(|(k, _)| k != "KONATA_TEST_API_KEY"));
        assert!(env.iter().all(|(k, _)| !k.contains("KEY")));
        std::env::remove_var("KONATA_TEST_API_KEY");
    }

    #[test]
    fn sensitive_detection_helper() {
        assert!(CommandGuard::is_sensitive("cmd.exe"));
        assert!(CommandGuard::is_sensitive("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"));
        assert!(CommandGuard::is_sensitive("script.bat"));
        assert!(!CommandGuard::is_sensitive("cargo"));
    }
}
