use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::State;

use crate::commands::logger::push_log;
use crate::state::AppState;
use naxone_core::domain::log::LogLevel;
use naxone_core::domain::service::ServiceKind;
use naxone_core::domain::vhost::{is_safe_hostname, VhostSource, VirtualHost};
use naxone_core::use_cases::vhost_mgr::VhostManager;

fn persist_vhosts(vhosts: &[VirtualHost], state: &AppState) {
    let path = crate::state::vhosts_json_path();
    let _ = state.vhost_manager.save_vhosts_json(&path, vhosts);
}

/// 校验 document_root：必须是绝对路径，禁止指向系统保留目录。
/// 平台相关检查放在 commands 层，不污染 core。
fn validate_document_root(p: &std::path::Path) -> Result<(), String> {
    if !p.is_absolute() {
        return Err("网站目录必须是绝对路径".into());
    }
    let s = p.display().to_string().to_lowercase().replace('\\', "/");
    const BLOCKED: &[&str] = &[
        "c:/windows",
        "c:/program files",
        "c:/program files (x86)",
        "c:/programdata",
        "c:/$recycle.bin",
        "c:/boot",
        "c:/perflogs",
        "c:/system volume information",
    ];
    for b in BLOCKED {
        if s == *b || s.starts_with(&format!("{}/", b)) {
            return Err(format!("不允许将网站目录设在系统路径下: {}", p.display()));
        }
    }
    Ok(())
}

/// vhost 字段 + 路径双重校验，create/update 入口统一调用。
fn prevalidate_vhost(vhost: &VirtualHost) -> Result<(), String> {
    vhost.validate()?;
    validate_document_root(&vhost.document_root)?;
    Ok(())
}

// ==================== SSL 自签 ====================

#[derive(Debug, Serialize)]
pub struct GeneratedCert {
    pub cert_path: String,
    pub key_path: String,
}

/// 为 vhost 生成自签证书，写到 ~/.naxone/certs/{server_name}.{crt,key}
#[tauri::command]
pub async fn generate_self_signed_cert(
    server_name: String,
    aliases: Vec<String>,
) -> Result<GeneratedCert, String> {
    if server_name.trim().is_empty() {
        return Err("请先填写域名（server_name）".into());
    }
    // 防止通过证书命令绕开 vhost 校验路径写出非法文件名（哪怕底层 sanitize_filename 兜底了，
    // 也要在入口报错更明确，避免静默生成 `foo___bar.crt` 这种用户没想要的文件）
    if !is_safe_hostname(&server_name) {
        return Err(format!("域名格式不合法：{}", server_name));
    }
    for a in &aliases {
        if !a.trim().is_empty() && !is_safe_hostname(a) {
            return Err(format!("域名别名格式不合法：{}", a));
        }
    }
    // 证书存放目录：~/.naxone/certs/（dev 用 .naxone-dev/certs/）
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
    let out_dir = PathBuf::from(home).join(crate::state::naxone_home_dirname()).join("certs");

    let (cert, key) = naxone_adapters::platform::ssl_cert::generate_self_signed(
        &server_name,
        &aliases,
        &out_dir,
    )?;

    Ok(GeneratedCert {
        cert_path: cert.display().to_string(),
        key_path: key.display().to_string(),
    })
}

/// Best-effort：尝试给端口加 Windows 防火墙入站放行。
/// 本地访问（loopback）不经防火墙，浏览器访问 http://host.test:port 无需此规则；
/// 此规则仅在让局域网其他设备 / 手机访问时才需要。所以非管理员模式下失败也无所谓，
/// 静默处理即可。
async fn try_open_firewall(port: u16, state: &AppState) {
    if port == 80 {
        return;
    }
    match state.platform_ops.add_firewall_port(port) {
        Ok(_) => {
            push_log(
                state,
                LogLevel::Info,
                "vhost",
                format!("已放行防火墙端口 {}", port),
                Some("其他设备/手机可通过本机 IP 访问此端口".to_string()),
                None,
            )
            .await;
        }
        Err(e) => {
            // 不弹 Warn —— 防火墙规则只影响外部访问，本地浏览器访问站点不受影响。
            // 想让外部设备访问的用户自然会看不到端口通，那时再以管理员重启 NaxOne。
            tracing::debug!("防火墙端口 {} 未放行（非管理员模式正常）: {}", port, e);
        }
    }
}

