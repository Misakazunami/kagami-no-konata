mod agent;
mod appdata;
mod commands;
mod config;
mod llm;
mod memory;
mod persona;
mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

use agent::chat_agent::ChatAgent;
use agent::dispatcher::AgentDispatcher;
use agent::harness::approve::{new_approval_map, ApprovalMap};
use agent::harness::tools::builtin_registry;
use config::types::{AppConfig, DEFAULT_WORKSPACE_DIR_NAME};
use llm::proxy::LlmProxy;
use persona::engine::PersonaEngine;
use store::chat_store::ChatStore;
use store::memory_store::MemoryStore;

/// 进行中的生成任务（注册到 `AppState::cancel_flags`，供 `stop_generation` 取消）
pub struct ActiveStream {
    pub session_id: String,
    pub cancel: Arc<AtomicBool>,
}

/// 全局应用状态
pub struct AppState {
    pub config: Mutex<AppConfig>,
    /// 共享 Agent 调度器（内部状态热更新，无需外层锁）
    pub dispatcher: Arc<AgentDispatcher>,
    /// 存储句柄用 `Arc` 包装：工具运行时需要在 `send_message` 的 async 流程里
    /// 长期持有它们（`Arc<Mutex<T>>` 的解引用与 `Mutex<T>` 完全一致，
    /// 既有的 `state.chat_store.lock()` 写法不受影响）
    pub chat_store: Arc<Mutex<ChatStore>>,
    pub memory_store: Arc<Mutex<MemoryStore>>,
    /// 共享人格引擎（聊天 Agent 与记忆/人设工具共用）
    pub personas: Arc<RwLock<PersonaEngine>>,
    pub app_data_dir: Mutex<PathBuf>,
    /// 进行中生成的取消标记（key = stream_id）
    ///
    /// 历史实现从未写入过这个字段，导致 `stop_generation` 与 `cancel` 完全是死代码。
    pub cancel_flags: Mutex<HashMap<String, ActiveStream>>,
    /// 等待用户决定的工具审批（key = approval_id）
    pub pending_approvals: ApprovalMap,
}

