use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tauri::State;
use tokio::process::Command as TokioCommand;

use crate::commands::logger::logged;
use crate::state::{resolve_packages_root, AppState};

// ─── 返回类型 ────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DevToolsInfo {
    pub composer: Option<ComposerToolInfo>,
    pub nvm: Option<NvmToolInfo>,
    pub mysql: Option<MysqlToolInfo>,
}

#[derive(Serialize)]
pub struct MysqlToolInfo {
    /// 当前 user/system PATH 中匹配到的 MySQL 版本（多个匹配时取第一个）
    pub active_version: Option<String>,
    pub available: Vec<MysqlOption>,
    /// 系统 PATH (HKLM) 中含 mysqld.exe 但不属于"当前选中要设全局"的版本的目录
    /// —— 它们会屏蔽用户 PATH 的切换。前端用来展示一键修复横条。
    pub conflicts: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct MysqlOption {
    pub version: String,
    pub install_path: String,
    pub data_dir: String,
    pub bin_dir: String,
    pub port: u16,
    pub initialized: bool,
    pub root_password: String,
    /// "store" / "phpstudy" / "manual" / "system"
    pub source: String,
}

#[derive(Serialize)]
pub struct ComposerToolInfo {
    pub active_version: Option<String>,
    pub available: Vec<ComposerOption>,
}

#[derive(Serialize, Clone)]
pub struct ComposerOption {
    pub version: String,
    pub source: String,
    pub phar_path: String,
}

#[derive(Serialize)]
pub struct NvmToolInfo {
    pub nvm_version: String,
    pub nvm_source: String,
    pub nvm_home: String,
    pub current_node: Option<String>,
    pub installed_nodes: Vec<String>,
}

#[derive(Serialize)]
pub struct NodeVersionCatalog {
    /// Node.js 当前发布线（非 LTS）。
    pub current: Vec<String>,
    /// Node.js 长期支持发布线。
    pub lts: Vec<String>,
}

struct NvmCommandContext {
    home: PathBuf,
    symlink: PathBuf,
    exe: PathBuf,
}

/// nvm-windows 会修改共享的 settings、版本目录和 NVM_SYMLINK，所有操作必须串行。
static NVM_COMMAND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ─── Commands ────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_dev_tools_info(state: State<'_, AppState>) -> Result<DevToolsInfo, String> {
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);

    let services_snap = state.services.read().await.clone();

    let composer = build_composer_info(&packages_root);
    let nvm = build_nvm_info(&packages_root);
    let mysql = build_mysql_info(&packages_root, &services_snap);
    Ok(DevToolsInfo { composer, nvm, mysql })
}

#[tauri::command]
pub async fn switch_node_version(
    version: String,
    state: State<'_, AppState>,
) -> Result<NvmToolInfo, String> {
    let label = format!("切换 Node.js 到 v{}", version);
    let result = do_switch_node(&version, &state).await;
    logged(&state, "tool", label, result).await
}

async fn do_switch_node(version: &str, state: &AppState) -> Result<NvmToolInfo, String> {
    use naxone_adapters::package::tool_detect;

    let _guard = NVM_COMMAND_LOCK.lock().await;
    let version = tool_detect::normalize_node_version(version)
        .ok_or_else(|| "Node.js 版本号格式无效，请使用如 22.22.3 的完整版本号".to_string())?;
    let nvm = resolve_nvm_context()?;
    if !tool_detect::list_node_versions(&nvm.home).contains(&version) {
        return Err(format!("Node.js v{} 尚未安装", version));
    }

    run_nvm_command(&nvm, &["use", &version], Duration::from_secs(120)).await?;

    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);

    build_nvm_info(&packages_root).ok_or_else(|| "切换后读取 NVM 信息失败".into())
}