/// Best-effort：端口不再被任何 vhost 使用时关闭对应防火墙规则。
/// 失败时静默处理（同 try_open_firewall）：留个废规则不构成安全风险，普通用户不必看到。
async fn try_close_firewall(port: u16, remaining_vhosts: &[VirtualHost], state: &AppState) {
    if port == 80 {
        return;
    }
    let still_used = remaining_vhosts.iter().any(|v| v.listen_port == port);
    if still_used {
        return;
    }
    if let Err(e) = state.platform_ops.remove_firewall_port(port) {
        tracing::debug!("防火墙端口 {} 关闭失败（非管理员模式正常）: {}", port, e);
    }
}

// --- DTOs ---

#[derive(Debug, Clone, Serialize)]
pub struct VhostInfo {
    pub id: String,
    pub server_name: String,
    pub aliases: Vec<String>,
    pub listen_port: u16,
    pub document_root: String,
    pub php_version: Option<String>,
    pub index_files: String,
    pub rewrite_rule: String,
    pub autoindex: bool,
    pub has_ssl: bool,
    pub ssl_cert: String,
    pub ssl_key: String,
    pub force_https: bool,
    pub custom_directives: String,
    pub access_log: String,
    pub enabled: bool,
    pub created_at: String,
    pub expires_at: String,
    /// "phpstudy" | "custom" | "standalone"
    pub source: String,
    /// Package name when source = "standalone" (e.g. "nginx")
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateVhostRequest {
    pub server_name: String,
    pub aliases: String,
    pub listen_port: u16,
    pub document_root: String,
    pub php_version: Option<String>,
    #[serde(default = "default_index_files")]
    pub index_files: String,
    #[serde(default)]
    pub rewrite_rule: String,
    #[serde(default)]
    pub autoindex: bool,
    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,
    #[serde(default)]
    pub force_https: bool,
    pub custom_directives: Option<String>,
    pub access_log: Option<String>,
    #[serde(default = "default_sync_hosts")]
    pub sync_hosts: bool,
    #[serde(default)]
    pub expires_at: String,
}

fn default_sync_hosts() -> bool {
    true
}

fn default_index_files() -> String {
    "index.php index.html".into()
}

#[derive(Debug, Clone, Serialize)]
pub struct PhpVersionInfo {
    pub label: String,
    pub version: String,
    pub port: u16,
    pub install_path: String,
}

// --- Conversions ---

fn to_info(v: &VirtualHost) -> VhostInfo {
    let (source, source_name) = match &v.source {
        VhostSource::PhpStudy => ("phpstudy".into(), None),
        VhostSource::Custom => ("custom".into(), None),
        VhostSource::Standalone(name) => ("standalone".into(), Some(name.clone())),
    };
    VhostInfo {
        id: v.id.clone(),
        server_name: v.server_name.clone(),
        aliases: v.aliases.clone(),
        listen_port: v.listen_port,
        document_root: v.document_root.display().to_string(),
        php_version: v.php_version.clone(),
        index_files: v.index_files.clone(),
        rewrite_rule: v.rewrite_rule.clone(),
        autoindex: v.autoindex,
        has_ssl: v.ssl.is_some(),
        ssl_cert: v.ssl.as_ref().map(|s| s.cert_path.display().to_string()).unwrap_or_default(),
        ssl_key: v.ssl.as_ref().map(|s| s.key_path.display().to_string()).unwrap_or_default(),
        force_https: v.ssl.as_ref().map(|s| s.force_https).unwrap_or(false),
        custom_directives: v.custom_directives.clone().unwrap_or_default(),
        access_log: v.access_log.clone().unwrap_or_default(),
        enabled: v.enabled,
        created_at: v.created_at.clone(),
        expires_at: v.expires_at.clone(),
        source,
        source_name,
    }
}

fn all_infos(vhosts: &[VirtualHost]) -> Vec<VhostInfo> {
    vhosts.iter().map(to_info).collect()
}

/// Build a VirtualHost from a CreateVhostRequest, resolving PHP version to port and path.
/// 选了 PHP 版本但在 services 找不到 → 返回 Err。否则会创建一个无 fastcgi 块的 vhost，
/// 浏览器访问 .php 直接下载 raw 内容，用户排错很难。让用户先去解决 services 扫描的问题
/// （如 phpstudy_path 配错、php 包卸载未重扫等）。
fn build_vhost(
    req: &CreateVhostRequest,
    _state: &AppState,
    services: &[naxone_core::domain::service::ServiceInstance],
) -> Result<VirtualHost, String> {
    let aliases: Vec<String> = req
        .aliases
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    let id = format!("{}_{}", req.server_name, req.listen_port);

    // Resolve PHP version to port and install path
    let (php_fastcgi_port, php_install_path) = if let Some(ref php_ver) = req.php_version {
        let php_inst = services.iter().find(|s| {
            if s.kind != ServiceKind::Php {
                return false;
            }
            let inst_ver = if let Some(ref variant) = s.variant {
                format!("php{}{}", s.version.replace('.', ""), variant)
            } else {
                format!("php{}", s.version.replace('.', ""))
            };
            &inst_ver == php_ver
        });
        match php_inst {
            Some(inst) => (Some(inst.port), Some(inst.install_path.clone())),
            None => {
                return Err(format!(
                    "选择的 PHP 版本 '{}' 不在已扫描服务中。\n常见原因：\n1. 「设置」里 PHPStudy 路径配错（拼写错误等）\n2. 该 PHP 已卸载但没重新扫描\n请到「设置」检查路径并重新扫描服务后重试。",
                    php_ver
                ));
            }
        }
    } else {
        (None, None)
    };

    let ssl = match (&req.ssl_cert, &req.ssl_key) {
        (Some(cert), Some(key)) if !cert.is_empty() && !key.is_empty() => {
            Some(naxone_core::domain::vhost::SslConfig {
                cert_path: PathBuf::from(cert),
                key_path: PathBuf::from(key),
                force_https: req.force_https,
            })
        }
        _ => None,
    };

    Ok(VirtualHost {
        id,
        server_name: req.server_name.clone(),
        aliases,
        listen_port: req.listen_port,
        document_root: PathBuf::from(&req.document_root),
        php_version: req.php_version.clone(),
        php_fastcgi_port,
        php_install_path,
        index_files: if req.index_files.is_empty() { "index.php index.html".into() } else { req.index_files.clone() },
        rewrite_rule: req.rewrite_rule.clone(),
        autoindex: req.autoindex,
        ssl,
        custom_directives: req.custom_directives.clone().filter(|s| !s.is_empty()),
        access_log: req.access_log.clone().filter(|s| !s.is_empty()),
        enabled: true,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        expires_at: req.expires_at.clone(),
        sync_hosts: req.sync_hosts,
        source: VhostSource::Custom,
    })
}

// --- Helper: resolve vhost dirs ---

struct VhostDirs {
    nginx_install: Option<PathBuf>,
    nginx_vhosts: Option<PathBuf>,
    apache_vhosts: Option<PathBuf>,
    apache_listen: Option<PathBuf>,
}

async fn resolve_dirs(state: &AppState) -> Result<VhostDirs, String> {
    let services = state.services.read().await;
    let nginx_install = services
        .iter()
        .find(|s| s.kind == ServiceKind::Nginx)
        .map(|s| s.install_path.clone());
    let apache_install = services
        .iter()
        .find(|s| s.kind == ServiceKind::Apache)
        .map(|s| s.install_path.clone());
    drop(services);

    let (mut nginx_dir, mut apache_dir) = (nginx_install, apache_install);

    if nginx_dir.is_none() || apache_dir.is_none() {
        let config = state.config.read().await;
        if let Some(phpstudy) = config.general.phpstudy_path.as_ref() {
            let ext = phpstudy.join("Extensions");
            if nginx_dir.is_none() {
                nginx_dir = find_extension_dir(&ext, "Nginx");
            }
            if apache_dir.is_none() {
                apache_dir = find_extension_dir(&ext, "Apache");
            }
        }
    }

    if nginx_dir.is_none() && apache_dir.is_none() {
        return Err("未检测到可用的 Web 服务器，请先安装或扫描 Nginx / Apache".into());
    }

    Ok(VhostDirs {
        nginx_install: nginx_dir.clone(),
        nginx_vhosts: nginx_dir.map(|dir| dir.join("conf").join("vhosts")),
        apache_vhosts: apache_dir
            .as_ref()
            .map(|dir| dir.join("conf").join("vhosts")),
        apache_listen: apache_dir
            .map(|dir| dir.join("conf").join("vhosts").join("Listen.conf")),
    })
}

/// 确保 `<install>/conf/nginx.conf` 含 `include vhosts/*.conf;`，并创建 vhosts 子目录。
/// 商店装的官方 nginx 默认 conf 不含这行，NaxOne 写的 vhost 不会被加载。
/// 幂等：检查到已 include 立即返回 Ok；写入前先 .bak 备份。
pub fn ensure_nginx_vhosts_include(install: &std::path::Path) -> Result<(), String> {
    let conf_dir = install.join("conf");
    let vhosts_dir = conf_dir.join("vhosts");
    if !vhosts_dir.exists() {
        std::fs::create_dir_all(&vhosts_dir)
            .map_err(|e| format!("创建 vhosts 目录失败: {}", e))?;
    }

    let nginx_conf = conf_dir.join("nginx.conf");
    if !nginx_conf.is_file() {
        // 没有 nginx.conf 不算这里的事（用户场景不应发生），交给上层报错
        return Ok(());
    }

    let content = std::fs::read_to_string(&nginx_conf)
        .map_err(|e| format!("读 nginx.conf 失败: {}", e))?;

    // 任何形态的 "include vhosts/" 都视作已配置
    if content.contains("include vhosts/") || content.contains("include  vhosts/") {
        return Ok(());
    }

    // 找 "http {"，在它后面插入一行 include。简单字符串匹配能覆盖官方 conf
    // 与 PHPStudy conf 的通用写法。
    let marker = content.find("http {").or_else(|| content.find("http  {"));
    let Some(http_pos) = marker else {
        return Err("nginx.conf 中找不到 http 块，无法自动插入 include vhosts/*.conf;".into());
    };
    let insert_at = content[http_pos..]
        .find('\n')
        .map(|n| http_pos + n + 1)
        .unwrap_or(content.len());

    let mut new_content = String::with_capacity(content.len() + 32);
    new_content.push_str(&content[..insert_at]);
    new_content.push_str("    include vhosts/*.conf;\n");
    new_content.push_str(&content[insert_at..]);

    // .bak 备份
    let bak = nginx_conf.with_extension("conf.bak");
    let _ = std::fs::write(&bak, content.as_bytes());

    std::fs::write(&nginx_conf, new_content.as_bytes())
        .map_err(|e| format!("写 nginx.conf 失败: {}", e))?;

    Ok(())
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

// --- Commands ---

#[tauri::command]
pub async fn get_vhosts(state: State<'_, AppState>) -> Result<Vec<VhostInfo>, String> {
    let vhosts = state.vhosts.read().await;
    Ok(all_infos(&vhosts))
}

#[derive(Debug, Serialize)]
pub struct VhostConfFile {
    pub path: String,
    pub content: String,
}

#[tauri::command]
pub async fn read_vhost_conf(
    id: String,
    state: State<'_, AppState>,
) -> Result<VhostConfFile, String> {
    let dirs = resolve_dirs(&state).await?;
    let nginx_dir = dirs.nginx_vhosts.ok_or_else(|| "未找到 nginx vhosts 目录".to_string())?;

    let vhosts = state.vhosts.read().await;
    let vh = vhosts
        .iter()
        .find(|v| v.id == id)
        .ok_or_else(|| format!("找不到站点 {}", id))?;
    let filename = vh.config_filename();
    let path = nginx_dir.join(&filename);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
    Ok(VhostConfFile {
        path: path.display().to_string(),
        content,
    })
}

#[tauri::command]
pub async fn write_vhost_conf(
    id: String,
    content: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let dirs = resolve_dirs(&state).await?;
    let nginx_dir = dirs.nginx_vhosts.ok_or_else(|| "未找到 nginx vhosts 目录".to_string())?;

    let path = {
        let vhosts = state.vhosts.read().await;
        let vh = vhosts
            .iter()
            .find(|v| v.id == id)
            .ok_or_else(|| format!("找不到站点 {}", id))?;
        nginx_dir.join(vh.config_filename())
    };

    std::fs::write(&path, &content)
        .map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;

    push_log(
        &state,
        LogLevel::Info,
        "vhost",
        format!("手动编辑 nginx 配置：{}", id),
        Some(path.display().to_string()),
        None,
    )
    .await;

    // best-effort reload nginx
    let services = state.services.read().await;
    let running_ws = VhostManager::find_running_web_server(&services).cloned();
    drop(services);

    if let Some(ws) = running_ws {
        if let Err(e) = state.vhost_manager.reload_web_server(&ws).await {
            return Err(format!("文件已保存，但 nginx reload 失败: {}", e));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn create_vhost(
    req: CreateVhostRequest,
    state: State<'_, AppState>,
) -> Result<Vec<VhostInfo>, String> {
    let dirs = resolve_dirs(&state).await?;
    let services = state.services.read().await;
    let vhost = build_vhost(&req, &state, &services).map_err(|e| {
        let domain = format!("{}:{}", req.server_name, req.listen_port);
        tracing::warn!("创建站点 {} 被拒绝: {}", domain, e);
        e
    })?;

    if let Err(e) = prevalidate_vhost(&vhost) {
        push_log(&state, LogLevel::Error, "vhost", format!("创建站点 {}:{} 被拒绝", vhost.server_name, vhost.listen_port), Some(e.clone()), None).await;
        return Err(e);
    }

    if let Err(e) = std::fs::create_dir_all(&vhost.document_root) {
        let msg = format!("创建网站目录失败: {}", e);
        push_log(&state, LogLevel::Error, "vhost", format!("创建站点 {}:{} 失败", vhost.server_name, vhost.listen_port), Some(msg.clone()), None).await;
        return Err(msg);
    }

    // 兜底：确保 nginx.conf 含 include vhosts/*.conf;（商店包/官方包默认没有这行 → 浏览器看到 nginx 欢迎页）
    if let Some(nginx_install) = dirs.nginx_install.as_ref() {
        if let Err(e) = ensure_nginx_vhosts_include(nginx_install) {
            push_log(&state, LogLevel::Warn, "vhost",
                "nginx.conf 自动注入 include vhosts/*.conf 失败",
                Some(e), None).await;
        }
    }

    let running_ws = VhostManager::find_running_web_server(&services);

    let domain_port = format!("{}:{}", vhost.server_name, vhost.listen_port);

    // 端口冲突预检（避免写入配置后 reload 失败导致不一致）
    {
        let existing = state.vhosts.read().await;
        if let Err(e) = VhostManager::check_port_conflict(vhost.listen_port, &existing, None) {
            let msg = e.to_string();
            push_log(&state, LogLevel::Error, "vhost", format!("创建站点 {} 失败", domain_port), Some(msg.clone()), None).await;
            return Err(msg);
        }
    }

    match state.vhost_manager.create_vhost(&vhost, dirs.nginx_vhosts.as_deref(), dirs.apache_vhosts.as_deref(), dirs.apache_listen.as_deref(), running_ws).await {
        Ok(_) => {
            let details = format!("文档根: {}\nPHP: {}\n伪静态: {}\nSSL: {}",
                vhost.document_root.display(),
                vhost.php_version.as_deref().unwrap_or("无"),
                if vhost.rewrite_rule.is_empty() { "无" } else { "已设置" },
                if vhost.ssl.is_some() { "已启用" } else { "未启用" });
            push_log(&state, LogLevel::Success, "vhost", format!("创建站点 {}", domain_port), Some(details), None).await;
        }
        Err(e) => {
            let msg = e.to_string();
            push_log(&state, LogLevel::Error, "vhost", format!("创建站点 {} 失败", domain_port), Some(msg.clone()), None).await;
            return Err(msg);
        }
    }

    drop(services);

    // 防火墙放行（best-effort，失败不阻止站点创建）
    try_open_firewall(vhost.listen_port, &state).await;

    let mut vhosts = state.vhosts.write().await;
    vhosts.push(vhost);
    persist_vhosts(&vhosts, &state);
    Ok(all_infos(&vhosts))
}

#[tauri::command]
pub async fn update_vhost(
    id: String,
    req: CreateVhostRequest,
    state: State<'_, AppState>,
) -> Result<Vec<VhostInfo>, String> {
    let dirs = resolve_dirs(&state).await?;
    let services = state.services.read().await;

    let mut vhosts = state.vhosts.write().await;
    let idx = vhosts
        .iter()
        .position(|v| v.id == id)
        .ok_or_else(|| format!("Vhost not found: {}", id))?;

    let old = vhosts[idx].clone();
    let new_vhost = build_vhost(&req, &state, &services).map_err(|e| {
        tracing::warn!("更新站点 {}:{} 被拒绝: {}", req.server_name, req.listen_port, e);
        e
    })?;
    let running_ws = VhostManager::find_running_web_server(&services);

    let domain = new_vhost.server_name.clone();

    if let Err(e) = prevalidate_vhost(&new_vhost) {
        push_log(&state, LogLevel::Error, "vhost", format!("更新站点 {} 被拒绝", domain), Some(e.clone()), None).await;
        return Err(e);
    }

    // 端口冲突预检（仅当端口变更时）
    if old.listen_port != new_vhost.listen_port {
        if let Err(e) = VhostManager::check_port_conflict(new_vhost.listen_port, &vhosts, Some(&id)) {
            let msg = e.to_string();
            push_log(&state, LogLevel::Error, "vhost", format!("更新站点 {} 失败", domain), Some(msg.clone()), None).await;
            return Err(msg);
        }
    }

    if let Err(e) = state.vhost_manager.update_vhost(&old, &new_vhost, dirs.nginx_vhosts.as_deref(), dirs.apache_vhosts.as_deref(), dirs.apache_listen.as_deref(), &vhosts, running_ws).await {
        let msg = e.to_string();
        push_log(&state, LogLevel::Error, "vhost", format!("更新站点 {} 失败", domain), Some(msg.clone()), None).await;
        return Err(msg);
    }
    push_log(&state, LogLevel::Success, "vhost", format!("更新站点 {}", domain), None, None).await;

    let port_changed = old.listen_port != new_vhost.listen_port;
    vhosts[idx] = new_vhost.clone();
    persist_vhosts(&vhosts, &state);

    // 端口变更 → 新端口放行、旧端口若无人用则关闭
    if port_changed {
        let vhosts_snap = vhosts.clone();
        drop(vhosts);
        try_open_firewall(new_vhost.listen_port, &state).await;
        try_close_firewall(old.listen_port, &vhosts_snap, &state).await;
        let vhosts = state.vhosts.read().await;
        return Ok(all_infos(&vhosts));
    }
    Ok(all_infos(&vhosts))
}

#[tauri::command]
pub async fn delete_vhost(
    id: String,
    state: State<'_, AppState>,
) -> Result<Vec<VhostInfo>, String> {
    let dirs = resolve_dirs(&state).await?;
    let services = state.services.read().await;

    let mut vhosts = state.vhosts.write().await;
    let idx = vhosts
        .iter()
        .position(|v| v.id == id)
        .ok_or_else(|| format!("Vhost not found: {}", id))?;

    let vhost = &vhosts[idx];
    let running_ws = VhostManager::find_running_web_server(&services);

    let domain = vhost.server_name.clone();
    let deleted_port = vhost.listen_port;

    // 物理删除（.conf / hosts / listen.conf）——失败视为"整个删除失败"
    if let Err(e) = state.vhost_manager.delete_vhost(vhost, dirs.nginx_vhosts.as_deref(), dirs.apache_vhosts.as_deref(), dirs.apache_listen.as_deref(), &vhosts, running_ws).await {
        let msg = e.to_string();
        push_log(&state, LogLevel::Error, "vhost", format!("删除站点 {} 失败", domain), Some(msg.clone()), None).await;
        return Err(msg);
    }

    // 物理删除成功：立即从内存列表+持久化移除，避免 UI 卡在旧状态
    vhosts.remove(idx);
    persist_vhosts(&vhosts, &state);

    // 独立 reload：失败不回滚删除（删了就是删了），但要明确通知用户手动重启
    if let Some(ws) = running_ws {
        if let Err(e) = state.vhost_manager.reload_web_server(ws).await {
            push_log(
                &state,
                LogLevel::Warn,
                "vhost",
                format!("{} 已删除，但 {} reload 失败，请手动重启 Web 服务器",
                    domain, ws.kind.display_name()),
                Some(e.to_string()),
                None,
            ).await;
        } else {
            push_log(&state, LogLevel::Success, "vhost", format!("删除站点 {}", domain), None, None).await;
        }
    } else {
        push_log(&state, LogLevel::Success, "vhost", format!("删除站点 {}", domain), None, None).await;
    }

    // 若此端口不再被其他 vhost 使用 → 关闭防火墙规则（best-effort）
    let vhosts_snap = vhosts.clone();
    drop(vhosts);
    try_close_firewall(deleted_port, &vhosts_snap, &state).await;
    let vhosts = state.vhosts.read().await;
    Ok(all_infos(&vhosts))
}

/// Enable or disable a vhost by renaming .conf to .conf.disabled and vice versa
#[tauri::command]
pub async fn toggle_vhost(
    id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<Vec<VhostInfo>, String> {
    let dirs = resolve_dirs(&state).await?;
    let mut vhosts = state.vhosts.write().await;
    let idx = vhosts.iter().position(|v| v.id == id).ok_or("Vhost not found")?;

    let filename = vhosts[idx].config_filename();
    let mut renamed: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();

    if let Some(nginx_dir) = dirs.nginx_vhosts.as_ref() {
        let conf = nginx_dir.join(&filename);
        let disabled = nginx_dir.join(format!("{}.disabled", filename));
        if enabled {
            if disabled.exists() {
                if std::fs::rename(&disabled, &conf).is_ok() { renamed.push((conf.clone(), disabled.clone())); }
            }
        } else if conf.exists() {
            if std::fs::rename(&conf, &disabled).is_ok() { renamed.push((disabled.clone(), conf.clone())); }
        }
    }

    if let Some(apache_dir) = dirs.apache_vhosts.as_ref() {
        let conf = apache_dir.join(&filename);
        let disabled = apache_dir.join(format!("{}.disabled", filename));
        if enabled {
            if disabled.exists() {
                if std::fs::rename(&disabled, &conf).is_ok() { renamed.push((conf.clone(), disabled.clone())); }
            }
        } else if conf.exists() {
            if std::fs::rename(&conf, &disabled).is_ok() { renamed.push((disabled.clone(), conf.clone())); }
        }
    }

    let services = state.services.read().await;
    if let Some(ws) = VhostManager::find_running_web_server(&services) {
        if let Err(e) = state.vhost_manager.reload_web_server(ws).await {
            for (from, to) in renamed.iter().rev() {
                let _ = std::fs::rename(from, to);
            }
            return Err(format!("Reload 失败: {}", e));
        }
    }

    vhosts[idx].enabled = enabled;
    let domain = vhosts[idx].server_name.clone();
    push_log(&state, LogLevel::Success, "vhost",
        if enabled { format!("启用站点 {}", domain) } else { format!("停用站点 {}", domain) },
        None, None).await;

    persist_vhosts(&vhosts, &state);
    Ok(all_infos(&vhosts))
}

/// Check for expired vhosts and auto-disable them
#[tauri::command]
pub async fn check_expired_vhosts(state: State<'_, AppState>) -> Result<Vec<VhostInfo>, String> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dirs = resolve_dirs(&state).await?;
    let mut vhosts = state.vhosts.write().await;
    let mut changed = false;

    for vh in vhosts.iter_mut() {
        if vh.enabled && !vh.expires_at.is_empty() && vh.expires_at <= today {
            // Auto-disable expired vhost
            vh.enabled = false;
            let filename = vh.config_filename();
            if let Some(nginx_dir) = dirs.nginx_vhosts.as_ref() {
                let nginx_conf = nginx_dir.join(&filename);
                if nginx_conf.exists() {
                    let _ = std::fs::rename(&nginx_conf, nginx_dir.join(format!("{}.disabled", filename)));
                }
            }
            if let Some(apache_dir) = dirs.apache_vhosts.as_ref() {
                let apache_conf = apache_dir.join(&filename);
                if apache_conf.exists() {
                    let _ = std::fs::rename(&apache_conf, apache_dir.join(format!("{}.disabled", filename)));
                }
            }
            changed = true;
        }
    }

    if changed {
        persist_vhosts(&vhosts, &state);
        // Reload web server
        let services = state.services.read().await;
        if let Some(ws) = VhostManager::find_running_web_server(&services) {
            let _ = state.vhost_manager.reload_web_server(ws).await;
        }
    }

    Ok(all_infos(&vhosts))
}

#[tauri::command]
pub async fn get_php_versions(state: State<'_, AppState>) -> Result<Vec<PhpVersionInfo>, String> {
    let services = state.services.read().await;
    let php_versions: Vec<PhpVersionInfo> = services
        .iter()
        .filter(|s| s.kind == ServiceKind::Php)
        .map(|s| {
            let label = if let Some(ref variant) = s.variant {
                format!("PHP {} ({})", s.version, variant)
            } else {
                format!("PHP {}", s.version)
            };
            let version = if let Some(ref variant) = s.variant {
                format!("php{}{}", s.version.replace('.', ""), variant)
            } else {
                format!("php{}", s.version.replace('.', ""))
            };
            PhpVersionInfo {
                label,
                version,
                port: s.port,
                install_path: s.install_path.display().to_string(),
            }
        })
        .collect();
    Ok(php_versions)
}
