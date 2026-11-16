# Linux 支持说明

本文档描述 Kagami no Konata 在 Linux 上的支持等级、构建方式、运行依赖和已知限制。

## 支持等级

| 会话类型 | 支持等级 | 说明 |
|---|---|---|
| X11 / Xorg | 正式支持 | 悬浮窗定位、置顶、托盘菜单、全局快捷键、屏幕截图、鼠标键盘控制均按设计工作 |
| Wayland（GNOME / KDE） | 实验 / 降级 | 应用可启动，聊天、人设、记忆、文件工具和 Live2D 可用；GNOME 的绝对定位/置顶不可用；可选启用只读截图、XDG 门户全局快捷键、layer-shell 定位与 EIS 键鼠注入（均为实验，失败会明确回退） |
| Wayland（wlroots：Sway / Hyprland） | 实验 / 降级 | 可选只读截图依赖 wlr-screencopy；全局快捷键取决于桌面是否实现 `org.freedesktop.portal.GlobalShortcuts`；layer-shell 定位/拖拽通常可用；EIS 键鼠注入取决于 RemoteDesktop/ConnectToEIS 门户 |

> 如果你需要完整的桌面宠物体验，请在登录界面选择 **X11 / Xorg** 会话。Wayland 下应用会显示兼容性提示，不会把“不支持”伪装成“执行成功”。

## 构建依赖

以 Ubuntu / Debian 为例：

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential curl wget file \
  libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libxdo-dev libxcb1-dev libxrandr-dev \
  libdbus-1-dev libssl-dev libxkbcommon-dev patchelf rpm
```

说明：

- `libxdo-dev`：`enigo` 的 X11 键鼠后端。
- `libxkbcommon-dev`：`enigo` 的 libei/EIS 后端（Wayland 键鼠注入）在编译期需要。
- `libxcb1-dev` / `libxrandr-dev`：`screenshots` 的 X11 截图后端。
- `libayatana-appindicator3-dev`：系统托盘。
- `rpm`：仅在需要构建 `.rpm` 时安装。

> Wayland layer-shell 定位（P3.3）运行时才 `dlopen` `libgtk-layer-shell.so.0`，不参与编译依赖；需要该能力时再安装 `libgtk-layer-shell0`（Debian/Ubuntu）或 `gtk-layer-shell`（Fedora/Arch）。

还需要：

- Node.js 20.19+ 或 22.12+（推荐 22 LTS）
- pnpm
- Rust 1.87+

## 构建

```bash
pnpm install
pnpm build

# 只构建 deb + AppImage
pnpm tauri:build:linux