#[tauri::command]
pub async fn list_available_node_versions() -> Result<NodeVersionCatalog, String> {
    use naxone_adapters::package::tool_detect;

    let _guard = NVM_COMMAND_LOCK.lock().await;
    let nvm = resolve_nvm_context()?;
    let output =
        run_nvm_command(&nvm, &["list", "available"], Duration::from_secs(60)).await?;
    let (current, lts) = tool_detect::parse_available_node_versions(&output);
    if current.is_empty() && lts.is_empty() {
        return Err("NVM 未返回可下载的 Node.js 版本，请检查网络或 NVM 代理设置".into());
    }
    Ok(NodeVersionCatalog { current, lts })
}

#[tauri::command]
pub async fn install_node_version(
    version: String,
    activate: bool,
    state: State<'_, AppState>,
) -> Result<NvmToolInfo, String> {
    let normalized = naxone_adapters::package::tool_detect::normalize_node_version(&version)
        .unwrap_or_else(|| version.trim().to_string());
    let action = if activate {
        format!("下载并安装 Node.js v{}，安装后启用", normalized)
    } else {
        format!("下载并安装 Node.js v{}", normalized)
    };
    let result = do_install_node(&version, activate, &state).await;
    logged(&state, "tool", action, result).await
}

async fn do_install_node(
    version: &str,
    activate: bool,
    state: &AppState,
) -> Result<NvmToolInfo, String> {
    use naxone_adapters::package::tool_detect;

    let _guard = NVM_COMMAND_LOCK.lock().await;
    let version = tool_detect::normalize_node_version(version)
        .ok_or_else(|| "Node.js 版本号格式无效，请使用如 22.22.3 的完整版本号".to_string())?;
    let nvm = resolve_nvm_context()?;
    let already_installed = tool_detect::list_node_versions(&nvm.home).contains(&version);
    if !already_installed {
        run_nvm_command(
            &nvm,
            &["install", &version],
            Duration::from_secs(15 * 60),
        )
        .await?;
    }
    if activate {
        run_nvm_command(&nvm, &["use", &version], Duration::from_secs(120)).await?;
    }

    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    build_nvm_info(&packages_root).ok_or_else(|| "安装后读取 NVM 信息失败".into())
}

#[tauri::command]
pub async fn uninstall_node_version(
    version: String,
    state: State<'_, AppState>,
) -> Result<NvmToolInfo, String> {
    let normalized = naxone_adapters::package::tool_detect::normalize_node_version(&version)
        .unwrap_or_else(|| version.trim().to_string());
    let result = do_uninstall_node(&version, &state).await;
    logged(
        &state,
        "tool",
        format!("卸载 Node.js v{}", normalized),
        result,
    )
    .await
}

async fn do_uninstall_node(version: &str, state: &AppState) -> Result<NvmToolInfo, String> {
    use naxone_adapters::package::tool_detect;

    let _guard = NVM_COMMAND_LOCK.lock().await;
    let version = tool_detect::normalize_node_version(version)
        .ok_or_else(|| "Node.js 版本号格式无效，请使用如 22.22.3 的完整版本号".to_string())?;
    let nvm = resolve_nvm_context()?;
    if !tool_detect::list_node_versions(&nvm.home).contains(&version) {
        return Err(format!("Node.js v{} 未安装", version));
    }
    if tool_detect::get_current_node_version().as_deref() == Some(version.as_str()) {
        return Err("不能卸载当前正在使用的 Node.js，请先切换到其他版本".into());
    }

    run_nvm_command(
        &nvm,
        &["uninstall", &version],
        Duration::from_secs(120),
    )
    .await?;

    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    build_nvm_info(&packages_root).ok_or_else(|| "卸载后读取 NVM 信息失败".into())
}

fn resolve_nvm_context() -> Result<NvmCommandContext, String> {
    use naxone_adapters::package::tool_detect;

    let home = tool_detect::get_nvm_home().ok_or_else(|| "NVM_HOME 未设置".to_string())?;
    let symlink = tool_detect::get_nvm_symlink(&home)
        .ok_or_else(|| "NVM_SYMLINK 未设置，且 settings.txt 中没有 path 配置".to_string())?;
    let exe = home.join("nvm.exe");
    if !exe.is_file() {
        return Err(format!("nvm.exe 不存在: {}", exe.display()));
    }
    Ok(NvmCommandContext { home, symlink, exe })
}

