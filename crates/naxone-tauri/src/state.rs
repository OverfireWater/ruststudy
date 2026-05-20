use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use naxone_adapters::config::fs_config::FsConfigIO;
use naxone_adapters::package::composite::CompositeScanner;
use naxone_adapters::platform::windows::WindowsPlatform;
use naxone_adapters::process::NativeProcessManager;
use naxone_adapters::template::SimpleTemplateEngine;
use naxone_adapters::vhost::VhostScanner;
use naxone_core::config::AppConfig;
use naxone_core::domain::service::ServiceInstance;
use naxone_core::domain::log::LogEntry;
use naxone_core::domain::vhost::VirtualHost;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64};
use naxone_core::ports::config_io::ConfigIO;
use naxone_core::ports::process::ProcessManager;
use naxone_core::ports::template::TemplateEngine;
use naxone_core::ports::platform::PlatformOps;
use naxone_core::use_cases::config_editor::ConfigEditor;
use naxone_core::use_cases::php_mgr::PhpManager;
use naxone_core::use_cases::service_mgr::ServiceManager;
use naxone_core::use_cases::vhost_mgr::VhostManager;

pub struct AppState {
    pub services: Arc<RwLock<Vec<ServiceInstance>>>,
    pub service_manager: ServiceManager,
    /// 共享的 ProcessManager 实例（service_manager / vhost_manager 内部各持一份 clone）。
    /// 用于在 main 启动尾段注入 LogReporter，watchdog 等后台事件可推到活动日志。
    pub process_mgr: Arc<dyn ProcessManager>,
    pub vhost_manager: VhostManager,
    pub php_manager: PhpManager,
    pub config_editor: ConfigEditor,
    pub vhosts: Arc<RwLock<Vec<VirtualHost>>>,
    pub config: Arc<RwLock<AppConfig>>,
    pub startup_errors: Arc<RwLock<Vec<String>>>,
    pub logs: Arc<RwLock<VecDeque<LogEntry>>>,
    pub log_id_counter: Arc<AtomicU64>,
    pub log_writer_tx: Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<LogEntry>>>>,
    /// 后台 status 刷新的 single-flight 标志：已有刷新在跑时跳过新触发
    pub refresh_in_flight: Arc<AtomicBool>,
    /// 平台相关操作（hosts 文件、防火墙），命令层也要直接用
    pub platform_ops: Arc<dyn PlatformOps>,
    /// 当前正在跑的模板装包子进程 PID（如 composer create-project）。
    /// 前端 cancel_init_site_template 时读 PID 杀进程树（taskkill /F /T）。
    pub template_child_pid: Arc<tokio::sync::Mutex<Option<u32>>>,
}

impl AppState {
    /// Create a shallow clone (Arc clones) suitable for moving into async tasks
    pub fn clone_shallow(&self) -> Self {
        Self {
            services: self.services.clone(),
            service_manager: self.service_manager.clone(),
            process_mgr: self.process_mgr.clone(),
            vhost_manager: self.vhost_manager.clone(),
            php_manager: self.php_manager.clone(),
            config_editor: self.config_editor.clone(),
            vhosts: self.vhosts.clone(),
            config: self.config.clone(),
            startup_errors: self.startup_errors.clone(),
            logs: self.logs.clone(),
            log_id_counter: self.log_id_counter.clone(),
            log_writer_tx: self.log_writer_tx.clone(),
            refresh_in_flight: self.refresh_in_flight.clone(),
            platform_ops: self.platform_ops.clone(),
            template_child_pid: self.template_child_pid.clone(),
        }
    }