# 或显式指定格式
pnpm tauri build --bundles deb,appimage,rpm
```

产物位于：

```text
src-tauri/target/release/bundle/deb/
src-tauri/target/release/bundle/appimage/
src-tauri/target/release/bundle/rpm/
```

Windows 构建仍使用：

```bash
pnpm tauri:build:windows
```

## 运行依赖

`.deb` 已声明以下依赖，正常安装会自动拉取：

```text
libwebkit2gtk-4.1-0
libgtk-3-0
libayatana-appindicator3-1
libxdo3
libxcb1
libxrandr2
libdbus-1-3
xdg-utils
```

AppImage 不能可靠地打包 WebKitGTK，因此仍依赖系统提供 `webkit2gtk-4.1`。如果目标机器没有安装，请优先使用 `.deb`。

### AppImage 与 FUSE

部分发行版默认没有 FUSE 2：

- Ubuntu 22.04：安装 `libfuse2`
- Ubuntu 24.04：安装 `libfuse2t64`

无法安装 FUSE 时可以直接解包运行：

```bash
./Kagami-no-Konata_*.AppImage --appimage-extract-and-run
```

## 桌面环境注意事项

### GNOME

GNOME 默认不显示 AppIndicator 托盘图标，需要安装扩展：

```bash
sudo apt-get install gnome-shell-extension-appindicator
```

安装后在“扩展”应用中启用 **AppIndicator and KStatusNotifierItem Support**，然后重新登录。

如果托盘不可见，仍可通过以下方式恢复主窗口：

1. 重新启动应用（单实例会聚焦已有窗口）；
2. 在设置里把“关闭主窗口时”改为“退出程序”或“进入后台并打开悬浮窗”。

### 托盘行为

Linux 使用 AppIndicator / StatusNotifier。与 Windows 不同：

- 左键点击托盘图标通常直接打开菜单；
- `TrayIconEvent` 在 Linux 上不会触发；
- 菜单项（打开主窗口、悬浮窗、设置、退出）仍然可用。

因此 Linux 上不要依赖“左键单击切换窗口”，请使用托盘菜单或全局快捷键。

### 中文字体

最小化安装可能没有中文字体，界面会出现方框。推荐安装：

```bash
sudo apt-get install fonts-noto-cjk
```

应用样式已加入 Noto Sans CJK、Source Han Sans、WenQuanYi Micro Hei 等回退字体；如果仍然显示异常，请检查 `fc-list :lang=zh` 是否有输出。

## Wayland 限制

Wayland 不允许应用随意设置窗口绝对位置，也不提供通用的全局键鼠注入接口。当前版本在 Wayland 下：

| 功能 | 状态 |
|---|---|
| 主窗口 / 聊天 / 人设 / 记忆 | ✅ 可用 |
| 文件工具 / MCP / 网络工具 | ✅ 可用 |
| Live2D 悬浮窗 | ✅ 可显示、可拖拽 |
| 悬浮窗绝对定位 | ⚠️ 可选：`KONATA_WAYLAND_LAYER_SHELL=1`（仅 wlr-layer-shell，实验） |
| 悬浮窗置顶 | ⚠️ 随 layer-shell 一起启用（Top 层） |
| 托盘菜单 | ⚠️ 取决于桌面环境 |
| 全局快捷键 | ⚠️ 可选：`KONATA_WAYLAND_GLOBAL_SHORTCUTS=1`（XDG 门户，实验） |
| 屏幕截图 | ⚠️ 可选：`KONATA_WAYLAND_SCREENSHOT=1`（只读，实验） |
| 按需查看屏幕（`look_at_screen`） | ⚠️ 仅在只读截图启用时可用 |
| 桌面控制（os_control） | ⚠️ 可选：`KONATA_WAYLAND_INPUT=1`（EIS，实验，仅输入） |
| 窗口激活 | ❌ Wayland 下始终不可用 |

应用启动时会显示一次性提示。需要完整桌面能力时，请注销并选择 X11 / Xorg 会话。

### Wayland 只读截图（实验，P3.1）

默认关闭。显式设置 `KONATA_WAYLAND_SCREENSHOT=1` 并重启后：

```bash
KONATA_WAYLAND_SCREENSHOT=1 ./kagami-no-konata
```

- 只提供 `look_at_screen` 工具：模型按需截一张当前屏幕，图片在下一轮对话前注入；**不能**点击、输入或切换窗口。
- 依赖 `screenshots` 的 Wayland 后端（GNOME Shell DBus → `org.freedesktop.portal.Screenshot` → `wlr-screencopy`），因此需要 `DISPLAY`（XWayland 用于枚举显示器）和 `DBUS_SESSION_BUS_ADDRESS`；纯 Wayland 且关闭 XWayland 时能力探测直接返回不可用。
- 首次截图可能弹出系统授权框；单次截图 20 秒超时。超时、拒绝或后端失败都会明确报错，不会返回黑屏或旧图。
- 多显示器和分数缩放下，XWayland 的显示器几何信息可能与合成器实际布局不一致，截图/选屏可能不准确；X11 仍是正式支持基线。
- 同一时间只允许一个截图请求在途；上一次未结束时再次调用会被明确拒绝。

### Wayland 全局快捷键（实验，P3.2）

默认关闭。显式设置 `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1` 并重启后，应用会尝试通过 `org.freedesktop.portal.GlobalShortcuts` 绑定 `Ctrl+Shift+K`（显示/隐藏主窗口）：

```bash
KONATA_WAYLAND_GLOBAL_SHORTCUTS=1 ./kagami-no-konata
```

- 需要 Wayland 会话、`DBUS_SESSION_BUS_ADDRESS`，以及桌面环境实现 GlobalShortcuts 门户（GNOME / KDE Plasma 6 的较新版本；Sway / Hyprland 取决于 `xdg-desktop-portal` 后端）。
- 首次绑定通常会在系统弹窗中请求确认；**只有门户确认绑定后**，设置页才显示“可用（系统门户，实验）”。用户取消、桌面拒绝或会话关闭都会明确提示，不会假装成功。
- 实际触发方式由桌面环境管理，可能被用户在系统快捷键设置中修改；设置页会显示门户返回的触发描述。
- 绑定成功后由 Rust 后端直接监听 `Activated` 并切换主窗口，前端不再调用 `tauri-plugin-global-shortcut`，避免重复注册。
- 非沙箱应用需要向 `org.freedesktop.host.portal.Registry` 注册应用 ID。deb / rpm 包会安装隐藏的 `com.konata-mirror.main.desktop`，供 GNOME 50+/xdg-desktop-portal 1.21+ 解析应用身份；AppImage 内也包含该文件，但不会写入宿主机的 `/usr/share/applications`，若门户要求宿主桌面文件，请通过 AppImageLauncher 集成或手动复制。直接 `cargo tauri dev` 或手动运行未安装的二进制时同理。
- 门户不可用时，托盘菜单仍然可用；需要完整快捷键支持请改用 X11 / Xorg 会话。

### Wayland layer-shell 悬浮窗定位（实验，P3.3）

默认关闭。显式设置 `KONATA_WAYLAND_LAYER_SHELL=1` 并重启后，应用会尝试通过 `wlr-layer-shell` 把悬浮窗放到配置的角落，并支持自由拖拽：

```bash
KONATA_WAYLAND_LAYER_SHELL=1 ./kagami-no-konata
```

- 需要合成器实现 `zwlr_layer_shell_v1`。Sway / Hyprland 等 wlroots 系通常支持；**GNOME / Mutter 不支持**，KDE 等环境需实机确认。
- 通过运行时 `dlopen` 加载 `libgtk-layer-shell.so.0`，不硬链接依赖。库缺失、合成器不支持或窗口已经 realize 时，能力保持不可用并给出明确原因，应用正常启动。
- 安装运行库：Debian/Ubuntu `libgtk-layer-shell0`，Fedora `gtk-layer-shell`，Arch `gtk-layer-shell`。
- 初始化成功后，设置页显示“layer-shell（实验，可拖拽）”；悬浮窗位于 `Top` 层，高于普通窗口但低于全屏/锁屏，不预留工作区、不强制抢占键盘焦点。
- 拖拽由前端把指针增量发给后端，后端更新 layer-shell 的 left/top 边距；不会调用 `set_position`。拖拽平滑度取决于 WebKitGTK 在 Wayland 下能否持续提供指针增量，若拖拽不跟手请改用预设角落。
- v1 固定主显示器，多显示器/分数缩放下的位置仍需实机验证；X11 仍是正式支持基线。

### Wayland EIS 键鼠注入（实验，P3.4）

默认关闭。显式设置 `KONATA_WAYLAND_INPUT=1` 并重启后，应用会在独立线程中通过 `org.freedesktop.portal.RemoteDesktop` 请求用户授权，并用 `ConnectToEIS` + libei/EIS 注入键鼠：

```bash
KONATA_WAYLAND_INPUT=1 ./kagami-no-konata
```

- 需要 Wayland 会话、`DBUS_SESSION_BUS_ADDRESS`，以及桌面环境实现 RemoteDesktop 门户的 `ConnectToEIS`（版本 2+）。不支持时启动日志和设置页会给出明确原因，应用继续可用。
- 首次启动会弹出系统授权框；**连接成功前** `os_control` 不可用。用户拒绝、取消或会话关闭都会把能力写回 false 并提示。
- 只支持鼠标点击、键盘输入和组合键；**窗口激活始终不可用**，不要承诺已切换窗口。
- **没有可用截图时禁止鼠标点击**：P3.1 未启用或尚未截过屏时，`mouse_click` 会明确失败；键盘输入仍可使用。
- 每次 `os_control` 调用都必须单独审批；即使会话开启 AUTO，Wayland EIS 输入也不会跳过审批。
- 绝对坐标暂不支持负坐标（多显示器布局在主屏左侧/上方时会被拒绝）；需要完整多屏控制请改用 X11/Xorg 会话。

### XWayland（可选）

如果更在意窗口定位、截图和桌面控制，可以在 Wayland 会话中显式启用 XWayland：

```bash
KONATA_FORCE_X11=1 ./kagami-no-konata
```

应用会设置 `GDK_BACKEND=x11`（仅在用户未显式设置时），能力探测按 X11 处理。注意：

- 只覆盖本应用自己的窗口；其它原生 Wayland 窗口仍无法被截图或注入输入；
- 分数缩放 / HiDPI 下可能比原生 Wayland 模糊；
- 这是 opt-in，不会默认改变会话行为；
- `screenshots` 的截图后端按 `XDG_SESSION_TYPE` / `WAYLAND_DISPLAY` 选择 Wayland 或 X11，强制 XWayland 不一定能让它改走 X11 截图；若截图失败，请直接切换到 X11 会话。

### 关闭主窗口

Linux 新安装的默认行为是“进入后台并打开悬浮窗”（`hide_and_float`），避免在托盘不可见时应用完全失去入口。Windows 仍保持“进入后台”（`hide`）。可以在“设置 → 页面逻辑 → 关闭主窗口时”修改。


## NVIDIA / WebKitGTK

部分 NVIDIA 驱动 + WebKitGTK 组合会出现透明窗口黑屏、WebGL 异常或渲染卡死。应用检测到 `/proc/driver/nvidia/version` 时会自动设置 `WEBKIT_DISABLE_DMABUF_RENDERER=1`。如果仍然异常，可以尝试：

```bash
# 推荐：一次性打开全部兼容变量
KONATA_WEBKIT_SAFE=1 ./kagami-no-konata