async fn run_nvm_command(
    nvm: &NvmCommandContext,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = TokioCommand::new(&nvm.exe);
    command
        .args(args)
        .env("NVM_HOME", &nvm.home)
        .env("NVM_SYMLINK", &nvm.symlink)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("nvm {} 超时", args.join(" ")))?
        .map_err(|e| format!("启动 nvm {} 失败: {}", args.join(" "), e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let details = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!(
            "nvm {} 失败{}",
            args.join(" "),
            if details.is_empty() {
                String::new()
            } else {
                format!(": {}", details)
            }
        ));
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

#[tauri::command]
pub async fn set_global_composer(
    version: String,
    state: State<'_, AppState>,
) -> Result<DevToolsInfo, String> {
    let label = format!("设置全局 Composer 为 v{}", version);
    let result = do_set_global_composer(&version, &state).await;
    logged(&state, "tool", label, result).await
}

async fn do_set_global_composer(version: &str, state: &AppState) -> Result<DevToolsInfo, String> {
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let services_snap = state.services.read().await.clone();

    let info = build_composer_info(&packages_root)
        .ok_or_else(|| "未找到任何 Composer 安装".to_string())?;

    let selected = info
        .available
        .iter()
        .find(|a| a.version == version)
        .ok_or_else(|| format!("未找到 Composer v{}", version))?;

    let phar_path = PathBuf::from(&selected.phar_path);
    if !phar_path.exists() {
        return Err(format!("composer.phar 不存在: {}", selected.phar_path));
    }

    let bin_dir = naxone_adapters::platform::global_php::bin_dir();
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("创建 bin 目录失败: {}", e))?;

    // composer shim 调 PATH 里的 php，不写死任何 php 路径 —— 切换 PHP / 不用 NaxOne 全局
    // PHP（直接用系统/PHPStudy 的 php）时 composer 都能正常工作。
    let content = format!(
        "@echo off\r\nphp \"{}\" %*\r\n",
        phar_path.display()
    );
    std::fs::write(bin_dir.join("composer.bat"), content.as_bytes())
        .map_err(|e| format!("写 composer shim 失败: {}", e))?;

    let _ = naxone_adapters::platform::global_php::ensure_path_in_user_env();

    let composer = build_composer_info(&packages_root);
    let nvm = build_nvm_info(&packages_root);
    let mysql = build_mysql_info(&packages_root, &services_snap);
    Ok(DevToolsInfo { composer, nvm, mysql })
}

// ─── 构建逻辑 ────────────────────────────────────────────────────────────

fn build_composer_info(packages_root: &Path) -> Option<ComposerToolInfo> {
    use naxone_adapters::package::tool_detect;

    let mut available: Vec<ComposerOption> = Vec::new();

    if let Some(sys) = tool_detect::detect("composer", packages_root) {
        let phar_path = PathBuf::from(&sys.install_path).join("composer.phar");
        if phar_path.exists() {
            available.push(ComposerOption {
                version: sys.version,
                source: "system".into(),
                phar_path: phar_path.display().to_string(),
            });
        }
    }

    let tools_dir = packages_root.join("tools");
    if let Ok(entries) = std::fs::read_dir(&tools_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(ver) = name.strip_prefix("composer-") {
                let phar = entry.path().join("composer.phar");
                if phar.exists() {
                    available.push(ComposerOption {
                        version: ver.to_string(),
                        source: "store".into(),
                        phar_path: phar.display().to_string(),
                    });
                }
            }
        }
    }

    if available.is_empty() {
        return None;
    }

    let active_version = detect_active_composer(&available);
    Some(ComposerToolInfo { active_version, available })
}

