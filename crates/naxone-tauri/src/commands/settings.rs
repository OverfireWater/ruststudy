use serde::{Deserialize, Serialize};
use tauri::{Emitter, State};

use crate::commands::logger::push_log;
use crate::state::AppState;
use naxone_core::config::ExtraInstallPath;
use naxone_core::domain::log::LogLevel;
use naxone_core::domain::service::ServiceKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraInstallPathDto {
    pub id: String,
    /// "nginx" | "apache" | "mysql" | "redis" | "php"
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
}

impl From<&ExtraInstallPath> for ExtraInstallPathDto {
    fn from(e: &ExtraInstallPath) -> Self {
        Self {
            id: e.id.clone(),
            kind: kind_to_str(e.kind).into(),
            path: e.path.display().to_string(),
            label: e.label.clone(),
        }
    }
}

fn kind_to_str(k: ServiceKind) -> &'static str {
    match k {
        ServiceKind::Nginx => "nginx",
        ServiceKind::Apache => "apache",
        ServiceKind::Mysql => "mysql",
        ServiceKind::Redis => "redis",
        ServiceKind::Php => "php",
    }
}

fn parse_kind(s: &str) -> Result<ServiceKind, String> {
    match s.to_lowercase().as_str() {
        "nginx" => Ok(ServiceKind::Nginx),
        "apache" | "httpd" => Ok(ServiceKind::Apache),
        "mysql" => Ok(ServiceKind::Mysql),
        "redis" => Ok(ServiceKind::Redis),
        "php" => Ok(ServiceKind::Php),
        _ => Err(format!("未知服务类型: {}", s)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDto {
    pub phpstudy_path: String,
    pub www_root: String,
    pub active_web_server: String,
    pub auto_start: Vec<String>,
    pub mysql_port: u16,
    pub redis_port: u16,
    #[serde(default)]
    pub log_dir: String,
    #[serde(default = "default_retention")]
    pub log_retention_days: u32,
    #[serde(default)]
    pub extra_install_paths: Vec<ExtraInstallPathDto>,
    #[serde(default)]
    pub stop_services_on_exit: bool,
    #[serde(default = "default_php_worker_budget")]
    pub php_worker_budget: u16,
    #[serde(default = "default_phpstudy_compatible_workers")]
    pub phpstudy_compatible_workers: bool,
    #[serde(default = "default_php_workers_per_version")]
    pub php_workers_per_version: u16,
    #[serde(default = "default_php_max_requests")]
    pub php_max_requests: u32,
}

fn default_retention() -> u32 {
    7
}

fn default_php_worker_budget() -> u16 { 8 }
fn default_phpstudy_compatible_workers() -> bool { true }
fn default_php_workers_per_version() -> u16 { 4 }
fn default_php_max_requests() -> u32 { 1000 }

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<ConfigDto, String> {
    let config = state.config.read().await;
    Ok(ConfigDto {
        phpstudy_path: config
            .general
            .phpstudy_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        www_root: config.general.www_root.display().to_string(),
        active_web_server: config.web_server.active.clone(),
        auto_start: config.general.auto_start.clone(),
        mysql_port: config.mysql.port,
        redis_port: config.redis.port,
        log_dir: config
            .general
            .log_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        log_retention_days: config.general.log_retention_days,
        extra_install_paths: config
            .general
            .extra_install_paths
            .iter()
            .map(ExtraInstallPathDto::from)
            .collect(),
        stop_services_on_exit: config.general.stop_services_on_exit,
        php_worker_budget: config.php_runtime.total_worker_budget,
        phpstudy_compatible_workers: config.php_runtime.phpstudy_compatible_workers,
        php_workers_per_version: config.php_runtime.max_workers_per_version,
        php_max_requests: config.php_runtime.max_requests,
    })
}

#[tauri::command]
pub async fn save_config(
    dto: ConfigDto,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if !(1..=64).contains(&dto.php_worker_budget) {
        return Err("PHP worker 总预算必须在 1–64 之间".into());
    }
    if !(1..=16).contains(&dto.php_workers_per_version) {
        return Err("单个 PHP 版本 worker 上限必须在 1–16 之间".into());
    }
    if !(100..=10_000).contains(&dto.php_max_requests) {
        return Err("PHP worker 最大请求数必须在 100–10000 之间".into());
    }

    let mut config = state.config.write().await;

    // 记录旧的 phpstudy_path，保存后若变化则自动 rescan，避免 services 缓存指向旧路径
    let old_phpstudy = config.general.phpstudy_path.clone();
    let new_phpstudy = if dto.phpstudy_path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(&dto.phpstudy_path))
    };

    config.general.phpstudy_path = new_phpstudy.clone();
    config.general.www_root = std::path::PathBuf::from(&dto.www_root);
    config.web_server.active = dto.active_web_server;
    config.general.auto_start = dto.auto_start;
    config.mysql.port = dto.mysql_port;
    config.redis.port = dto.redis_port;
    config.general.log_dir = if dto.log_dir.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(&dto.log_dir))
    };
    config.general.log_retention_days = dto.log_retention_days;
    config.general.stop_services_on_exit = dto.stop_services_on_exit;
    config.php_runtime.total_worker_budget = dto.php_worker_budget;
    config.php_runtime.phpstudy_compatible_workers = dto.phpstudy_compatible_workers;
    config.php_runtime.max_workers_per_version = dto.php_workers_per_version;
    config.php_runtime.max_requests = dto.php_max_requests;

    // Persist to file
    let config_path = crate::state::config_path();
    config.save(&config_path).map_err(|e| e.to_string())?;
    drop(config);

    push_log(
        &state,
        LogLevel::Success,
        "settings",
        "保存全局设置",
        None,
        None,
    )
    .await;

    // PHPStudy 路径变了 → 先清掉 vhosts.json 里来源为 PhpStudy 的旧条目（指向旧路径已失效），
    // 再 rescan 让新路径下的 vhost 被扫到。用户自建（source=Custom）的不动。
    if old_phpstudy != new_phpstudy {
        purge_phpstudy_sourced_vhosts(&state).await;
        let _ = rescan_services(app, state).await;
    }

    Ok(())
}

