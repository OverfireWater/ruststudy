use crate::domain::service::ServiceKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Main application configuration (naxone.toml)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub general: GeneralConfig,
    #[serde(default)]
    pub web_server: WebServerConfig,
    #[serde(default)]
    pub mysql: MysqlConfig,
    #[serde(default)]
    pub redis: RedisConfig,
    #[serde(default)]
    pub php_runtime: PhpRuntimeConfig,
    #[serde(default)]
    pub php_instances: HashMap<String, PhpInstanceConfig>,
}

/// A user-specified extra install path for a standalone (non-PHPStudy) service.
/// E.g. the user has Nginx installed somewhere outside PHPStudy and wants
/// NaxOne to manage it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraInstallPath {
    /// Stable id for frontend remove operations
    pub id: String,
    pub kind: ServiceKind,
    pub path: PathBuf,
    /// Optional user-friendly label
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub data_dir: PathBuf,
    pub www_root: PathBuf,
    #[serde(default)]
    pub phpstudy_path: Option<PathBuf>,
    #[serde(default = "default_auto_start")]
    pub auto_start: Vec<String>,
    #[serde(default)]
    pub log_dir: Option<PathBuf>,
    #[serde(default = "default_log_retention")]
    pub log_retention_days: u32,
    /// Paths to manually-added standalone installs
    #[serde(default)]
    pub extra_install_paths: Vec<ExtraInstallPath>,
    /// Root dir where packages installed via the in-app store go.
    /// Default (None) resolves to %APPDATA%/NaxOne/Packages/.
    #[serde(default)]
    pub package_install_root: Option<PathBuf>,
    /// CLI 层全局 PHP 版本。None 表示未设置；设置后在
    /// %USERPROFILE%\.naxone\bin\php.cmd 里把 php 命令指向这个版本。
    /// 不影响 vhost 绑定的 PHP 版本，只影响用户命令行里敲 `php -v`。
    #[serde(default)]
    pub global_php_version: Option<String>,
    /// 托盘“退出”时是否先停止全部服务。
    #[serde(default)]
    pub stop_services_on_exit: bool,
    /// 用户通过「解除关联」操作隐藏的系统级工具名（"composer" / "nvm" 等）。
    /// NaxOne 视野内当作没装，但用户原有的系统安装本身不会被改动。
    #[serde(default)]
    pub ignored_system_tools: Vec<String>,
}

fn default_auto_start() -> Vec<String> {
    vec![]
}

fn default_log_retention() -> u32 {
    7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebServerConfig {
    #[serde(default = "default_web_server")]
    pub active: String,
    pub nginx_version: Option<String>,
    pub apache_version: Option<String>,
}

fn default_web_server() -> String {
    "nginx".into()
}

impl Default for WebServerConfig {
    fn default() -> Self {
        Self {
            active: "nginx".into(),
            nginx_version: None,
            apache_version: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MysqlConfig {
    pub version: Option<String>,
    #[serde(default = "default_mysql_port")]
    pub port: u16,
}

fn default_mysql_port() -> u16 {
    3306
}

impl Default for MysqlConfig {
    fn default() -> Self {
        Self {
            version: None,
            port: 3306,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    pub version: Option<String>,
    #[serde(default = "default_redis_port")]
    pub port: u16,
}

fn default_redis_port() -> u16 {
    6379
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            version: None,
            port: 6379,
        }
    }
}

/// Global PHP FastCGI pool limits.
///
/// Workers are distributed only among PHP versions referenced by enabled
/// vhosts. `total_worker_budget` is therefore a global budget, not a
/// per-version multiplier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhpRuntimeConfig {
    /// Match PHPStudy's `1+16` pool: one manager/root plus 16 request workers
    /// for every PHP version referenced by an enabled vhost.
    #[serde(default = "default_phpstudy_compatible_workers")]
    pub phpstudy_compatible_workers: bool,
    #[serde(default = "default_total_worker_budget")]
    pub total_worker_budget: u16,
    #[serde(default = "default_max_workers_per_version")]
    pub max_workers_per_version: u16,
    #[serde(default = "default_max_requests")]
    pub max_requests: u32,
}

fn default_phpstudy_compatible_workers() -> bool {
    true
}

fn default_total_worker_budget() -> u16 {
    8
}

fn default_max_workers_per_version() -> u16 {
    4
}

fn default_max_requests() -> u32 {
    1000
}

impl Default for PhpRuntimeConfig {
    fn default() -> Self {
        Self {
            phpstudy_compatible_workers: default_phpstudy_compatible_workers(),
            total_worker_budget: default_total_worker_budget(),
            max_workers_per_version: default_max_workers_per_version(),
            max_requests: default_max_requests(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhpInstanceConfig {
    pub port: u16,
    #[serde(default = "default_workers")]
    pub workers: u16,
    #[serde(default = "default_max_requests")]
    pub max_requests: u32,
    #[serde(default)]
    pub auto_start: bool,
}

fn default_workers() -> u16 {
    4
}

impl AppConfig {
    /// Load config from a TOML file。
    /// 解析失败时不再 crash：把坏文件重命名为 .corrupt-<ts>.toml 留底，返回 Err，
    /// 让 caller（state.rs）走 default_config() 兜底初始化。
    pub fn load(path: &Path) -> crate::error::Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return Err(crate::error::NaxOneError::Config(format!("Failed to read config: {e}"))),
        };
        match toml::from_str::<Self>(&content) {
            Ok(c) => Ok(c),
            Err(e) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let corrupt = path.with_extension(format!("corrupt-{}.toml", ts));
                let _ = std::fs::rename(path, &corrupt);
                tracing::warn!(
                    "naxone.toml 解析失败（{}），原文件已重命名为 {}，本次启动将使用默认配置重建。",
                    e,
                    corrupt.display()
                );
                Err(crate::error::NaxOneError::Config(format!(
                    "naxone.toml 已损坏，已备份至 {}",
                    corrupt.display()
                )))
            }
        }
    }

    /// Save config to a TOML file
    pub fn save(&self, path: &Path) -> crate::error::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::error::NaxOneError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Create a default config for a fresh PHPStudy-compatible setup.
    /// `www_root` 由调用方传入（tauri 层计算：优先 exe 同级便携目录，
    /// 不可写时 fallback %APPDATA%）。core 不知道 exe 路径，所以不自己决定。
    pub fn default_with_phpstudy(phpstudy_path: PathBuf, www_root: PathBuf) -> Self {
        Self {
            general: GeneralConfig {
                data_dir: phpstudy_path.clone(),
                www_root,
                phpstudy_path: Some(phpstudy_path),
                auto_start: vec!["nginx".into(), "mysql".into()],
                log_dir: None,
                log_retention_days: 7,
                extra_install_paths: Vec::new(),
                package_install_root: None,
                global_php_version: None,
                stop_services_on_exit: false,
                ignored_system_tools: Vec::new(),
            },
            web_server: WebServerConfig::default(),
            mysql: MysqlConfig::default(),
            redis: RedisConfig::default(),
            php_runtime: PhpRuntimeConfig::default(),
            php_instances: HashMap::new(),
        }
    }
}