/// 读当前 composer 全局 repo.packagist 配置。空串表示官方源（未配置或 default）。
/// 直接读 `%APPDATA%/Composer/config.json`，避免 `composer config` 命令对数组形式
/// repositories 字段处理不一致的问题。
#[tauri::command]
pub async fn get_composer_repo(_state: State<'_, AppState>) -> Result<String, String> {
    let Some(path) = composer_config_path() else {
        return Ok(String::new());
    };
    if !path.is_file() {
        return Ok(String::new());
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(String::new()),
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(String::new()),
    };
    Ok(extract_packagist_url(&json).unwrap_or_default())
}

/// 设置 composer 镜像源。url 为空 → 清掉自定义 repo 回到 Packagist 官方源。
#[tauri::command]
pub async fn set_composer_repo(
    url: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let label = if url.is_empty() {
        "重置 Composer 镜像源为官方".to_string()
    } else {
        format!("设置 Composer 镜像源 → {}", url)
    };
    let result = do_set_composer_repo(&url, &state).await;
    logged(&state, "tool", label, result).await
}

async fn do_set_composer_repo(url: &str, _state: &AppState) -> Result<String, String> {
    let path = composer_config_path()
        .ok_or_else(|| "无法定位 %APPDATA%/Composer 目录".to_string())?;

    // 读现有 config.json；不存在或解析失败 → 视为空配置
    let mut json: serde_json::Value = if path.is_file() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("读 config.json 失败: {}", e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // 清掉两种可能的 packagist 配置：repo.packagist (config 节) + repositories.packagist (顶层)
    if let Some(config) = json.get_mut("config") {
        if let Some(obj) = config.as_object_mut() {
            obj.remove("repo");
        }
    }
    if let Some(repos) = json.get_mut("repositories") {
        // 数组形式：移除 name=packagist 的项
        if let Some(arr) = repos.as_array_mut() {
            arr.retain(|r| r.get("name").and_then(|n| n.as_str()) != Some("packagist"));
        }
        // 对象形式：移除 packagist key
        if let Some(obj) = repos.as_object_mut() {
            obj.remove("packagist");
        }
    }

    if !url.is_empty() {
        // 用对象形式写入（composer 官方推荐写法）
        let entry = serde_json::json!({ "type": "composer", "url": url });
        // 复用现有 repositories 字段类型；不存在则用 object
        match json.get_mut("repositories") {
            Some(serde_json::Value::Array(arr)) => {
                arr.push(serde_json::json!({
                    "name": "packagist",
                    "type": "composer",
                    "url": url
                }));
            }
            Some(serde_json::Value::Object(obj)) => {
                obj.insert("packagist".into(), entry);
            }
            _ => {
                let mut obj = serde_json::Map::new();
                obj.insert("packagist".into(), entry);
                json["repositories"] = serde_json::Value::Object(obj);
            }
        }
    } else {
        // 清空后若 repositories 变空，删掉整个字段避免遗留空数组/对象
        let should_remove = match json.get("repositories") {
            Some(serde_json::Value::Array(a)) => a.is_empty(),
            Some(serde_json::Value::Object(o)) => o.is_empty(),
            _ => false,
        };
        if should_remove {
            if let Some(obj) = json.as_object_mut() {
                obj.remove("repositories");
            }
        }
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let serialized = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("序列化 config.json 失败: {}", e))?;
    std::fs::write(&path, serialized.as_bytes())
        .map_err(|e| format!("写 config.json 失败: {}", e))?;
    Ok(url.to_string())
}

fn composer_config_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(appdata).join("Composer").join("config.json"))
}

