//! Scanner for standalone (non-PHPStudy) service installs.
//!
//! Two sources are combined:
//! 1. `extra_install_paths` — user-specified paths in config (one path = one service)
//! 2. `%APPDATA%/NaxOne/Packages/{name}/{version}/` — things installed via the in-app store
//!
//! The "store" path scanner is skeletal for phase 1 (no store yet), but the
//! directory layout is fixed so future installs light up automatically.

use std::path::{Path, PathBuf};

use naxone_core::config::ExtraInstallPath;
use naxone_core::domain::service::{ServiceInstance, ServiceKind, ServiceOrigin, ServiceStatus};

pub struct StandaloneScanner;

impl StandaloneScanner {
    /// Scan every registered extra-install path + the packages root.
    pub fn scan(
        extras: &[ExtraInstallPath],
        packages_root: Option<&Path>,
    ) -> Vec<ServiceInstance> {
        let mut out = Vec::new();

        for extra in extras {
            if let Some(inst) = probe_install(&extra.path, extra.kind, ServiceOrigin::Manual) {
                out.push(inst);
            }
        }

        if let Some(root) = packages_root {
            out.extend(scan_packages_root(root));
        }

        out
    }
}

/// Scan `%APPDATA%/NaxOne/Packages/` layout:
///     Packages/{name}/{version}/  — one install each
fn scan_packages_root(root: &Path) -> Vec<ServiceInstance> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };

    for entry in entries.flatten() {
        let name_dir = entry.path();
        if !name_dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let kind = match name.as_str() {
            "nginx" => ServiceKind::Nginx,
            "apache" | "httpd" => ServiceKind::Apache,
            "mysql" => ServiceKind::Mysql,
            "redis" => ServiceKind::Redis,
            "php" => ServiceKind::Php,
            _ => continue,
        };

        let Ok(version_dirs) = std::fs::read_dir(&name_dir) else {
            continue;
        };
        for vdir in version_dirs.flatten() {
            let version_path = vdir.path();
            if !version_path.is_dir() {
                continue;
            }
            if let Some(mut inst) = probe_install(&version_path, kind, ServiceOrigin::Store) {
                // Trust the directory name as the authoritative version if the
                // probe couldn't deduce one (common for Nginx).
                if inst.version.is_empty() {
                    inst.version = vdir.file_name().to_string_lossy().to_string();
                }
                out.push(inst);
            }
        }
    }

    out
}

/// Probe a path to see whether it looks like a valid install of `kind`.
/// The `install_path` returned is always the directory that contains `conf/`,
/// `bin/`, etc. — i.e. the same layout as PhpStudy's scanner.
pub(crate) fn probe_install(path: &Path, kind: ServiceKind, origin: ServiceOrigin) -> Option<ServiceInstance> {
    if !path.exists() {
        return None;
    }

    // Normalise: if the user pointed at a zip's top-level ("nginx-1.25.3")
    // wrapper, try that too. Accept both `<path>` and `<path>/<single-subdir>`.
    let candidate = if looks_like_install(path, kind) {
        path.to_path_buf()
    } else {
        match single_subdir(path) {
            Some(sub) if looks_like_install(&sub, kind) => sub,
            _ => return None,
        }
    };

    let (config_path, version, variant) = inspect(&candidate, kind);
    let port = kind.default_port();

    Some(ServiceInstance {
        kind,
        version,
        variant,
        install_path: candidate,
        config_path,
        port,
        status: ServiceStatus::Stopped,
        auto_start: false,
        origin,
        php_runtime: None,
    })
}

pub(crate) fn looks_like_install(path: &Path, kind: ServiceKind) -> bool {
    match kind {
        ServiceKind::Nginx => path.join("nginx.exe").exists(),
        ServiceKind::Apache => path.join("bin").join("httpd.exe").exists(),
        ServiceKind::Mysql => path.join("bin").join("mysqld.exe").exists(),
        ServiceKind::Redis => {
            path.join("redis-server.exe").exists()
                || path.join("bin").join("redis-server.exe").exists()
        }
        ServiceKind::Php => path.join("php-cgi.exe").exists() || path.join("php.exe").exists(),
    }
}