    pub fn new() -> Self {
        let config_io = Arc::new(FsConfigIO) as Arc<dyn ConfigIO>;
        let template_engine = Arc::new(SimpleTemplateEngine) as Arc<dyn TemplateEngine>;
        let platform_ops = Arc::new(WindowsPlatform) as Arc<dyn PlatformOps>;
        let process_mgr = Arc::new(NativeProcessManager::new()) as Arc<dyn ProcessManager>;

        let service_manager = ServiceManager::new(process_mgr.clone());
        let vhost_manager = VhostManager::new(
            config_io.clone(),
            template_engine,
            platform_ops.clone(),
            process_mgr.clone(),
        );
        let php_manager = PhpManager::new(config_io.clone());
        let config_editor = ConfigEditor::new(config_io.clone());

        // 老用户从 RustStudy 升级：把 ~/.ruststudy 整个迁移到 ~/.naxone，
        // 把 AppData\Roaming\RustStudy 迁移到 AppData\Roaming\NaxOne。
        // 只在新目录不存在时执行（一次性、幂等）。
        migrate_legacy_ruststudy_data();

        // Try to load config or create default
        let cfg_path = config_path();
        let mut config = if cfg_path.exists() {
            AppConfig::load(&cfg_path).unwrap_or_else(|_| default_config())
        } else {
            let cfg = default_config();
            let _ = cfg.save(&cfg_path);
            cfg
        };

        let resolved_www_root = resolve_default_www_root();
        // 自动迁移：legacy 默认值；以及 dev 模式下写入但当前已切到 release 的脏值
        // （路径落在 cargo 的 target\debug 或 target\release 下，绝大多数情况都是
        // 上一次 cargo tauri dev 留下的，正式安装版应当指向自己同级的 www）
        let needs_migrate = config.general.www_root == legacy_default_www_root()
            || is_cargo_target_path(&config.general.www_root)
            || is_legacy_ruststudy_path(&config.general.www_root);
        if needs_migrate {
            tracing::info!(
                old = %config.general.www_root.display(),
                new = %resolved_www_root.display(),
                "迁移过期的 www_root（legacy 或 cargo target 路径）",
            );
            config.general.www_root = resolved_www_root.clone();
            let _ = config.save(&cfg_path);
        }

        // 保持 NaxOne 自己管理默认站点目录，不再自动改回 PHPStudy 的 WWW。

        // 确保 www_root 目录存在（新建站点时默认指向它，空目录也无妨）
        let _ = std::fs::create_dir_all(&config.general.www_root);

        // Scan for installed services from all sources.
        let ext_path = config
            .general
            .phpstudy_path
            .as_ref()
            .map(|p| p.join("Extensions"));
        let store_ext = resolve_packages_root(&config);
        let legacy_root = legacy_packages_root();
        tracing::info!(
            store_extensions = ?store_ext,
            "Resolved store extensions root",
        );
        let services = CompositeScanner::scan(
            ext_path.as_deref(),
            Some(&store_ext),
            &config.general.extra_install_paths,
            Some(&legacy_root),
        );
        tracing::info!(
            service_count = services.len(),
            phpstudy_ext = ?ext_path,
            store_ext = ?store_ext,
            "Initial scan completed",
        );

        // Load saved vhost metadata + scan .conf files, then merge
        let vhosts_json_path = vhosts_json_path();
        let saved_vhosts = VhostManager::load_vhosts_json(&vhosts_json_path);
        let scanned_vhosts = if let Some(phpstudy_path) = &config.general.phpstudy_path {
            let ext_path = phpstudy_path.join("Extensions");
            let nginx_vhosts_dir = find_extension_dir(&ext_path, "Nginx")
                .map(|d| d.join("conf").join("vhosts"));
            if let Some(dir) = nginx_vhosts_dir {
                VhostScanner::scan(config_io.as_ref(), &dir, &services).unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let vhosts = VhostManager::merge_vhosts(scanned_vhosts, saved_vhosts);

        Self {
            services: Arc::new(RwLock::new(services)),
            service_manager,
            process_mgr,
            vhost_manager,
            php_manager,
            config_editor,
            vhosts: Arc::new(RwLock::new(vhosts)),
            config: Arc::new(RwLock::new(config)),
            startup_errors: Arc::new(RwLock::new(Vec::new())),
            logs: Arc::new(RwLock::new(VecDeque::with_capacity(1000))),
            log_id_counter: Arc::new(AtomicU64::new(0)),
            log_writer_tx: Arc::new(tokio::sync::Mutex::new(None)),
            refresh_in_flight: Arc::new(AtomicBool::new(false)),
            platform_ops,
            template_child_pid: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

fn find_extension_dir(ext_path: &std::path::Path, prefix: &str) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(ext_path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(prefix) && entry.path().is_dir() {
                return Some(entry.path());
            }
        }
    }
    None
}

/// Resolve the store install root. We model this after PHPStudy's layout —
/// the directory ends up looking like a PHPStudy `Extensions/`, so users
/// can read it with familiar intuition.
///
/// Resolution order:
///   1. `config.general.package_install_root` if the user set one explicitly.
///   2. `{exe_dir}/Extensions/` if the exe's folder is writable (portable /
///      dev-time case).
///   3. `%APPDATA%/NaxOne/Extensions/` fallback (Program Files install).
pub fn resolve_packages_root(config: &AppConfig) -> PathBuf {
    if let Some(custom) = &config.general.package_install_root {
        if !custom.as_os_str().is_empty() {
            return custom.clone();
        }
    }

    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        if is_writable_dir(&exe_dir) {
            return exe_dir.join("Extensions");
        }
    }

    let appdata = std::env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
            PathBuf::from(home).join("AppData").join("Roaming")
        });
    appdata.join(naxone_appdata_dirname()).join("Extensions")
}