/// 从 config.json 提取 packagist 镜像的 URL（数组或对象形式都支持）。
fn extract_packagist_url(json: &serde_json::Value) -> Option<String> {
    let repos = json.get("repositories")?;
    if let Some(arr) = repos.as_array() {
        for r in arr {
            if r.get("name").and_then(|n| n.as_str()) == Some("packagist") {
                if let Some(url) = r.get("url").and_then(|u| u.as_str()) {
                    return Some(url.to_string());
                }
            }
        }
    }
    if let Some(obj) = repos.as_object() {
        if let Some(p) = obj.get("packagist") {
            if let Some(url) = p.get("url").and_then(|u| u.as_str()) {
                return Some(url.to_string());
            }
        }
    }
    None
}

fn detect_active_composer(available: &[ComposerOption]) -> Option<String> {
    let shim = naxone_adapters::platform::global_php::bin_dir().join("composer.bat");
    if let Ok(content) = std::fs::read_to_string(&shim) {
        let content_norm = content.replace('/', "\\").to_lowercase();
        for opt in available {
            let phar_norm = opt.phar_path.replace('/', "\\").to_lowercase();
            if content_norm.contains(&phar_norm) {
                return Some(opt.version.clone());
            }
        }
    }
    // shim 不存在或不匹配 → 返回第一个可用版本
    available.first().map(|a| a.version.clone())
}

// ─── MySQL Commands ──────────────────────────────────────────────────────

/// 复用 PHP 的 UAC 一键清理逻辑：从 HKLM PATH 删除指定目录。
#[tauri::command]
pub async fn fix_mysql_path_conflicts(
    paths: Vec<String>,
    state: State<'_, AppState>,
) -> Result<DevToolsInfo, String> {
    let label = format!("一键清理系统 PATH 中的 MySQL 冲突 ({} 条)", paths.len());
    let result = do_fix_mysql_path_conflicts(&paths, &state).await;
    logged(&state, "tool", label, result).await
}

async fn do_fix_mysql_path_conflicts(
    paths: &[String],
    state: &AppState,
) -> Result<DevToolsInfo, String> {
    if paths.is_empty() {
        return Err("没有需要清理的路径".into());
    }
    let buf: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    naxone_adapters::platform::global_php::fix_masking_paths(&buf)?;

    // 清理后重读返回最新状态
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let services_snap = state.services.read().await.clone();
    Ok(DevToolsInfo {
        composer: build_composer_info(&packages_root),
        nvm: build_nvm_info(&packages_root),
        mysql: build_mysql_info(&packages_root, &services_snap),
    })
}

#[tauri::command]
pub async fn set_global_mysql(
    version: String,
    state: State<'_, AppState>,
) -> Result<DevToolsInfo, String> {
    let label = format!("设置全局 MySQL 为 v{}", version);
    let result = do_set_global_mysql(&version, &state).await;
    logged(&state, "tool", label, result).await
}

async fn do_set_global_mysql(version: &str, state: &AppState) -> Result<DevToolsInfo, String> {
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let services_snap = state.services.read().await.clone();

    let info = build_mysql_info(&packages_root, &services_snap)
        .ok_or_else(|| "未找到任何 MySQL 安装".to_string())?;
    let selected = info
        .available
        .iter()
        .find(|a| a.version == version)
        .ok_or_else(|| format!("未找到 MySQL v{}", version))?
        .clone();

    // 1) 把所有其他 MySQL bin 从用户 PATH 移除（保证只有一个全局）
    for opt in &info.available {
        if opt.version == version { continue; }
        let _ = naxone_adapters::platform::user_env::remove_from_user_path(&opt.bin_dir);
    }
    // 2) 追加目标 bin
    naxone_adapters::platform::user_env::append_to_user_path(&selected.bin_dir)
        .map_err(|e| format!("写用户 PATH 失败: {}", e))?;

    Ok(DevToolsInfo {
        composer: build_composer_info(&packages_root),
        nvm: build_nvm_info(&packages_root),
        mysql: build_mysql_info(&packages_root, &services_snap),
    })
}