/// 创建工作区目录并写入标识文件
///
/// 放在应用数据目录下（`%APPDATA%/com.konata-mirror.main/workspace`）：
/// 这是一个天然受限的默认沙箱——工具无法通过 `..` 或符号链接逃到父目录
/// （父目录里放着 config.json 与 data.db）。
fn bootstrap_workspace(app_data_dir: &std::path::Path) -> std::io::Result<PathBuf> {
    let root = app_data_dir.join(DEFAULT_WORKSPACE_DIR_NAME);
    std::fs::create_dir_all(&root)?;

    let marker = root.join(".konata-workspace");
    if !marker.exists() {
        std::fs::write(&marker, "konata-mirror-workspace v1\n")?;
    }
    Ok(root)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data dir");

            // ─── 分家后的一次性数据迁移 ───
            //
            // 本应用此前与另一条开发线共用 bundle id `com.konata-mirror.app`，
            // 两套迁移编号互相污染（真实事故：`no such column: context_summary`）。
            // 现在 identifier 已经独立，但用户的聊天记录 / API Key / 人格 / 工作区
            // 都还在旧目录里，因此这里**只复制、不删除**地搬一次，
            // 否则用户会以为"数据全没了"。必须在 load_config 之前执行。
            if let Some(legacy_dir) = appdata::resolve_legacy_dir(&app_data_dir) {
                match appdata::migrate_legacy_data_dir(&app_data_dir, &legacy_dir) {
                    Ok(report) if report.performed => {
                        println!(
                            "[appdata] 已从旧数据目录迁移：{} → {}（{}）",
                            legacy_dir.display(),
                            app_data_dir.display(),
                            report.copied.join("、")
                        );
                        for note in &report.notes {
                            eprintln!("[appdata] 提示：{}", note);
                        }
                    }
                    Ok(report) => {
                        println!(
                            "[appdata] 跳过旧数据迁移（{}）",
                            report.skipped_reason.unwrap_or_else(|| "未知原因".to_string())
                        );
                    }
                    Err(e) => eprintln!("[appdata] 旧数据迁移失败（不影响启动）: {}", e),
                }
            }

            let app_config = config::load_config(&app_data_dir).expect("Failed to load config");

            let conn = store::db::init_db(&app_data_dir).expect("Failed to init database");
            let chat_store = ChatStore::new(conn);

            // 第二个连接不再执行迁移（migrations 只应运行一次），
            // 只应用 WAL / foreign_keys 等连接级 PRAGMA。
            let mem_conn =
                store::db::open_connection(&app_data_dir).expect("Failed to open memory db");
            let memory_store = MemoryStore::new(mem_conn);
            // 一次性回填旧格式记忆向量（JSON → 归一化 BLOB）
            memory_store
                .backfill_embeddings()
                .expect("Failed to backfill embeddings");

            // 共享人格引擎（内置 + 磁盘用户自定义）
            let persona_engine =
                PersonaEngine::load_all(&app_data_dir).expect("Failed to init persona engine");
            let personas = Arc::new(RwLock::new(persona_engine));

            // 工具运行时：默认工作区固定在应用数据目录下的 workspace/
            //
            // 创建失败**不能**让应用起不来（磁盘只读、目录被占都可能发生）：
            // 记录错误并关掉工具开关，聊天功能必须继续可用。
            let mut app_config = app_config;
            if let Err(e) = bootstrap_workspace(&app_data_dir) {
                eprintln!("[harness] 工作区初始化失败，已禁用工具功能: {}", e);
                app_config.tools.enabled = false;
            }

            let tool_registry = Arc::new(builtin_registry());
            let llm_proxy = LlmProxy::new(app_config.llm.active_provider());
            let chat_agent = ChatAgent::new(llm_proxy, personas.clone(), tool_registry);
            let dispatcher = Arc::new(AgentDispatcher::new(chat_agent));

            app.manage(AppState {
                config: Mutex::new(app_config),
                dispatcher,
                chat_store: Arc::new(Mutex::new(chat_store)),
                memory_store: Arc::new(Mutex::new(memory_store)),
                personas,
                app_data_dir: Mutex::new(app_data_dir),
                cancel_flags: Mutex::new(HashMap::new()),
                pending_approvals: new_approval_map(),
            });

            // ─── 系统托盘 ─────────────────────────
            let show_item = MenuItemBuilder::with_id("show", "打开主窗口").build(app)?;
            let float_item = MenuItemBuilder::with_id("float", "打开悬浮窗").build(app)?;
            let settings_item = MenuItemBuilder::with_id("settings", "设置").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "退出").build(app)?;

            let menu = MenuBuilder::new(app)
                .item(&show_item)
                .item(&float_item)
                .separator()
                .item(&settings_item)
                .separator()
                .item(&quit_item)
                .build()?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("镜中此方")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    let id = event.id().as_ref();
                    match id {
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "float" => {
                            if let Some(window) = app.get_webview_window("float") {
                                if window.is_visible().unwrap_or(false) {
                                    let _ = window.hide();
                                    let _ = app.emit("float-visibility-changed", false);
                                } else {
                                    let _ = window.set_shadow(false);
                                    let position = app.state::<AppState>().config.lock().map(|c| c.ui.float_position.clone()).unwrap_or_else(|_| "bottom-right".to_string());
                                    commands::window::apply_float_position(&window, &position);
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                    let _ = app.emit("float-visibility-changed", true);
                                }
                            }
                        }
                        "settings" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                                let _ = app.emit("navigate-to", "settings");
                            }
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // ─── 窗口关闭拦截（根据配置决定行为） ───────
            let main_window = app.get_webview_window("main").unwrap();
            let window_clone = main_window.clone();
            let app_handle = app.handle().clone();
            main_window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let close_action = app_handle
                        .state::<AppState>()
                        .config
                        .lock()
                        .map(|c| c.ui.close_action.clone())
                        .unwrap_or_else(|_| "hide".to_string());

                    match close_action.as_str() {
                        "exit" => {
                            // 退出程序（不阻止默认关闭）
                            std::process::exit(0);
                        }
                        "hide_and_float" => {
                            // 隐藏主窗口并打开悬浮窗
                            api.prevent_close();
                            let _ = window_clone.hide();
                            if let Some(float_win) = app_handle.get_webview_window("float") {
                                let _ = float_win.set_shadow(false);
                                let position = app_handle.state::<AppState>().config.lock().map(|c| c.ui.float_position.clone()).unwrap_or_else(|_| "bottom-right".to_string());
                                commands::window::apply_float_position(&float_win, &position);
                                let _ = float_win.show();
                                let _ = float_win.set_focus();
                            }
                            let _ = app_handle.emit("float-visibility-changed", true);
                        }
                        _ => {
                            // 默认：隐藏到后台
                            api.prevent_close();
                            let _ = window_clone.hide();
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::chat::send_message,
            commands::chat::stop_generation,
            commands::chat::create_session,
            commands::chat::find_or_create_today_session,
            commands::chat::update_session_title,
            commands::chat::get_sessions,
            commands::chat::get_messages,
            commands::chat::delete_session,
            commands::settings::get_config,
            commands::settings::update_config,
            commands::settings::test_llm_connection,
            commands::settings::fetch_models,
            commands::settings::set_active_model,
            commands::settings::add_provider,
            commands::settings::update_provider,
            commands::settings::delete_provider,
            commands::settings::set_active_provider,
            commands::settings::test_provider_connection,
            commands::settings::fetch_provider_models,
            commands::persona::list_personas,
            commands::persona::get_persona_summary,
            commands::persona::get_persona_yaml,
            commands::persona::save_persona,
            commands::persona::delete_persona,
            commands::memory::list_memories,
            commands::memory::delete_memory,
            commands::memory::clear_memories,
            commands::backup::export_memories,
            commands::backup::import_memories,
            commands::backup::export_personas,
            commands::backup::import_personas,
            commands::stats::get_usage_stats,
            commands::stats::reset_usage_stats,
            commands::tools::list_tools,
            commands::tools::resolve_tool_approval,
            commands::tools::get_tool_invocations,
            commands::tools::get_tool_safety_summary,
            commands::tools::list_workspaces,
            commands::tools::add_workspace,
            commands::tools::update_workspace,
            commands::tools::remove_workspace,
            commands::window::show_main_window,
            commands::window::hide_main_window,
            commands::window::toggle_main_window,
            commands::window::show_float_window,
            commands::window::hide_float_window,
            commands::window::toggle_float_window,
            commands::window::is_float_visible,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