/// If `path` contains exactly one subdirectory (e.g. an unextracted zip's
/// `nginx-1.25.3/`), return that directory. Otherwise None.
fn single_subdir(path: &Path) -> Option<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(path).ok()?.flatten().collect();
    let dirs: Vec<_> = entries.iter().filter(|e| e.path().is_dir()).collect();
    if dirs.len() == 1 {
        Some(dirs[0].path())
    } else {
        None
    }
}

fn inspect(
    path: &Path,
    kind: ServiceKind,
) -> (Option<PathBuf>, String, Option<String>) {
    match kind {
        ServiceKind::Nginx => {
            let conf = path.join("conf").join("nginx.conf");
            let conf = if conf.exists() { Some(conf) } else { None };
            (conf, extract_version_from_dirname(path), None)
        }
        ServiceKind::Apache => {
            let conf = path.join("conf").join("httpd.conf");
            let conf = if conf.exists() { Some(conf) } else { None };
            (conf, extract_version_from_dirname(path), None)
        }
        ServiceKind::Mysql => {
            let mut conf = None;
            for c in ["my.ini", "my.cnf"] {
                let p = path.join(c);
                if p.exists() {
                    conf = Some(p);
                    break;
                }
            }
            (conf, extract_version_from_dirname(path), None)
        }
        ServiceKind::Redis => {
            let mut conf = None;
            for c in ["redis.windows.conf", "redis.conf"] {
                let p = path.join(c);
                if p.exists() {
                    conf = Some(p);
                    break;
                }
            }
            (conf, extract_version_from_dirname(path), None)
        }
        ServiceKind::Php => {
            let conf = path.join("php.ini");
            let conf = if conf.exists() { Some(conf) } else { None };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let (folder_version, variant) = parse_php_dir_name(name);
            // 优先 spawn php.exe -n -v 读真实版本（文件夹名可能撒谎），失败退化到 parse
            let real_version = super::scanner::detect_real_php_version(path)
                .unwrap_or(folder_version);
            (conf, real_version, Some(variant))
        }
    }
}

fn extract_version_from_dirname(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // For directories like "nginx-1.25.3" or "mysql-8.0.36-winx64" strip the prefix
    let stripped = name
        .trim_start_matches(|c: char| c.is_alphabetic() || c == '-')
        .trim_end_matches(|c: char| c.is_alphabetic() || c == '-');
    // Keep only the version-ish portion (digits and dots)
    let end = stripped
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(stripped.len());
    stripped[..end].trim_matches('.').to_string()
}

/// Parse PHP directory name like "php85nts" -> ("8.5", "nts")
/// (copied from the phpstudy scanner so this module stays self-contained)
fn parse_php_dir_name(name: &str) -> (String, String) {
    let name = name.strip_prefix("php").unwrap_or(name);
    let (digits, variant) = if let Some(pos) = name.find(|c: char| !c.is_ascii_digit() && c != '.') {
        (&name[..pos], name[pos..].to_string())
    } else {
        (name, "nts".to_string())
    };

    let version = if digits.contains('.') {
        digits.to_string()
    } else if digits.len() >= 2 {
        let chars: Vec<char> = digits.chars().collect();
        if chars.len() == 2 {
            format!("{}.{}", chars[0], chars[1])
        } else {
            format!("{}.{}.{}", chars[0], chars[1], &digits[2..])
        }
    } else {
        digits.to_string()
    };

    (version, variant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_version_from_dirname() {
        let p = Path::new("/x/nginx-1.25.3");
        assert_eq!(extract_version_from_dirname(p), "1.25.3");
    }
}