/// 修改 MySQL root 密码。`current_password` 在 .naxone-root.txt 缺失
/// （如 PHPStudy MySQL）时由前端让用户输入；为 None 时回退读 .naxone-root.txt。
#[tauri::command]
pub async fn set_mysql_password(
    version: String,
    new_password: String,
    current_password: Option<String>,
    state: State<'_, AppState>,
) -> Result<DevToolsInfo, String> {
    let label = format!("修改 MySQL v{} root 密码", version);
    let result = do_set_mysql_password(&version, &new_password, current_password.as_deref(), &state).await;
    logged(&state, "tool", label, result).await
}

async fn do_set_mysql_password(
    version: &str,
    new_password: &str,
    current_password: Option<&str>,
    state: &AppState,
) -> Result<DevToolsInfo, String> {
    use naxone_adapters::package::tool_detect;

    if new_password.is_empty() {
        return Err("新密码不能为空".into());
    }

    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let services_snap = state.services.read().await.clone();

    let info = build_mysql_info(&packages_root, &services_snap)
        .ok_or_else(|| "未找到任何 MySQL 安装".to_string())?;
    let selected = info
        .available
        .iter()
        .find(|a| a.version == version)
        .ok_or_else(|| format!("未找到 MySQL v{}", version))?
        .clone();

    let install = PathBuf::from(&selected.install_path);
    // 优先用前端传的当前密码；缺省时读 .naxone-root.txt
    let current = current_password
        .map(|s| s.to_string())
        .unwrap_or_else(|| tool_detect::read_mysql_root_password(&install));

    // 必须 mysqld 在跑（端口可连）才能改
    if !port_is_open("127.0.0.1", selected.port) {
        return Err(format!(
            "MySQL 服务未运行（127.0.0.1:{} 未监听），请先在仪表盘启动它再修改密码",
            selected.port
        ));
    }

    tool_detect::change_mysql_root_password(&install, selected.port, &current, new_password)
        .map_err(|e| format!("ALTER USER 失败: {}", e))?;
    tool_detect::write_mysql_root_password(&install, new_password)
        .map_err(|e| format!("写 .naxone-root.txt 失败: {}", e))?;

    Ok(DevToolsInfo {
        composer: build_composer_info(&packages_root),
        nvm: build_nvm_info(&packages_root),
        mysql: build_mysql_info(&packages_root, &services_snap),
    })
}

