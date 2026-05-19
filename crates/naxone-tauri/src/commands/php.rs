use serde::Serialize;
use tauri::State;

use crate::commands::logger::push_log;
use crate::state::AppState;
use naxone_core::domain::log::LogLevel;
use naxone_core::domain::php::{PhpExtension, PhpIniSettings};
use naxone_core::domain::service::ServiceKind;

#[derive(Debug, Clone, Serialize)]
pub struct PhpInstanceInfo {
    pub id: String,
    pub label: String,
    pub version: String,
    pub variant: Option<String>,
    pub install_path: String,
}

#[tauri::command]
pub async fn get_php_instances(state: State<'_, AppState>) -> Result<Vec<PhpInstanceInfo>, String> {
    let services = state.services.read().await;
    let instances: Vec<PhpInstanceInfo> = services
        .iter()
        .filter(|s| s.kind == ServiceKind::Php)
        .map(|s| PhpInstanceInfo {
            id: s.id(),
            label: format!("PHP {} {}", s.version, s.variant.as_deref().unwrap_or("")),
            version: s.version.clone(),
            variant: s.variant.clone(),
            install_path: s.install_path.display().to_string(),
        })
        .collect();
    Ok(instances)
}

#[tauri::command]
pub async fn get_php_extensions(
    install_path: String,
    state: State<'_, AppState>,
) -> Result<Vec<PhpExtension>, String> {
    state
        .php_manager
        .list_extensions(std::path::Path::new(&install_path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn toggle_php_extension(
    install_path: String,
    ext_name: String,
    enable: bool,
    is_zend: bool,
    state: State<'_, AppState>,
) -> Result<Vec<PhpExtension>, String> {
    state.php_manager.toggle_extension(std::path::Path::new(&install_path), &ext_name, enable, is_zend).map_err(|e| e.to_string())?;
    push_log(&state, LogLevel::Success, "extension",
        if enable { format!("启用扩展 {}", ext_name) } else { format!("禁用扩展 {}", ext_name) },
        None, None).await;

    state.php_manager.list_extensions(std::path::Path::new(&install_path)).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_php_ini_settings(
    install_path: String,
    state: State<'_, AppState>,
) -> Result<PhpIniSettings, String> {
    state
        .php_manager
        .read_ini_settings(std::path::Path::new(&install_path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_php_ini_settings(
    install_path: String,
    settings: PhpIniSettings,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.php_manager.save_ini_settings(std::path::Path::new(&install_path), &settings).map_err(|e| e.to_string())?;
    push_log(&state, LogLevel::Success, "config", "保存 PHP 配置", None, None).await;
    Ok(())
}

// ==================== HTML phpinfo ====================

#[derive(Debug, Clone, Serialize)]
pub struct PhpInfoHtml {
    /// phpinfo() 完整 HTML（含官方默认 <style> 块）。前端用 <iframe srcdoc> 隔离渲染。
    pub html: String,
    /// 同步写到 %TEMP% 的副本，给「在浏览器打开」按钮用。形如 file:///C:/.../naxone-phpinfo-XXXX.html
    pub file_url: String,
}

/// 用 `php-cgi.exe -q -r 'phpinfo();'` 拿官方 HTML 版 phpinfo。
/// 必须走 CGI SAPI —— CLI SAPI 的 phpinfo() 是纯文本输出，没有表格。
/// 同时把结果写到 %TEMP%，返回 file:// URL 供「在浏览器打开」按钮用。
#[tauri::command]
pub async fn get_phpinfo_html(install_path: String) -> Result<PhpInfoHtml, String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::process::Command;
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    let install = std::path::PathBuf::from(&install_path);
    let cgi_exe = install.join("php-cgi.exe");
    if !cgi_exe.is_file() {
        return Err(format!(
            "php-cgi.exe 不存在: {}（HTML 版 phpinfo 必须走 CGI SAPI）",
            cgi_exe.display()
        ));
    }
    let ini = ["php.ini", "php.ini-production", "php.ini-development"]
        .iter()
        .map(|n| install.join(n))
        .find(|p| p.is_file());
    let ext_dir = install.join("ext");

    let mut cmd = Command::new(&cgi_exe);
    cmd.arg("-q"); // suppress HTTP headers, 输出纯 HTML
    if let Some(ini_path) = &ini {
        cmd.arg("-c").arg(ini_path);
    }
    if ext_dir.is_dir() {
        cmd.arg("-d").arg(format!("extension_dir={}", ext_dir.display()));
    }
    // PHP 8 + Windows ASLR 下 OPcache 在 php-cgi 进程会 fatal "Opcode handlers are unusable"，
    // 这里只是查 phpinfo，opcache 没用处，强制关掉避免拿到空 stdout。
    cmd.arg("-d").arg("opcache.enable=0");
    cmd.arg("-d").arg("opcache.enable_cli=0");
    cmd.arg("-i"); // -i = phpinfo()，CGI SAPI 下走 HTML 分支
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("启动 php-cgi.exe 失败: {}", e))?;
    let mut html = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // -q 已经吃掉 HTTP 头，但有些 build 头还会残留。从第一个 <!DOCTYPE / <html 截。
    let lower = html.to_ascii_lowercase();
    if !lower.trim_start().starts_with("<!doctype") && !lower.trim_start().starts_with("<html") {
        if let Some(idx) = lower.find("<!doctype").or_else(|| lower.find("<html")) {
            html = html[idx..].to_string();
        }
    }

    // 若 stdout 里根本没 HTML（php-cgi fatal 退出等），把错误反馈给前端，别静默给空白
    if !html.to_ascii_lowercase().contains("<html") {
        let code = out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into());
        return Err(format!(
            "php-cgi.exe 退出 code={} 但没有 HTML 输出\nstdout: {}\nstderr: {}",
            code,
            html.chars().take(500).collect::<String>(),
            stderr.chars().take(500).collect::<String>(),
        ));
    }

    let mut hasher = DefaultHasher::new();
    install_path.hash(&mut hasher);
    let temp_file = std::env::temp_dir().join(format!("naxone-phpinfo-{:x}.html", hasher.finish()));
    std::fs::write(&temp_file, &html).map_err(|e| format!("写入临时文件失败: {}", e))?;
    let file_url = format!("file:///{}", temp_file.display().to_string().replace('\\', "/"));

    Ok(PhpInfoHtml { html, file_url })
}

// ==================== 全局 PHP CLI 版本 ====================

#[derive(Debug, Clone, Serialize)]
pub struct GlobalPhpInfo {
    /// 当前活跃版本的 version 字符串（如 "8.5.5"）；未设置时为 null
    pub version: Option<String>,
    /// shim 所在目录，如 "C:\\Users\\xx\\.naxone\\bin"
    pub bin_dir: String,
    /// 该目录是否已在用户 PATH
    pub path_registered: bool,
    /// 系统 PATH (HKLM) 里会**屏蔽** shim 的 PHP 目录列表。
    /// 非空意味着全局切换不生效（Windows 解析 PATH 系统在前、用户在后），
    /// 用户需要手动从系统环境变量里清掉这些条目。
    pub conflicts: Vec<String>,
}

#[tauri::command]
pub async fn get_global_php_version(state: State<'_, AppState>) -> Result<GlobalPhpInfo, String> {
    build_global_php_info(&state).await
}

#[tauri::command]
pub async fn set_global_php_version(
    version: String,
    state: State<'_, AppState>,
) -> Result<GlobalPhpInfo, String> {
    #[cfg(target_os = "windows")]
    {
        use naxone_adapters::platform::global_php;

        // 1) 找到对应的 PHP ServiceInstance
        let install_path = {
            let services = state.services.read().await;
            services
                .iter()
                .find(|s| s.kind == ServiceKind::Php && s.version == version)
                .map(|s| s.install_path.clone())
                .ok_or_else(|| format!("未找到 PHP v{}", version))?
        };

        // 2) 写 shim
        global_php::write_shims(&install_path)
            .map_err(|e| format!("写入 shim 失败: {}", e))?;

        // 3) 确保 PATH 注册（首次会真写注册表）
        let changed = global_php::ensure_path_in_user_env()
            .map_err(|e| format!("写入 HKCU PATH 失败: {}", e))?;

        // 4) 持久化 config.global_php_version
        {
            let mut config = state.config.write().await;
            config.general.global_php_version = Some(version.clone());
            let cfg_path = crate::state::config_path();
            if let Err(e) = config.save(&cfg_path) {
                tracing::warn!("持久化 global_php_version 失败: {}", e);
            }
        }

        let detail = if changed {
            Some(format!(
                "已追加 PATH 条目：{}。请**新开**命令行窗口让它生效。",
                global_php::bin_dir().display()
            ))
        } else {
            None
        };
        push_log(
            &state,
            LogLevel::Success,
            "php",
            format!("全局 PHP 切到 v{}", version),
            detail,
            None,
        )
        .await;

        return build_global_php_info(&state).await;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = version;
        let _ = state;
        Err("非 Windows 平台暂不支持全局 PHP 切换".into())
    }
}

#[tauri::command]
pub async fn fix_global_php_conflicts(
    paths: Vec<String>,
    state: State<'_, AppState>,
) -> Result<GlobalPhpInfo, String> {
    #[cfg(target_os = "windows")]
    {
        use naxone_adapters::platform::global_php;
        if paths.is_empty() {
            return build_global_php_info(&state).await;
        }
        let pbs: Vec<std::path::PathBuf> =
            paths.iter().map(std::path::PathBuf::from).collect();
        global_php::fix_masking_paths(&pbs)?;
        push_log(
            &state,
            LogLevel::Success,
            "php",
            format!("清理系统 PATH 冲突 ×{}", paths.len()),
            Some(paths.join("\n")),
            None,
        )
        .await;
        return build_global_php_info(&state).await;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (paths, state);
        Err("仅 Windows 支持".into())
    }
}

#[tauri::command]
pub async fn open_system_env_editor() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        naxone_adapters::platform::global_php::open_env_editor()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("仅 Windows 支持".into())
    }
}

async fn build_global_php_info(state: &State<'_, AppState>) -> Result<GlobalPhpInfo, String> {
    #[cfg(target_os = "windows")]
    {
        use naxone_adapters::platform::global_php;
        let cfg_version = state.config.read().await.general.global_php_version.clone();
        let conflicts: Vec<String> = global_php::detect_masking_paths()
            .into_iter()
            .map(|p| p.display().to_string())
            .collect();
        return Ok(GlobalPhpInfo {
            version: cfg_version,
            bin_dir: global_php::bin_dir().display().to_string(),
            path_registered: global_php::is_path_registered(),
            conflicts,
        });
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = state;
        Ok(GlobalPhpInfo {
            version: None,
            bin_dir: String::new(),
            path_registered: false,
            conflicts: Vec::new(),
        })
    }
}
