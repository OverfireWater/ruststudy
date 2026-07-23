use std::path::Path;

use naxone_core::domain::service::{ServiceInstance, ServiceKind, ServiceOrigin, ServiceStatus};
use naxone_core::error::Result;

/// Scans a PHPStudy-compatible Extensions directory to discover installed packages
pub struct PhpStudyScanner;

impl PhpStudyScanner {
    /// Scan the PHPStudy Extensions directory and return discovered service instances
    pub fn scan(extensions_path: &Path) -> Result<Vec<ServiceInstance>> {
        let mut instances = Vec::new();

        if !extensions_path.exists() {
            return Ok(instances);
        }

        // Scan for PHP versions: Extensions/php/php85nts/, php74nts/, etc.
        // 优先从 `php.exe -n -v` 读真实版本，不要只信文件夹名
        // （PHPStudy 的 php84nts 实际可能装 8.4.12，php85nts 可能装 8.5.1）
        let php_dir = extensions_path.join("php");
        if php_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&php_dir) {
                let mut port = 9000u16;
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && path.join("php-cgi.exe").exists() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            let (folder_version, variant) = parse_php_dir_name(name);
                            // 跑 php.exe -n -v 读真实版本；失败就退化为文件夹名
                            let real_version = detect_real_php_version(&path)
                                .unwrap_or(folder_version);
                            let config_path = resolve_config(&path, &["php.ini"]);
                            instances.push(ServiceInstance {
                                kind: ServiceKind::Php,
                                version: real_version,
                                variant: Some(variant),
                                install_path: path,
                                config_path,
                                port,
                                status: ServiceStatus::Stopped,
                                auto_start: false,
                                origin: ServiceOrigin::PhpStudy,
                                php_runtime: None,
                            });
                            port += 1;
                        }
                    }
                }
            }
        }

        // Scan for Nginx: Extensions/Nginx*/
        scan_service_dirs(
            extensions_path,
            "Nginx",
            "nginx.exe",
            ServiceKind::Nginx,
            80,
            &["conf/nginx.conf"],
            &mut instances,
        );

        // Scan for Apache: Extensions/Apache*/
        scan_service_dirs(
            extensions_path,
            "Apache",
            "bin/httpd.exe",
            ServiceKind::Apache,
            80,
            &["conf/httpd.conf"],
            &mut instances,
        );

        // Scan for MySQL: Extensions/MySQL*/
        scan_service_dirs(
            extensions_path,
            "MySQL",
            "bin/mysqld.exe",
            ServiceKind::Mysql,
            3306,
            &["my.ini", "my.cnf"],
            &mut instances,
        );

        // Scan for Redis: Extensions/redis*/
        scan_service_dirs(
            extensions_path,
            "redis",
            "redis-server.exe",
            ServiceKind::Redis,
            6379,
            &["redis.windows.conf", "redis.conf"],
            &mut instances,
        );

        tracing::info!(
            count = instances.len(),
            "Scanned PHPStudy Extensions directory"
        );

        Ok(instances)
    }
}

fn scan_service_dirs(
    extensions_path: &Path,
    prefix: &str,
    exe_relative: &str,
    kind: ServiceKind,
    default_port: u16,
    config_candidates: &[&str],
    instances: &mut Vec<ServiceInstance>,
) {
    let entries = match std::fs::read_dir(extensions_path) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("扫描 {} 失败: {} —— 该类型服务可能不可见", extensions_path.display(), e);
            return;
        }
    };
    let prefix_lower = prefix.to_lowercase();
    for entry_result in entries {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("读 {} 子项失败: {}（已跳过该项）", extensions_path.display(), e);
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.to_lowercase().starts_with(&prefix_lower) && path.join(exe_relative).exists()
            {
                let version = extract_version(name, prefix);
                let config_path = resolve_config(&path, config_candidates);
                instances.push(ServiceInstance {
                    kind,
                    version,
                    variant: None,
                    install_path: path,
                    config_path,
                    port: default_port,
                    status: ServiceStatus::Stopped,
                    auto_start: false,
                    origin: ServiceOrigin::PhpStudy,
                    php_runtime: None,
                });
            }
        }
    }
}

/// Try to find a config file from a list of candidates
fn resolve_config(base: &Path, candidates: &[&str]) -> Option<std::path::PathBuf> {
    for candidate in candidates {
        let p = base.join(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Spawn `php.exe -n -v` and parse the first line like "PHP 8.5.1 (cli) (...)".
/// Returns the full dotted version ("8.5.1") when parseable, else None.
///
/// `-n` skips php.ini so we don't print extension warnings and we don't fail
/// if ini is broken. Timeout is 3s; if php.exe hangs we bail out.
pub(crate) fn detect_real_php_version(php_install_dir: &Path) -> Option<String> {
    let exe = php_install_dir.join("php.exe");
    if !exe.exists() {
        return None;
    }
    // 3-second timeout via a background thread + Child::kill. std::process has no
    // native timeout, so spawn + poll is the cleanest way without tokio here.
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(&exe);
    cmd.arg("-n").arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // release 模式不弹 CMD 窗口
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd.spawn().ok()?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stdout_handle = child.stdout.take()?;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(_) => return None,
        }
    }
    // Child exited. Read stdout.
    use std::io::Read;
    let mut buf = String::new();
    stdout_handle.read_to_string(&mut buf).ok()?;
    parse_php_v_output(&buf)
}

/// 从 "PHP 8.5.1 (cli) ..." 抽出 "8.5.1"
fn parse_php_v_output(output: &str) -> Option<String> {
    let first_line = output.lines().next()?.trim();
    let rest = first_line.strip_prefix("PHP ")?;
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if ver.is_empty() || !ver.contains('.') {
        return None;
    }
    Some(ver)
}

/// Parse PHP directory name like "php85nts" -> ("8.5", "nts")
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
        // "85" -> "8.5", "74" -> "7.4", "734" -> "7.3.4"
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

/// Extract version from directory name like "Nginx1.15.11" -> "1.15.11"
fn extract_version(name: &str, prefix: &str) -> String {
    let after = if name.len() > prefix.len() {
        &name[prefix.len()..]
    } else {
        ""
    };
    after.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_php_dir_name() {
        assert_eq!(parse_php_dir_name("php85nts"), ("8.5".into(), "nts".into()));
        assert_eq!(
            parse_php_dir_name("php7.4.3nts"),
            ("7.4.3".into(), "nts".into())
        );
        assert_eq!(
            parse_php_dir_name("php84nts"),
            ("8.4".into(), "nts".into())
        );
    }

    #[test]
    fn parse_php_v_output_extracts_dotted_version() {
        let full = "PHP 8.5.1 (cli) (built: Dec 16 2025 16:25:55) (NTS Visual C++ 2022 x64)\nCopyright (c) The PHP Group\n";
        assert_eq!(parse_php_v_output(full), Some("8.5.1".into()));
        let full2 = "PHP 7.4.33 (cli) (built: Nov 16 2022 ...)";
        assert_eq!(parse_php_v_output(full2), Some("7.4.33".into()));
        // 异常输入
        assert_eq!(parse_php_v_output(""), None);
        assert_eq!(parse_php_v_output("garbage"), None);
        // 只有单段数字（无点）也拒绝
        assert_eq!(parse_php_v_output("PHP 8 (cli)"), None);
    }
}