fn port_is_open(host: &str, port: u16) -> bool {
    use std::net::TcpStream;
    TcpStream::connect_timeout(
        &format!("{}:{}", host, port).parse().unwrap(),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}

fn build_mysql_info(
    packages_root: &Path,
    services: &[naxone_core::domain::service::ServiceInstance],
) -> Option<MysqlToolInfo> {
    use naxone_adapters::package::tool_detect;
    use naxone_core::domain::service::{ServiceKind, ServiceOrigin};
    use std::collections::HashSet;

    let mut available: Vec<MysqlOption> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let combined_path = combined_paths();

    // 1) 商店扫描（store 目录下的 MySQL*）
    for m in tool_detect::list_installed_mysql(packages_root) {
        let install = PathBuf::from(&m.install_path);
        let key = normalize_install(&install);
        if !seen.insert(key) { continue; }
        let bin_dir = install.join("bin").display().to_string();
        let root_password = tool_detect::read_mysql_root_password(&install);
        available.push(MysqlOption {
            version: m.version,
            install_path: m.install_path,
            data_dir: m.data_dir,
            bin_dir,
            port: m.port,
            initialized: m.initialized,
            root_password,
            source: "store".into(),
        });
    }

    // 2) services 里所有 MySQL（PHPStudy / manual / system）合并进来
    for svc in services.iter().filter(|s| s.kind == ServiceKind::Mysql) {
        let key = normalize_install(&svc.install_path);
        if !seen.insert(key) { continue; }
        let install = svc.install_path.clone();
        let bin_dir = install.join("bin").display().to_string();
        let data_dir = install.join("data");
        let initialized = data_dir.join("mysql").exists();
        let root_password = tool_detect::read_mysql_root_password(&install);
        let source = match svc.origin {
            ServiceOrigin::Store => "store",
            ServiceOrigin::PhpStudy => "phpstudy",
            ServiceOrigin::Manual => "manual",
            ServiceOrigin::System => "system",
        };
        available.push(MysqlOption {
            version: svc.version.clone(),
            install_path: install.display().to_string(),
            data_dir: data_dir.display().to_string(),
            bin_dir,
            port: svc.port,
            initialized,
            root_password,
            source: source.into(),
        });
    }

    if available.is_empty() {
        return None;
    }

    // 活跃判定：用户 + 系统 PATH 合一
    let mut active_version: Option<String> = None;
    for opt in &available {
        if naxone_adapters::platform::user_env::path_contains(&combined_path, &opt.bin_dir) {
            active_version = Some(opt.version.clone());
            break;
        }
    }

    // 系统 PATH 冲突：HKLM 里所有含 mysqld.exe 的目录（除当前活跃版本本身）
    let conflicts = detect_mysql_system_conflicts(active_version.as_deref().and_then(|v| {
        available.iter().find(|o| o.version == *v).map(|o| o.bin_dir.as_str())
    }));

    Some(MysqlToolInfo { active_version, available, conflicts })
}

/// 规整 install_path：小写 + 反斜杠 + 去尾分隔符，作为 dedupe key
fn normalize_install(p: &Path) -> String {
    p.display()
        .to_string()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// 扫 HKLM PATH 里所有"含 mysqld.exe 的目录"，排除等于 keep_bin 的那个。
/// 返回的目录将屏蔽用户 PATH 的 MySQL（系统 PATH 优先于用户 PATH）。
fn detect_mysql_system_conflicts(keep_bin: Option<&str>) -> Vec<String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = keep_bin;
        return Vec::new();
    }
    #[cfg(target_os = "windows")]
    {
        let system_path = naxone_adapters::platform::user_env::read_system_path();
        let keep_norm = keep_bin.map(|s| {
            s.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
        });
        let mut out = Vec::new();
        for entry in system_path.split(';') {
            let trimmed = entry.trim();
            if trimmed.is_empty() { continue; }
            let p = PathBuf::from(trimmed);
            if !p.join("mysqld.exe").is_file() { continue; }
            let norm = trimmed.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
            if keep_norm.as_deref() == Some(norm.as_str()) { continue; }
            out.push(trimmed.to_string());
        }
        out
    }
}

/// 读用户 PATH + 系统 PATH 拼成一个 ;-分隔的串供 path_contains 查询。
fn combined_paths() -> String {
    #[cfg(target_os = "windows")]
    {
        let user = naxone_adapters::platform::user_env::read_user_path();
        let system = naxone_adapters::platform::user_env::read_system_path();
        return format!("{};{}", user, system);
    }
    #[cfg(not(target_os = "windows"))]
    String::new()
}

// ─── NVM ────────────────────────────────────────────────────────────────

fn build_nvm_info(packages_root: &Path) -> Option<NvmToolInfo> {
    use naxone_adapters::package::tool_detect;

    let nvm_home = tool_detect::get_nvm_home()?;
    let nvm_exe = nvm_home.join("nvm.exe");
    if !nvm_exe.exists() {
        return None;
    }

    let nvm_version = tool_detect::detect("nvm", packages_root)
        .map(|d| d.version)
        .unwrap_or_else(|| "?".into());

    let naxone_tools = packages_root.join("tools");
    let source = if nvm_home.starts_with(&naxone_tools) { "store" } else { "system" };

    let installed_nodes = tool_detect::list_node_versions(&nvm_home);
    let current_node = tool_detect::get_current_node_version();

    Some(NvmToolInfo {
        nvm_version,
        nvm_source: source.into(),
        nvm_home: nvm_home.display().to_string(),
        current_node,
        installed_nodes,
    })
}