/// 新用户 / 迁移时使用的默认 WWW 根目录。
/// 策略同 packages_root：
///   1. exe 同级可写 → `{exe_dir}/www/`（便携模式、开发模式）
///   2. 不可写（Program Files）→ `%APPDATA%/NaxOne/www/`
pub fn resolve_default_www_root() -> PathBuf {
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        if is_writable_dir(&exe_dir) {
            return exe_dir.join("www");
        }
    }
    let appdata = std::env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
            PathBuf::from(home).join("AppData").join("Roaming")
        });
    appdata.join(naxone_appdata_dirname()).join("www")
}

/// Legacy root from the first store prototype (`%APPDATA%/NaxOne/Packages/`).
/// Returned so the scanner can still pick up packages installed before the
/// PHPStudy-style refactor.
pub fn legacy_packages_root() -> PathBuf {
    let appdata = std::env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
            PathBuf::from(home).join("AppData").join("Roaming")
        });
    appdata.join(naxone_appdata_dirname()).join("Packages")
}

/// Probe: can we create a file in this directory? Cheap, avoids relying on
/// metadata flags that lie on Windows.
fn is_writable_dir(dir: &std::path::Path) -> bool {
    if !dir.exists() {
        return std::fs::create_dir_all(dir).is_ok();
    }
    let probe = dir.join(format!(
        ".naxone-write-probe-{}.tmp",
        std::process::id()
    ));
    match std::fs::write(&probe, b"") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Resolve the log directory: custom from config, or default next to exe
pub fn resolve_log_dir(config: &AppConfig) -> PathBuf {
    if let Some(custom) = &config.general.log_dir {
        if !custom.as_os_str().is_empty() {
            return custom.clone();
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs")
}

/// 判断路径是不是落在 cargo 编译产物目录下（`...\target\debug\...` 或
/// `...\target\release\...`）。用于识别 dev 时写下、正式版启动后已无意义的脏路径。
fn is_cargo_target_path(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy().replace('/', "\\").to_lowercase();
    s.contains("\\target\\debug\\") || s.contains("\\target\\release\\")
}

/// 判断路径是否落在 RustStudy 时代的安装目录（如 `D:\RustStudy\www`）。
/// 改名 NaxOne 后，老用户配置里的 www_root 应迁移到新 default。
fn is_legacy_ruststudy_path(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy().replace('/', "\\").to_lowercase();
    // 匹配路径段，避免误伤项目源码所在的 ...\utils\ruststudy\... 这种巧合
    s.contains("\\ruststudy\\") || s.ends_with("\\ruststudy")
}

fn legacy_default_www_root() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
    PathBuf::from(home).join(naxone_home_dirname()).join("www")
}

/// dev 和 prod 共享同一个数据目录，允许两边同时启动。
/// 代价：双开同时写配置文件会竞态（实战几乎不会撞，因为写入是低频用户操作）。
/// dev 与 prod 用不同后缀彻底隔离用户态数据（PATH 条目、~/.naxone/、%APPDATA%/NaxOne/）。
/// 见 `naxone_adapters::platform::dirs` 模块说明。
pub use naxone_adapters::platform::dirs::{naxone_appdata_dirname, naxone_home_dirname};

pub fn vhosts_json_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
    PathBuf::from(home).join(naxone_home_dirname()).join("vhosts.json")
}

pub fn config_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
    PathBuf::from(home)
        .join(naxone_home_dirname())
        .join("naxone.toml")
}