# 或者手动逐项排查
WEBKIT_DISABLE_DMABUF_RENDERER=1 ./kagami-no-konata
WEBKIT_DISABLE_COMPOSITING_MODE=1 ./kagami-no-konata
__NV_DISABLE_EXPLICIT_SYNC=1 ./kagami-no-konata
```

这些变量只影响当前进程，不会修改系统配置。

## 故障排查

### 启动时报缺少 `libwebkit2gtk-4.1.so.0`

安装运行依赖：

```bash
sudo apt-get install libwebkit2gtk-4.1-0
```

### 启动时报缺少 `libxdo.so.3`

```bash
sudo apt-get install libxdo3
```

### 托盘图标不显示

- GNOME：安装并启用 AppIndicator 扩展；
- KDE / XFCE：确认状态通知器（StatusNotifierWatcher）正在运行；
- 也可以直接使用全局快捷键或重新启动应用恢复窗口。

### Wayland 全局快捷键不可用

- 确认启动时设置了 `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1`，且当前是 Wayland 会话、存在 `DBUS_SESSION_BUS_ADDRESS`；
- 确认桌面环境实现了 `org.freedesktop.portal.GlobalShortcuts`（GNOME / KDE Plasma 6 的较新版本；Sway / Hyprland 取决于 xdg-desktop-portal 后端）；
- 首次绑定需要在系统弹窗中确认；设置页显示“门户未就绪”时，查看平台说明里的具体失败原因；
- GNOME 50+/xdg-desktop-portal 1.21+ 需要应用 ID 对应已安装的 `.desktop` 文件。deb / rpm 包会安装 `com.konata-mirror.main.desktop`；AppImage 与未安装的二进制需要 AppImageLauncher 集成或手动复制该文件到 `/usr/share/applications/`；
- 仍不可用时请改用 X11/Xorg 会话，或使用托盘菜单显示/隐藏主窗口。

### 悬浮窗不能定位 / 不能拖拽

- 确认启动时设置了 `KONATA_WAYLAND_LAYER_SHELL=1`，且当前是 Wayland 会话；
- 确认合成器支持 `zwlr_layer_shell_v1`（GNOME/Mutter 不支持；Sway/Hyprland 通常支持；KDE 需实机确认）；
- 确认已安装 `libgtk-layer-shell.so.0`（Debian/Ubuntu `libgtk-layer-shell0`，Fedora `gtk-layer-shell`）；
- 设置页显示“layer-shell 未就绪”时，查看平台说明里的具体失败原因；应用不会假装定位成功；
- 拖拽不跟手时，可改用设置里的预设角落；X11 会话仍支持原生拖拽。

### Wayland EIS 键鼠注入不可用

- 确认启动时设置了 `KONATA_WAYLAND_INPUT=1`，且当前是 Wayland 会话、存在 `DBUS_SESSION_BUS_ADDRESS`；
- 确认桌面环境实现了 `org.freedesktop.portal.RemoteDesktop` 的 `ConnectToEIS`（版本 2+）；不支持时会在启动日志和设置页给出原因；
- 首次启动需要在系统弹窗中允许远程控制；拒绝或关闭会话后需要重启应用重新授权；
- 鼠标点击还要求先启用 `KONATA_WAYLAND_SCREENSHOT=1` 并成功截过屏；键盘输入不要求截图；
- `activate_window` 在 Wayland 下始终不可用；多显示器负坐标也不支持，请改用 X11/Xorg 会话。

### 悬浮窗透明区域是黑色

- 确认当前会话是 X11；
- 尝试 `WEBKIT_DISABLE_COMPOSITING_MODE=1`；
- 某些 NVIDIA 驱动需要同时设置 `__NV_DISABLE_EXPLICIT_SYNC=1`。

### 中文输入或界面方框

```bash
sudo apt-get install fonts-noto-cjk
```

### 截图失败

- 确认当前是 X11 会话；
- 确认已安装 `libxcb1`、`libxrandr2`；
- Wayland 下默认不截图；可设置 `KONATA_WAYLAND_SCREENSHOT=1` 启用只读实验截图（需要 XWayland 与 DBus 会话，首次可能要求系统授权），或尝试 `KONATA_FORCE_X11=1`（只覆盖 XWayland 窗口，原生 Wayland 窗口仍无法截取），或切换到 X11 会话。

## 打包配置说明

- `src-tauri/tauri.conf.json`：跨平台公共配置，`bundle.targets` 为 `"all"`。
- `src-tauri/tauri.windows.conf.json`：Windows 覆盖为 NSIS，并打包 `WebView2Loader.dll`。
- `src-tauri/tauri.linux.conf.json`：Linux 覆盖为 deb / AppImage / rpm，声明运行依赖，并额外安装 `com.konata-mirror.main.desktop`（隐藏条目，供 XDG 门户解析应用身份）。

平台配置文件由 Tauri CLI 根据目标平台自动合并，不需要手动切换。

## 环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `KONATA_FORCE_X11=1` | Wayland 会话下强制本应用使用 XWayland（设置 `GDK_BACKEND=x11`）。窗口定位、置顶、桌面控制按 X11 语义；截图后端是否跟随取决于 `XDG_SESSION_TYPE` / `WAYLAND_DISPLAY` | 关闭 |
| `KONATA_WAYLAND_SCREENSHOT=1` | Wayland 会话下启用只读实验截图（`look_at_screen`），需要 XWayland（`DISPLAY`）与 `DBUS_SESSION_BUS_ADDRESS`；不提供键鼠注入 | 关闭 |
| `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1` | Wayland 会话下尝试通过 XDG 门户绑定 `Ctrl+Shift+K`；需要 `DBUS_SESSION_BUS_ADDRESS`、桌面环境支持 GlobalShortcuts，并经过用户确认 | 关闭 |
| `KONATA_WAYLAND_LAYER_SHELL=1` | Wayland 会话下尝试用 `wlr-layer-shell` 定位悬浮窗（自由拖拽 + Top 层置顶）；需要合成器支持与 `libgtk-layer-shell.so.0` | 关闭 |
| `KONATA_WAYLAND_INPUT=1` | Wayland 会话下通过 XDG RemoteDesktop + libei/EIS 注入键鼠；需要门户授权，仅输入、不支持窗口激活；鼠标点击要求先启用只读截图 | 关闭 |
| `KONATA_WEBKIT_SAFE=1` | WebKitGTK 兼容模式：`WEBKIT_DISABLE_DMABUF_RENDERER=1`、`WEBKIT_DISABLE_COMPOSITING_MODE=1`、`__NV_DISABLE_EXPLICIT_SYNC=1` | 关闭 |
| `KONATA_SMOKE_TEST=1` | 启动 2 秒后自动退出，供 CI / 打包冒烟测试使用 | 关闭 |

已经显式设置的同名变量不会被覆盖。检测到 NVIDIA 驱动时，应用会自动设置 `WEBKIT_DISABLE_DMABUF_RENDERER=1`；仍然异常时再使用 `KONATA_WEBKIT_SAFE=1`。

## 安全加固（Linux）

- `config.json` 含 API Key：Unix 上配置目录权限为 `0700`，配置文件（包括原子写入的临时文件）为 `0600`；旧版本留下的宽权限会在加载时就地收紧。
- 文件工具的默认拒绝清单在 Linux 上额外包含密钥环（keyrings）、KWallet、password-store、gcloud / gh / 1Password / SOPS 配置和浏览器登录数据。这些是内置条目，不能从设置里移除。
- 命令工具会拒绝 `-I/etc`、`-Wl,-rpath,/path`、`--flag=/path` 这类把工作区外路径粘在开关里的写法；子进程只继承显示会话所需的白名单环境变量，不继承 API Key / Token。
- `run_command` 超时或点击“停止”时会回收整个 Unix 进程组，避免 `cargo` / `npm` 拉起的 rustc / node 变成孤儿进程。
- Wayland EIS 键鼠注入（P3.4）默认关闭，必须显式 opt-in 并经过系统门户授权；每次 `os_control` 调用都单独审批，**永不**被会话 AUTO 或免审批名单跳过；没有可用截图时禁止鼠标点击。

## Wayland 后续计划（P3）

P3 采用运行时 opt-in + 能力探测，默认关闭；重型/实验依赖按需 feature gate。X11 仍是正式支持基线。

- **截图（P3.1，已实现，实验）**：`KONATA_WAYLAND_SCREENSHOT=1` 启用只读截图，新增 `look_at_screen` 工具；只解决“看”，不解决键鼠注入。需要 XWayland 与 DBus 会话，首次可能要求系统授权。
- **全局快捷键（P3.2，已实现，实验）**：`KONATA_WAYLAND_GLOBAL_SHORTCUTS=1` 启用 `org.freedesktop.portal.GlobalShortcuts`；绑定成功后才把能力标为可用，失败/取消/会话关闭都会明确提示。
- **悬浮窗定位 / 置顶（P3.3，已实现，实验）**：`KONATA_WAYLAND_LAYER_SHELL=1` 通过运行时 `dlopen` 使用 `wlr-layer-shell`，支持自由拖拽与 Top 层置顶；GNOME 不支持，初始化失败时明确回退。
- **键鼠注入（P3.4，已实现，实验）**：`KONATA_WAYLAND_INPUT=1` 通过 XDG RemoteDesktop + libei/EIS 注入键鼠；仅输入、不支持窗口激活，没有截图时禁止鼠标点击，审批不可被 AUTO 绕过。

P3.1–P3.4 依赖 Linux 实机验证，当前版本不会做“看起来成功”的降级。GNOME / KDE Wayland 与 Sway 的实际门户支持情况仍需在对应实机上验证。