/// 切换 phpstudy 路径时调用：从 vhosts.json 中删除 source=PhpStudy 的条目。
/// Custom / Standalone 来源保留。
async fn purge_phpstudy_sourced_vhosts(state: &State<'_, AppState>) {
    use naxone_core::domain::vhost::VhostSource;
    use naxone_core::use_cases::vhost_mgr::VhostManager;

    let path = crate::state::vhosts_json_path();
    let mut saved = VhostManager::load_vhosts_json(&path);
    let before = saved.len();
    saved.retain(|v| !matches!(v.source, VhostSource::PhpStudy));
    if saved.len() == before {
        return;
    }
    // 直接写 JSON，VhostManager 没暴露不依赖 self 的写方法
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&saved) {
        Ok(s) => {
            let _ = std::fs::write(&path, s);
        }
        Err(e) => {
            tracing::warn!("写 vhosts.json 失败: {}", e);
        }
    }
    push_log(
        state,
        LogLevel::Info,
        "settings",
        format!("切换 PHPStudy 路径：清理 {} 条旧扫描 vhost 元数据", before - saved.len()),
        None,
        None,
    )
    .await;
}

#[tauri::command]
pub async fn rescan_services(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use naxone_adapters::config::fs_config::FsConfigIO;
    use naxone_adapters::package::composite::CompositeScanner;
    use naxone_adapters::vhost::VhostScanner;
    use naxone_core::use_cases::vhost_mgr::VhostManager;

    let config = state.config.read().await;
    let phpstudy_opt = config.general.phpstudy_path.clone();
    let extras = config.general.extra_install_paths.clone();
    let store_ext = crate::state::resolve_packages_root(&config);
    let legacy_root = crate::state::legacy_packages_root();
    drop(config);

    let ext_path = phpstudy_opt.as_ref().map(|p| p.join("Extensions"));
    let mut new_services = CompositeScanner::scan(
        ext_path.as_deref(),
        Some(&store_ext),
        &extras,
        Some(&legacy_root),
    );
    // 诊断日志：便于排查"rescan 后服务空了"问题
    push_log(
        &state,
        LogLevel::Info,
        "system",
        format!(
            "rescan 完成（扫到 {} 个）",
            new_services.len()
        ),
        Some(format!(
            "phpstudy_ext={:?}\nstore_ext={}\nextras={}",
            ext_path,
            store_ext.display(),
            extras.len()
        )),
        None,
    )
    .await;

    // Rescan vhosts and merge with saved metadata (PHPStudy dir only for phase 1).
    let saved_vhosts = VhostManager::load_vhosts_json(&crate::state::vhosts_json_path());
    let scanned_vhosts = if let Some(ref phpstudy_path) = phpstudy_opt {
        let ext_path = phpstudy_path.join("Extensions");
        let nginx_dir = std::fs::read_dir(&ext_path).ok().and_then(|rd| {
            rd.flatten()
                .find(|e| {
                    e.file_name().to_string_lossy().starts_with("Nginx") && e.path().is_dir()
                })
                .map(|e| e.path().join("conf").join("vhosts"))
        });
        if let Some(dir) = nginx_dir {
            VhostScanner::scan(&FsConfigIO, &dir, &new_services).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let merged = VhostManager::merge_vhosts(scanned_vhosts, saved_vhosts);
    let php_runtime = state.config.read().await.php_runtime.clone();
    naxone_core::use_cases::php_runtime::apply_php_runtime_policy(
        &mut new_services,
        &merged,
        &php_runtime,
    );

    {
        let mut services = state.services.write().await;
        *services = new_services;
    }
    {
        let mut vhosts = state.vhosts.write().await;
        *vhosts = merged;
    }

    // nginx/apache 重装后会丢失 conf/vhosts/*.conf。从 vhosts.json 元数据自动重写
    // 缺失的配置文件（不动用户已有的 .conf，幂等）。这样用户卸载 nginx → 重装 → 站点立即恢复。
    regenerate_missing_vhost_configs(&state).await;

    // 通知前端：services + vhosts 都已更新，让 Dashboard / Vhosts 立即刷新
    let _ = app.emit("services-changed", ());
    Ok(())
}

/// 扫所有 web 服务器 install_dir，对 state.vhosts 里每个 vhost 检查 .conf 是否存在，
/// 缺失则通过 VhostManager.regenerate_configs 重新写出来。
async fn regenerate_missing_vhost_configs(state: &State<'_, AppState>) {
    use naxone_core::domain::service::ServiceKind;

    let services = state.services.read().await.clone();
    let nginx_install = services.iter().find(|s| s.kind == ServiceKind::Nginx).map(|s| s.install_path.clone());
    let apache_install = services.iter().find(|s| s.kind == ServiceKind::Apache).map(|s| s.install_path.clone());
    drop(services);

    let nginx_vhosts = nginx_install.as_ref().map(|d| d.join("conf").join("vhosts"));
    let apache_vhosts = apache_install.as_ref().map(|d| d.join("conf").join("vhosts"));

    if let Some(d) = &nginx_vhosts { let _ = std::fs::create_dir_all(d); }
    if let Some(d) = &apache_vhosts { let _ = std::fs::create_dir_all(d); }

    let vhosts = state.vhosts.read().await.clone();
    let mut restored = 0usize;
    for v in &vhosts {
        match state.vhost_manager.regenerate_configs(
            v,
            nginx_vhosts.as_deref(),
            apache_vhosts.as_deref(),
        ) {
            Ok(true) => restored += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!("regenerate vhost {} 失败: {}", v.id, e),
        }
    }
    if restored > 0 {
        push_log(
            state,
            LogLevel::Success,
            "vhost",
            format!("从元数据恢复了 {} 个 vhost 配置文件", restored),
            Some("nginx/apache 重装后 conf/vhosts/ 是空的，已根据 vhosts.json 自动重写。请到 Web 服务器手动 reload 或重启服务以加载新配置。".to_string()),
            None,
        )
        .await;
    }
}

// ==================== Extra install paths ====================

#[derive(Debug, Clone, Deserialize)]
pub struct AddExtraPathRequest {
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[tauri::command]
pub async fn add_extra_install_path(
    req: AddExtraPathRequest,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ExtraInstallPathDto>, String> {
    let kind = parse_kind(&req.kind)?;
    let path = std::path::PathBuf::from(&req.path);
    if !path.exists() {
        return Err(format!("路径不存在: {}", req.path));
    }

    let id = format!(
        "{}-{}",
        kind_to_str(kind),
        chrono::Local::now().timestamp_millis()
    );
    let entry = ExtraInstallPath {
        id: id.clone(),
        kind,
        path,
        label: req.label,
    };

    let mut config = state.config.write().await;
    // De-dup by kind+path
    let already = config
        .general
        .extra_install_paths
        .iter()
        .any(|e| e.kind == kind && e.path == entry.path);
    if already {
        return Err("该路径已存在".into());
    }
    config.general.extra_install_paths.push(entry);
    let cfg_path = crate::state::config_path();
    config.save(&cfg_path).map_err(|e| e.to_string())?;
    let result: Vec<ExtraInstallPathDto> = config
        .general
        .extra_install_paths
        .iter()
        .map(ExtraInstallPathDto::from)
        .collect();
    drop(config);

    push_log(
        &state,
        LogLevel::Success,
        "settings",
        format!("添加独立安装路径 ({}): {}", req.kind, req.path),
        None,
        None,
    )
    .await;

    // Auto-rescan so the new service shows up immediately.
    let _ = rescan_services(app, state).await;

    Ok(result)
}

#[tauri::command]
pub async fn remove_extra_install_path(
    id: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ExtraInstallPathDto>, String> {
    let mut config = state.config.write().await;
    let before = config.general.extra_install_paths.len();
    config.general.extra_install_paths.retain(|e| e.id != id);
    if config.general.extra_install_paths.len() == before {
        return Err("未找到该路径条目".into());
    }
    let cfg_path = crate::state::config_path();
    config.save(&cfg_path).map_err(|e| e.to_string())?;
    let result: Vec<ExtraInstallPathDto> = config
        .general
        .extra_install_paths
        .iter()
        .map(ExtraInstallPathDto::from)
        .collect();
    drop(config);

    push_log(
        &state,
        LogLevel::Info,
        "settings",
        format!("删除独立安装路径: {}", id),
        None,
        None,
    )
    .await;

    let _ = rescan_services(app, state).await;
    Ok(result)
}

/// Indicates whether a usable PHPStudy install was found at the configured path.
/// The frontend uses this for empty-state / onboarding decisions.
#[tauri::command]
pub async fn check_phpstudy_installed(state: State<'_, AppState>) -> Result<bool, String> {
    let config = state.config.read().await;
    let ok = config
        .general
        .phpstudy_path
        .as_ref()
        .map(|p| p.join("Extensions").exists())
        .unwrap_or(false);
    Ok(ok)
}
