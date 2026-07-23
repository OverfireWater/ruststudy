use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The kind of software component managed by NaxOne
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Nginx,
    Apache,
    Php,
    Mysql,
    Redis,
}

impl ServiceKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Nginx => "Nginx",
            Self::Apache => "Apache",
            Self::Php => "PHP-CGI",
            Self::Mysql => "MySQL",
            Self::Redis => "Redis",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            Self::Nginx => 80,
            Self::Apache => 80,
            Self::Php => 9000,
            Self::Mysql => 3306,
            Self::Redis => 6379,
        }
    }
}

/// Runtime status of a managed service
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ServiceStatus {
    Stopped,
    Starting,
    /// `memory_mb`: 进程工作集内存（MB）。None 表示查询失败或信息不可用。
    /// serde default 让旧持久化的 JSON 反序列化成功（旧 `Running { pid }` → `memory_mb: None`）
    Running {
        pid: u32,
        #[serde(default)]
        memory_mb: Option<u64>,
    },
    Stopping,
    Failed {
        reason: String,
    },
}

impl ServiceStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Where a service instance was discovered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum ServiceOrigin {
    /// Discovered inside a PHPStudy Extensions directory
    PhpStudy,
    /// Installed via the in-app software store
    Store,
    /// Pointed at by an extra_install_paths entry
    Manual,
    /// Auto-discovered from system PATH or common install directories
    System,
}

impl Default for ServiceOrigin {
    fn default() -> Self {
        Self::PhpStudy
    }
}

/// Runtime-only options used when launching a PHP FastCGI pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpRuntimeOptions {
    pub workers: u16,
    pub max_requests: u32,
}

/// A concrete, runnable service instance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInstance {
    pub kind: ServiceKind,
    pub version: String,
    pub variant: Option<String>,
    pub install_path: PathBuf,
    pub config_path: Option<PathBuf>,
    pub port: u16,
    pub status: ServiceStatus,
    pub auto_start: bool,
    /// Where this service was found (default = PhpStudy for backward compat)
    #[serde(default)]
    pub origin: ServiceOrigin,
    /// Set only for PHP versions selected by the active-vhost worker policy.
    #[serde(default)]
    pub php_runtime: Option<PhpRuntimeOptions>,
}

impl ServiceInstance {
    pub fn id(&self) -> String {
        match &self.variant {
            Some(v) => format!("{:?}-{}-{}", self.kind, self.version, v),
            None => format!("{:?}-{}", self.kind, self.version),
        }
        .to_lowercase()
    }
}