fn default_config() -> AppConfig {
    let www_root = resolve_default_www_root();
    let phpstudy_path = PathBuf::from(r"D:\phpstudy_pro");
    if phpstudy_path.exists() {
        AppConfig::default_with_phpstudy(phpstudy_path, www_root)
    } else {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
        AppConfig::default_with_phpstudy(PathBuf::from(home).join(naxone_home_dirname()), www_root)
    }
}

/// 从 RustStudy 一次性迁移老用户数据。
/// - `~/.ruststudy/` → `~/.naxone/`（含 vhosts.json/certs/cache 等子目录）
///   并把 `ruststudy.toml` 改名为 `naxone.toml`。
/// - `%APPDATA%\RustStudy\` → `%APPDATA%\NaxOne\`（packages 安装树）。
/// 只在目标目录不存在时执行，幂等；失败不影响主流程。
fn migrate_legacy_ruststudy_data() {
    // dev 不迁移：dev 数据本来就独立在 ~/.naxone-dev/，跟历史 RustStudy 数据无关
    if cfg!(debug_assertions) {
        return;
    }
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
    let home = PathBuf::from(home);

    // ── 1. ~/.ruststudy → ~/.naxone ──
    let legacy_home = home.join(".ruststudy");
    let new_home = home.join(".naxone");
    if legacy_home.exists() && !new_home.exists() {
        match copy_dir_recursive(&legacy_home, &new_home) {
            Ok(_) => {
                // ruststudy.toml → naxone.toml
                let old_toml = new_home.join("ruststudy.toml");
                let new_toml = new_home.join("naxone.toml");
                if old_toml.exists() && !new_toml.exists() {
                    let _ = std::fs::rename(&old_toml, &new_toml);
                }
                tracing::info!(
                    from = %legacy_home.display(),
                    to = %new_home.display(),
                    "已从 RustStudy 迁移用户数据"
                );
            }
            Err(e) => {
                tracing::warn!(
                    from = %legacy_home.display(),
                    "RustStudy 用户数据迁移失败，跳过：{}", e
                );
            }
        }
    }

    // ── 2. %APPDATA%\RustStudy → %APPDATA%\NaxOne ──
    let appdata = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
    let legacy_app = appdata.join("RustStudy");
    let new_app = appdata.join("NaxOne");
    if legacy_app.exists() && !new_app.exists() {
        match copy_dir_recursive(&legacy_app, &new_app) {
            Ok(_) => tracing::info!(
                from = %legacy_app.display(),
                to = %new_app.display(),
                "已从 RustStudy 迁移 AppData 数据"
            ),
            Err(e) => tracing::warn!(
                from = %legacy_app.display(),
                "RustStudy AppData 迁移失败，跳过：{}", e
            ),
        }
    }
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}
