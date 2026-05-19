#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod state;

use crate::commands::logger::push_log;
use state::AppState;
use naxone_core::domain::log::LogLevel;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager,
};

fn main() {
    // 默认 info 级别，方便排查安装/代理/状态刷新相关的诊断日志
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hyper=warn,reqwest=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // WebView2 user data folder 隔离：dev 用 `<home>/webview2`，prod 用同一逻辑路径但 home
    // 不同（dev=.naxone-dev，prod=.naxone）。这样：
    // - dev 和 prod 完全不共享 webview cookies / localStorage / cache
    // - dev 启动时不会撞 prod 已锁的 user data folder（之前的 HRESULT 0x8007139F）
    // - admin 装的 prod 跟普通用户跑的 dev 互不打架
    #[cfg(target_os = "windows")]
    {
        let dir = naxone_adapters::platform::dirs::naxone_home_dir().join("webview2");
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: main 顶层、单线程、tauri webview 创建之前，env 写入安全
        unsafe { std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &dir); }
        tracing::info!(path = %dir.display(), "WebView2 user data folder");
        // Dev 模式开 CDP 9222，给 frontend/scripts/test-*-gui.mjs 自动化测试用。
        // prod 不开（性能 + 安全考虑）。
        #[cfg(debug_assertions)]
        unsafe {
            std::env::set_var(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                "--remote-debugging-port=9222",
            );
        }
    }

    let app_state = AppState::new();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init());

    // single-instance 只在 release 注册：dev 时允许和已装正式版并行，方便看 UI 改动
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        // 用户再次点击 exe 或快捷方式 → 激活已有窗口
        let windows = app.webview_windows();
        if let Some(win) = windows.values().next() {
            let _ = win.set_focus();
            let _ = win.unminimize();
            let _ = win.show();
        }
    }));

    builder
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::service::get_services,
            commands::service::get_services_fresh,
            commands::service::start_service,
            commands::service::stop_service,
            commands::service::restart_service,
            commands::service::start_all,
            commands::service::stop_all,
            commands::service::log_user_action,
            commands::strangers::scan_running_strangers,
            commands::strangers::kill_stranger,
            commands::vhost::get_vhosts,
            commands::vhost::create_vhost,
            commands::vhost::update_vhost,
            commands::vhost::delete_vhost,
            commands::vhost::toggle_vhost,
            commands::vhost::check_expired_vhosts,
            commands::vhost::get_php_versions,
            commands::vhost::generate_self_signed_cert,
            commands::php::get_php_instances,
            commands::php::get_php_extensions,
            commands::php::toggle_php_extension,
            commands::php::get_php_ini_settings,
            commands::php::save_php_ini_settings,
            commands::php::get_phpinfo_html,
            commands::php::get_global_php_version,
            commands::php::set_global_php_version,
            commands::php::fix_global_php_conflicts,
            commands::php::open_system_env_editor,
            commands::settings::get_config,
            commands::settings::save_config,
            commands::settings::rescan_services,
            commands::settings::add_extra_install_path,
            commands::settings::remove_extra_install_path,
            commands::settings::check_phpstudy_installed,
            commands::service_config::get_config_file_path,
            commands::service_config::get_nginx_config,
            commands::service_config::save_nginx_config,
            commands::service_config::get_mysql_config,
            commands::service_config::save_mysql_config,
            commands::service_config::get_redis_config,
            commands::service_config::save_redis_config,
            commands::utils::get_app_stats,
            commands::utils::get_app_version,
            commands::utils::open_in_browser,
            commands::utils::open_folder,
            commands::utils::open_file,
            commands::utils::check_port_available,
            commands::utils::dir_exists,
            commands::utils::read_log_tail,
            commands::utils::find_and_read_log,
            commands::utils::get_startup_errors,
            commands::hosts::get_hosts_file_path,
            commands::hosts::get_hosts_text,
            commands::hosts::save_hosts_text,
            commands::hosts::save_hosts_text_elevated,
            commands::updater::check_for_updates,
            commands::logger::get_logs,
            commands::logger::clear_logs,
            commands::logger::get_log_stats,
            commands::logger::open_log_dir,
            commands::package::list_packages,
            commands::package::refresh_package_index,
            commands::package::get_installed_packages,
            commands::package::install_package,
            commands::package::uninstall_package,
            commands::package::unlink_system_tool,
            commands::package::deep_uninstall_system_tool,
            commands::package::preview_system_tool_uninstall,
            commands::tools::get_dev_tools_info,
            commands::tools::switch_node_version,
            commands::tools::set_global_composer,
            commands::tools::get_composer_repo,
            commands::tools::set_composer_repo,
            commands::tools::set_global_mysql,
            commands::tools::set_mysql_password,
            commands::tools::fix_mysql_path_conflicts,
            commands::port::diagnose_port,
            commands::port::kill_process_by_pid,
            commands::pie::pie_runtime_info,
            commands::pie::pie_search,
            commands::pie::pie_install,
            commands::vhost::read_vhost_conf,
            commands::vhost::write_vhost_conf,
            commands::template::init_site_template,
            commands::template::cancel_init_site_template,
            commands::template::cleanup_template_dir,
        ])
        .setup(|app| {
            // Dev 模式窗口标题加标识
            if cfg!(debug_assertions) {
                if let Some(win) = app.webview_windows().values().next() {
                    let _ = win.set_title("NaxOne (Development)");
                }
            }

            // Start log writer background task
            {
                let state = app.state::<AppState>();
                let state_arc: std::sync::Arc<AppState> = std::sync::Arc::new(state.inner().clone_shallow());
                let tx = commands::logger::spawn_log_writer(state_arc);
                // Blocking set the sender - we're in setup, before runtime is fully started
                let tx_slot = state.log_writer_tx.clone();
                tauri::async_runtime::block_on(async move {
                    *tx_slot.lock().await = Some(tx);
                });
            }

            // Clean up old log files
            {
                let state = app.state::<AppState>();
                let config = state.config.clone();
                tauri::async_runtime::spawn(async move {
                    let cfg = config.read().await.clone();
                    let dir = state::resolve_log_dir(&cfg);
                    commands::logger::cleanup_old_logs(&dir, cfg.general.log_retention_days);
                });
            }

            // Log startup (with a concise scan summary — useful for debugging
            // "no services showing" reports without flooding the log)
            {
                let state = app.state::<AppState>();
                let state_clone: std::sync::Arc<AppState> = std::sync::Arc::new((*state.inner()).clone_shallow());
                tauri::async_runtime::spawn(async move {
                    let svc_count = state_clone.services.read().await.len();
                    commands::logger::push_log(
                        &state_clone,
                        naxone_core::domain::log::LogLevel::Info,
                        "system",
                        format!("NaxOne 启动（扫到 {} 个服务）", svc_count),
                        None, None,
                    ).await;
                });
            }

            // 启动迁移：给已经装好的 PHP 修 php.ini 的 extension_dir。
            // 商店包 / 历史装的 PHP 默认 extension_dir 全注释，导致 openssl 等扩展加载失败 →
            // composer create-project / php-cgi 跑站点都会受影响。幂等，已修复的 PHP 不再改。
            {
                let state = app.state::<AppState>();
                let state_clone = state.inner().clone_shallow();
                tauri::async_runtime::spawn(async move {
                    use naxone_core::domain::service::ServiceKind;
                    let services_snap = state_clone.services.read().await.clone();
                    for svc in services_snap.iter().filter(|s| s.kind == ServiceKind::Php) {
                        match naxone_adapters::package::post_install::ensure_php_ini_extension_dir(&svc.install_path) {
                            Ok(true) => tracing::info!(install = %svc.install_path.display(), "PHP 启动迁移：php.ini extension_dir 已修复"),
                            Ok(false) => {}
                            Err(e) => tracing::warn!(install = %svc.install_path.display(), "PHP 启动迁移失败: {}", e),
                        }
                    }
                });
            }

            // 启动迁移：从 vhosts.json 元数据 regenerate 缺失的 nginx/apache vhost .conf。
            // 场景：用户卸载 nginx 重装，目录里 conf/vhosts/ 是空的；或者首次装商店 nginx
            // 默认就没建 vhosts 子目录。幂等：已有的 .conf 不会被覆盖。
            {
                let state = app.state::<AppState>();
                let state_clone = state.inner().clone_shallow();
                tauri::async_runtime::spawn(async move {
                    use naxone_core::domain::service::ServiceKind;
                    let services = state_clone.services.read().await.clone();
                    let nginx_install = services.iter().find(|s| s.kind == ServiceKind::Nginx).map(|s| s.install_path.clone());
                    let apache_install = services.iter().find(|s| s.kind == ServiceKind::Apache).map(|s| s.install_path.clone());
                    drop(services);

                    let nginx_vhosts = nginx_install.as_ref().map(|d| d.join("conf").join("vhosts"));
                    let apache_vhosts = apache_install.as_ref().map(|d| d.join("conf").join("vhosts"));
                    if let Some(d) = &nginx_vhosts { let _ = std::fs::create_dir_all(d); }
                    if let Some(d) = &apache_vhosts { let _ = std::fs::create_dir_all(d); }

                    // 同步保证 nginx.conf 含 include vhosts/*.conf;
                    if let Some(install) = &nginx_install {
                        if let Err(e) = commands::vhost::ensure_nginx_vhosts_include(install) {
                            tracing::warn!("启动迁移 ensure_nginx_vhosts_include 失败: {}", e);
                        }
                    }

                    let vhosts = state_clone.vhosts.read().await.clone();
                    let mut restored = 0usize;
                    let mut broken: Vec<(String, String)> = Vec::new(); // (id, document_root)
                    for v in &vhosts {
                        match state_clone.vhost_manager.regenerate_configs(
                            v,
                            nginx_vhosts.as_deref(),
                            apache_vhosts.as_deref(),
                        ) {
                            Ok(true) => restored += 1,
                            Ok(false) => {}
                            Err(e) => {
                                // document_root 不存在 → 标记坏 vhost。同时清掉对应的旧 .conf
                                // 避免 nginx -t 因引用不存在文件而启动失败。
                                broken.push((v.id.clone(), v.document_root.display().to_string()));
                                let filename = format!("{}_{}.conf", v.server_name, v.listen_port);
                                if let Some(d) = &nginx_vhosts { let _ = std::fs::remove_file(d.join(&filename)); }
                                if let Some(d) = &apache_vhosts { let _ = std::fs::remove_file(d.join(&filename)); }
                                tracing::warn!(vhost = %v.id, "{}", e);
                            }
                        }
                    }
                    if restored > 0 {
                        tracing::info!(restored, "启动迁移：从 vhosts.json 重生成 {} 个 vhost 配置文件", restored);
                    }
                    if !broken.is_empty() {
                        use naxone_core::domain::log::LogLevel;
                        let details = broken
                            .iter()
                            .map(|(id, root)| format!("• {} → {}", id, root))
                            .collect::<Vec<_>>()
                            .join("\n");
                        commands::logger::push_log(
                            &state_clone,
                            LogLevel::Warn,
                            "vhost",
                            format!("发现 {} 个站点的目录已不存在，对应 nginx 配置已暂时移除", broken.len()),
                            Some(format!(
                                "以下站点目录缺失，请到「网站」页面处理（删除或重新指定目录）:\n{}",
                                details
                            )),
                            None,
                        )
                        .await;
                    }
                });
            }

            // 启动预热：并行刷新一次所有服务 status，让第一次 get_services 直接命中
            {
                let state = app.state::<AppState>();
                let warmup_state = state.inner().clone_shallow();
                tauri::async_runtime::spawn(async move {
                    let t = std::time::Instant::now();
                    commands::service::refresh_all_services_bg(warmup_state).await;
                    tracing::info!(elapsed_ms = t.elapsed().as_millis() as u64, "预热 status 缓存完成");
                });
            }

            // Auto-start services based on config
            {
                let state = app.state::<AppState>();
                let services = state.services.clone();
                let config = state.config.clone();
                let service_manager = state.service_manager.clone();
                let errors = state.startup_errors.clone();
                tauri::async_runtime::spawn(async move {
                    let auto_start = config.read().await.general.auto_start.clone();
                    if auto_start.is_empty() { return; }

                    // 用 snapshot + start_with_deps，让自启 web 服务器时自动联动 PHP-CGI；
                    // start_with_deps 还会处理 Nginx/Apache 互斥和同 kind 多版本互斥。
                    let snapshot = { services.read().await.clone() };

                    for (idx, svc) in snapshot.iter().enumerate() {
                        let kind_name = svc.kind.display_name().to_lowercase();
                        // 精确匹配：避免 "php" 匹中 "phpstudy" 等其它名
                        if !auto_start.iter().any(|a| kind_name == a.to_lowercase()) {
                            continue;
                        }

                        let mut target = svc.clone();
                        // 先刷状态：端口已被自家进程占（如 PHPStudy 自启同款 nginx）时跳过，不重复启动。
                        // 注意：refresh_status 应已通过 exe path 区分自家 vs 陌生进程；
                        // 这里只信任 is_running 结果即可。日志降到 debug 级，避免与后续
                        // start 失败时的"端口被外部占用"warn 在用户眼里同时出现造成困惑。
                        let _ = service_manager.refresh_status(&mut target).await;
                        if target.status.is_running() {
                            tracing::debug!(
                                service = target.kind.display_name(),
                                "auto_start: 已在运行，跳过"
                            );
                            // 把刷新后的 status 同步回 shared services
                            if let Some(s) = services.write().await.iter_mut().find(|s| s.id() == target.id()) {
                                s.status = target.status.clone();
                            }
                            continue;
                        }

                        // 构造 others（除 target 外的所有服务），让 start_with_deps 能联动 PHP / 处理互斥
                        let mut others: Vec<_> = snapshot
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != idx)
                            .map(|(_, s)| s.clone())
                            .collect();

                        match service_manager.start_with_deps(&mut target, &mut others).await {
                            Ok(_) => {
                                // 同步 target + others（含被联动启动的 PHP-CGI）的状态回 shared services
                                let mut svcs = services.write().await;
                                if let Some(s) = svcs.iter_mut().find(|s| s.id() == target.id()) {
                                    *s = target.clone();
                                }
                                for o in &others {
                                    if let Some(s) = svcs.iter_mut().find(|s| s.id() == o.id()) {
                                        s.status = o.status.clone();
                                    }
                                }
                            }
                            Err(e) => {
                                let estr = e.to_string();
                                // 端口被外部进程占着是用户环境问题（PHPStudy / 系统服务等），
                                // 走 warn 不弹"自动启动失败"红条，让用户在仪表板"陌生进程"banner 里处理。
                                if estr.contains("已被外部进程占用") {
                                    tracing::warn!(
                                        service = target.kind.display_name(),
                                        "自动启动跳过：{}",
                                        estr
                                    );
                                } else {
                                    let msg = format!("{} 自动启动失败: {}", target.kind.display_name(), e);
                                    tracing::error!("{}", &msg);
                                    errors.write().await.push(msg);
                                }
                            }
                        }
                    }
                });
            }

            // Build tray menu
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            // Create tray icon
            let icon = app.default_window_icon().cloned()
                .expect("No default window icon");

            TrayIconBuilder::new()
                .icon(icon)
                .tooltip("NaxOne")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        let state = app.state::<AppState>();
                        let app_state = std::sync::Arc::new(state.inner().clone_shallow());
                        tauri::async_runtime::block_on(async move {
                            let stop_on_exit = app_state.config.read().await.general.stop_services_on_exit;
                            if stop_on_exit {
                                push_log(
                                    &app_state,
                                    LogLevel::Info,
                                    "system",
                                    "退出应用：开始停止全部服务",
                                    None,
                                    None,
                                )
                                .await;

                                let mut working = { app_state.services.read().await.clone() };
                                let mut errors = Vec::new();
                                for svc in working.iter_mut() {
                                    if let Err(e) = app_state.service_manager.stop_service(svc).await {
                                        let msg = format!("{} {} 停止失败: {}", svc.kind.display_name(), svc.version, e);
                                        errors.push(msg.clone());
                                        push_log(
                                            &app_state,
                                            LogLevel::Error,
                                            "service",
                                            format!("{} {} 停止失败", svc.kind.display_name(), svc.version),
                                            Some(e.to_string()),
                                            None,
                                        )
                                        .await;
                                    }
                                }

                                {
                                    let mut services = app_state.services.write().await;
                                    for svc in services.iter_mut() {
                                        if let Some(updated) = working.iter().find(|w| w.id() == svc.id()) {
                                            svc.status = updated.status.clone();
                                        }
                                    }
                                }

                                if errors.is_empty() {
                                    push_log(
                                        &app_state,
                                        LogLevel::Success,
                                        "system",
                                        "退出应用：全部服务已停止",
                                        None,
                                        None,
                                    )
                                    .await;
                                } else {
                                    push_log(
                                        &app_state,
                                        LogLevel::Warn,
                                        "system",
                                        "退出应用：部分服务停止失败",
                                        Some(errors.join("\n")),
                                        None,
                                    )
                                    .await;
                                }
                            }
                        });
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Minimize to tray on close (instead of quitting)
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running NaxOne");
}
