//! Tauri commands powering the in-app software store.
//!
//! The list and installed endpoints are synchronous-cheap. `install_package`
//! spawns a background task and streams progress to the frontend via the
//! `install-progress` event name.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::commands::logger::push_log;
use crate::state::{resolve_packages_root, AppState};
use naxone_adapters::package::cache::DiskCache;
use naxone_adapters::package::installer::{phpstudy_style_dir_name, InstallEvent, Installer};
use naxone_adapters::package::manifest::{load_manifest, PackageEntry, PackageVersion};
use naxone_adapters::package::sources::{github_mirror, php_official};

/// How long upstream version indices stay fresh on disk.
const PACKAGE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Cached PHP releases file (relative to the cache dir).
const PHP_CACHE_FILE: &str = "php-releases.json";
/// Cached full mirror manifest (all software, all versions).
const MIRROR_CACHE_FILE: &str = "mirror-manifest.json";
use naxone_core::domain::log::LogLevel;

/// Returned by `get_installed_packages`. Mirrors what `StandaloneScanner` found
/// under `%APPDATA%/NaxOne/Packages/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub install_path: String,
    /// "store" = NaxOne 自己装的；"system" = 系统/用户已有的（仅 composer/nvm）
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "store".to_string()
}

/// Returns the package catalog: embedded manifest, enriched with live data
/// from upstream sources where available (currently: PHP via windows.php.net).
///
/// Lookup order for each dynamic source:
///   1. Fresh disk cache (< 6h old) — instant
///   2. Live fetch from upstream — store in cache on success
///   3. Stale disk cache — fallback when upstream is unreachable
///   4. Embedded-only — ultimate fallback
#[tauri::command]
pub async fn list_packages() -> Result<Vec<PackageEntry>, String> {
    list_packages_impl(false).await
}

/// Forces a refresh of dynamic sources, bypassing the cache. The frontend's
/// "🔄 刷新" button calls this to pull the very latest PHP list on demand.
#[tauri::command]
pub async fn refresh_package_index() -> Result<Vec<PackageEntry>, String> {
    list_packages_impl(true).await
}

async fn list_packages_impl(force: bool) -> Result<Vec<PackageEntry>, String> {
    let mut manifest = load_merged_manifest(force).await;
    manifest.packages.retain(|p| !p.versions.is_empty());
    Ok(manifest.packages)
}

/// 加载内嵌清单 + 合并**镜像源**（GitHub 镜像仓库）最新版本。
/// 策略：
///   1. 先拉 github_mirror（全套软件 + 多 URL 下载链路）
///   2. 镜像失败 → 退化到 php_official 直连 php.net（只有 PHP）
///   3. 两者都失败 → 只用内嵌 packages.json
/// 镜像版本会**覆盖**同 version 的内嵌版本（因为镜像带 download_urls 多路加速）。
async fn load_merged_manifest(force: bool) -> crate::commands::package::manifest_types::Manifest {
    let mut manifest = load_manifest();

    // 1) 尝试镜像
    if let Some(mirror) = load_mirror_manifest(force).await {
        for (software, versions) in mirror {
            let incoming: Vec<PackageVersion> = versions
                .into_iter()
                .filter(|v| {
                    if software != "apache" {
                        return true;
                    }
                    !matches!(v.version.as_str(), "2.4.63" | "2.4.62" | "2.4.58")
                })
                .collect();
            if incoming.is_empty() {
                continue;
            }
            if let Some(pkg) = manifest.packages.iter_mut().find(|p| p.name == software) {
                merge_versions(&mut pkg.versions, incoming);
            }
        }
        return manifest;
    }

    // 2) 镜像不可用 → 退到 php_official（仅 PHP）
    if let Some(live_versions) = load_php_versions(force).await {
        if let Some(php) = manifest.packages.iter_mut().find(|p| p.name == "php") {
            merge_versions(&mut php.versions, live_versions);
        }
    }

    manifest
}

/// 拉镜像 manifest，带 6h 本地缓存兜底。
async fn load_mirror_manifest(
    force: bool,
) -> Option<std::collections::HashMap<String, Vec<PackageVersion>>> {
    let cache = DiskCache::new(cache_dir().join(MIRROR_CACHE_FILE), PACKAGE_CACHE_TTL);
    if !force {
        if let Some(cached) = cache.read_fresh::<std::collections::HashMap<String, Vec<PackageVersion>>>() {
            tracing::debug!("Mirror manifest served from fresh cache");
            return Some(cached);
        }
    }
    match github_mirror::fetch().await {
        Ok(fresh) => {
            if let Err(e) = cache.write(&fresh) {
                tracing::warn!("Failed to persist mirror cache: {}", e);
            }
            Some(fresh)
        }
        Err(e) => {
            tracing::warn!("镜像 manifest 获取失败: {}。尝试 stale 缓存。", e);
            cache.read_stale::<std::collections::HashMap<String, Vec<PackageVersion>>>()
        }
    }
}

/// Re-export the Manifest type path so `load_merged_manifest` has a visible
/// return type without cluttering the public API. `load_manifest()` lives in
/// the adapters crate — this is just a thin alias to keep the signature stable.
mod manifest_types {
    pub type Manifest = naxone_adapters::package::manifest::Manifest;
}

/// Resolve PHP versions with cache-first strategy.
async fn load_php_versions(force: bool) -> Option<Vec<PackageVersion>> {
    let cache = php_cache();

    if !force {
        if let Some(cached) = cache.read_fresh::<Vec<PackageVersion>>() {
            tracing::debug!("PHP versions served from fresh cache");
            return Some(cached);
        }
    }

    match php_official::fetch().await {
        Ok(fresh) => {
            if let Err(e) = cache.write(&fresh) {
                tracing::warn!("Failed to persist PHP cache: {}", e);
            }
            Some(fresh)
        }
        Err(e) => {
            tracing::warn!("PHP upstream failed: {}. Trying stale cache.", e);
            cache.read_stale::<Vec<PackageVersion>>()
        }
    }
}

fn php_cache() -> DiskCache {
    DiskCache::new(cache_dir().join(PHP_CACHE_FILE), PACKAGE_CACHE_TTL)
}

fn cache_dir() -> std::path::PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".into());
    std::path::PathBuf::from(home).join(crate::state::naxone_home_dirname()).join("cache")
}

/// Merge 镜像版本到现有列表。策略：
/// - 镜像的版本**覆盖**内嵌同 version（镜像带 `download_urls`，是带加速链路的版本）
/// - 镜像不包含的内嵌版本保留（EOL 版本等）
/// - 合并后**按版本号降序整体排序**，避免镜像段和内嵌段视觉上出现 7.4 → 8.4 跳跃
fn merge_versions(existing: &mut Vec<PackageVersion>, incoming: Vec<PackageVersion>) {
    let mut existing_by_ver: std::collections::HashMap<String, PackageVersion> =
        existing.drain(..).map(|v| (v.version.clone(), v)).collect();

    let mut result: Vec<PackageVersion> = Vec::with_capacity(existing_by_ver.len() + incoming.len());

    for mut inc in incoming {
        if let Some(base) = existing_by_ver.remove(&inc.version) {
            // 镜像版本优先，但保留内嵌 URL 作为最终兜底。
            // 这样当 download_urls 全部失败时，仍可退到内嵌官方 URL。
            let mut merged_urls: Vec<String> = Vec::new();

            for u in inc.candidate_urls() {
                if !merged_urls.contains(&u) {
                    merged_urls.push(u);
                }
            }
            for u in base.candidate_urls() {
                if !merged_urls.contains(&u) {
                    merged_urls.push(u);
                }
            }

            if !merged_urls.is_empty() {
                inc.url = merged_urls[0].clone();
                inc.download_urls = merged_urls;
            }

            if inc.sha256.is_none() {
                inc.sha256 = base.sha256;
            }
            if inc.size_mb.is_none() {
                inc.size_mb = base.size_mb;
            }
            if inc.exe_rel.is_empty() {
                inc.exe_rel = base.exe_rel;
            }
            if inc.variant.is_none() {
                inc.variant = base.variant;
            }
        }
        result.push(inc);
    }

    // 保留镜像未覆盖到的内嵌版本（EOL 等）
    result.extend(existing_by_ver.into_values());

    // 统一按语义版本降序排列（8.5.5 > 8.4.20 > 8.4.2 > 8.1.27 > 7.4.33）
    result.sort_by(|a, b| semver_cmp_desc(&a.version, &b.version));
    *existing = result;
}


/// 两段版本号按数字段降序比较。"8.4.20" > "8.4.2" > "8.3.30"。
/// 非数字段落到字符串对比。
fn semver_cmp_desc(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (Some(_), None) => return std::cmp::Ordering::Less, // 长版本 > 短版本 → 降序先来
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u32>(), y.parse::<u32>()) {
                (Ok(xn), Ok(yn)) if xn != yn => return yn.cmp(&xn), // 降序
                (Ok(xn), Ok(yn)) if xn == yn => continue,
                _ => {
                    // 一段含非数字 → 字符串对比（降序）
                    let ord = y.cmp(x);
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
            },
        }
    }
}

/// Report which packages are installed via the store. Derived from the
/// already-scanned service list — keeps a single source of truth and handles
/// both the new PHPStudy-style layout and legacy `%APPDATA%/Packages/` dirs
/// for free.
///
/// The returned `name` uses the manifest key convention (lowercase:
/// "nginx"/"mysql"/"php"/...), and `version` is the parsed semver string.
#[tauri::command]
pub async fn get_installed_packages(
    state: State<'_, AppState>,
) -> Result<Vec<InstalledPackage>, String> {
    use naxone_core::domain::service::{ServiceKind, ServiceOrigin};

    let services = state.services.read().await;
    let mut out: Vec<InstalledPackage> = Vec::new();

    for svc in services.iter() {
        // Only count services sourced from the store.
        // Exclude PhpStudy/manual/system installs: they are not store-managed.
        match svc.origin {
            ServiceOrigin::Store => {}
            ServiceOrigin::PhpStudy | ServiceOrigin::Manual | ServiceOrigin::System => continue,
        }

        let name = match svc.kind {
            ServiceKind::Nginx => "nginx",
            ServiceKind::Apache => "apache",
            ServiceKind::Mysql => "mysql",
            ServiceKind::Redis => "redis",
            ServiceKind::Php => "php",
        };

        out.push(InstalledPackage {
            name: name.to_string(),
            version: svc.version.clone(),
            install_path: svc.install_path.display().to_string(),
            source: "store".into(),
        });
    }
    drop(services);

    // 工具类（composer / nvm）不是 service，扫 packages_root/tools 下的目录推导。
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    out.extend(scan_tools(&packages_root));

    // 系统/用户已装的工具（仅 composer / nvm）。NaxOne 自己装的会被 detect 模块排除。
    // 用户「解除关联」过的工具记在 config.ignored_system_tools，这里跳过 —— UI 上不再显示。
    use naxone_adapters::package::tool_detect;
    let ignored = state.config.read().await.general.ignored_system_tools.clone();
    for detected in tool_detect::detect_all(&packages_root) {
        if ignored.iter().any(|n| n.eq_ignore_ascii_case(&detected.name)) {
            continue;
        }
        out.push(InstalledPackage {
            name: detected.name,
            version: detected.version,
            install_path: detected.install_path,
            source: "system".into(),
        });
    }

    Ok(out)
}

/// 扫 `<packages_root>/tools/` 下形如 `composer-X.Y.Z` / `nvm-X.Y.Z` 的目录，
/// 返回对应的 InstalledPackage 列表。目录里没有可执行 stub 也算装上 —— 一旦目录在，
/// 商店就显示"已安装"，避免 zip 解压成功但 post-install 局部失败时前端误判未装。
fn scan_tools(packages_root: &std::path::Path) -> Vec<InstalledPackage> {
    let tools_dir = packages_root.join("tools");
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&tools_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name_os = entry.file_name();
        let dir_name = name_os.to_string_lossy();
        if let Some((tool, version)) = parse_tool_dir(&dir_name) {
            out.push(InstalledPackage {
                name: tool.to_string(),
                version: version.to_string(),
                install_path: path.display().to_string(),
                source: "store".into(),
            });
        }
    }
    out
}

/// "composer-2.7.7" → Some(("composer", "2.7.7"))
fn parse_tool_dir(dir: &str) -> Option<(&'static str, String)> {
    for prefix in ["composer-", "nvm-"] {
        if let Some(rest) = dir.strip_prefix(prefix) {
            let tool = match prefix {
                "composer-" => "composer",
                "nvm-" => "nvm",
                _ => unreachable!(),
            };
            return Some((tool, rest.to_string()));
        }
    }
    None
}

/// Kick off an install in the background. Returns immediately after validation;
/// progress and completion come via the `install-progress` Tauri event.
#[tauri::command]
pub async fn install_package(
    name: String,
    version: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use naxone_core::domain::service::ServiceKind;

    let manifest = load_merged_manifest(false).await;
    let pkg = manifest
        .packages
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("未知软件: {}", name))?
        .clone();
    let ver = pkg
        .versions
        .iter()
        .find(|v| v.version == version)
        .ok_or_else(|| format!("未知版本: {} v{}", name, version))?
        .clone();

    // 工具类（composer / nvm）：系统已经装了就拒绝重装，避免 NaxOne 覆盖用户原有环境
    // （nvm 装第二份会改 NVM_HOME，原 nvm 的 node 版本"消失"）
    if matches!(name.as_str(), "composer" | "nvm") {
        let config = state.config.read().await;
        let packages_root = resolve_packages_root(&config);
        drop(config);
        if let Some(detected) =
            naxone_adapters::package::tool_detect::detect(&name, &packages_root)
        {
            let msg = format!(
                "{} 已经在系统中安装（v{}，路径 {}），无需通过 NaxOne 商店重装。",
                pkg.display_name, detected.version, detected.install_path
            );
            push_log(&state, LogLevel::Warn, "store", msg.clone(), None, None).await;
            return Err(msg);
        }
    }

    // Guard: if this exact (kind, version) service is already running, refuse
    // the install cleanly — overwriting would race with the OS file lock and
    // produce a cryptic "directory can't be deleted" error deep in the
    // installer. Catching it here lets us return a friendly message.
    let target_kind = match name.as_str() {
        "nginx" => Some(ServiceKind::Nginx),
        "apache" => Some(ServiceKind::Apache),
        "mysql" => Some(ServiceKind::Mysql),
        "redis" => Some(ServiceKind::Redis),
        "php" => Some(ServiceKind::Php),
        _ => None,
    };
    if let Some(kind) = target_kind {
        let services = state.services.read().await;
        let running_conflict = services
            .iter()
            .any(|s| s.kind == kind && s.version == version && s.status.is_running());
        drop(services);
        if running_conflict {
            let msg = format!(
                "{} v{} 正在运行，请先停止后再重新安装",
                pkg.display_name, version
            );
            push_log(&state, LogLevel::Warn, "store", msg.clone(), None, None).await;
            return Err(msg);
        }
    }

    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);

    // Make sure the root exists before spawning (early-fail in the main task so
    // errors surface to the caller immediately).
    if let Err(e) = std::fs::create_dir_all(&packages_root) {
        return Err(format!(
            "创建包安装根目录失败 {}: {}",
            packages_root.display(),
            e
        ));
    }

    push_log(
        &state,
        LogLevel::Info,
        "store",
        format!("开始安装 {} v{}", pkg.display_name, version),
        None,
        None,
    )
    .await;

    // Spawn the install in the background. The Installer emits InstallEvents via
    // an mpsc channel; a forwarder task pumps each event onto the Tauri bus.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InstallEvent>();
    let installer = Installer::new(packages_root);
    let state_snap = std::sync::Arc::new(state.inner().clone_shallow());

    tauri::async_runtime::spawn(async move {
        let _ = installer.install(&pkg, &ver, tx).await;
    });

    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            // Log terminal events
            match &event {
                InstallEvent::Done { name, version, .. } => {
                    push_log(
                        &state_snap,
                        LogLevel::Success,
                        "store",
                        format!("{} v{} 安装成功", name, version),
                        None,
                        None,
                    )
                    .await;
                    // Trigger a services rescan so the newly installed package
                    // shows up in the Dashboard.
                    let _ = rescan_services_inner(&state_snap).await;
                    // 主动通知前端刷新，不用等 5s 轮询
                    let _ = app_handle.emit("services-changed", ());
                    // 装的是 PHP 且 NaxOne CA 已存在 → 注入 cabundle 信任，让新 PHP curl
                    // 跟其它 PHP 一样能信任本地 dev CA。CA 不存在时函数内部静默跳过。
                    crate::commands::vhost::refresh_php_cabundle_trust(&app_handle, &state_snap).await;
                }
                InstallEvent::Failed {
                    name,
                    version,
                    reason,
                } => {
                    push_log(
                        &state_snap,
                        LogLevel::Error,
                        "store",
                        format!("{} v{} 安装失败", name, version),
                        Some(reason.clone()),
                        None,
                    )
                    .await;
                }
                _ => {}
            }

            let _ = app_handle.emit("install-progress", &event);
        }
    });

    Ok(())
}

/// Internal rescan after install — mirrors `commands::settings::rescan_services`
/// but operates on a shallow-cloned state handle (no Tauri State required).
async fn rescan_services_inner(
    state: &std::sync::Arc<AppState>,
) -> Result<(), String> {
    use naxone_adapters::config::fs_config::FsConfigIO;
    use naxone_adapters::package::composite::CompositeScanner;
    use naxone_adapters::vhost::VhostScanner;
    use naxone_core::use_cases::vhost_mgr::VhostManager;

    let config = state.config.read().await;
    let phpstudy_opt = config.general.phpstudy_path.clone();
    let extras = config.general.extra_install_paths.clone();
    let store_ext = resolve_packages_root(&config);
    let legacy_root = crate::state::legacy_packages_root();
    drop(config);

    let ext_path = phpstudy_opt.as_ref().map(|p| p.join("Extensions"));
    let new_services = CompositeScanner::scan(
        ext_path.as_deref(),
        Some(&store_ext),
        &extras,
        Some(&legacy_root),
    );

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

    {
        let mut services = state.services.write().await;
        *services = new_services;
    }
    {
        let mut vhosts = state.vhosts.write().await;
        *vhosts = merged;
    }

    // 装完 nginx/apache 后，conf/vhosts/ 是空的，从 vhosts.json 元数据自动重写所有
    // 缺失的 .conf。让用户卸载 → 重装的流程能自动恢复站点。
    regen_missing_vhost_configs(state).await;

    Ok(())
}

async fn regen_missing_vhost_configs(state: &std::sync::Arc<AppState>) {
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
    for v in &vhosts {
        let _ = state.vhost_manager.regenerate_configs(
            v,
            nginx_vhosts.as_deref(),
            apache_vhosts.as_deref(),
        );
    }
}

#[tauri::command]
pub async fn uninstall_package(
    name: String,
    version: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use naxone_core::domain::service::ServiceKind;

    let manifest = load_merged_manifest(false).await;
    let pkg = manifest
        .packages
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("未知软件: {}", name))?
        .clone();
    let _ver = pkg
        .versions
        .iter()
        .find(|v| v.version == version)
        .ok_or_else(|| format!("未知版本: {} v{}", name, version))?;

    // 服务类用 ServiceKind 检查 origin / running；工具类（composer/nvm）跳过这一段。
    let target_kind = match name.as_str() {
        "nginx" => Some(ServiceKind::Nginx),
        "apache" => Some(ServiceKind::Apache),
        "mysql" => Some(ServiceKind::Mysql),
        "redis" => Some(ServiceKind::Redis),
        "php" => Some(ServiceKind::Php),
        "composer" | "nvm" => None,
        _ => return Err(format!("未知软件类型: {}", name)),
    };

    if let Some(kind) = target_kind {
        use naxone_core::domain::service::ServiceOrigin;
        let services = state.services.read().await;
        let matches: Vec<_> = services
            .iter()
            .filter(|s| s.kind == kind && s.version == version)
            .collect();
        if !matches.is_empty() && matches.iter().all(|s| s.origin == ServiceOrigin::PhpStudy) {
            let msg = "不能卸载 PHPStudy 自带的软件".to_string();
            push_log(&state, LogLevel::Warn, "store", msg.clone(), None, None).await;
            return Err(msg);
        }
        let running = matches.iter().any(|s| s.status.is_running());
        if running {
            let msg = format!("{} v{} 正在运行，请先停止后再卸载", pkg.display_name, version);
            push_log(&state, LogLevel::Warn, "store", msg.clone(), None, None).await;
            return Err(msg);
        }
    }

    // Compute the target dir under our packages root
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let dir_name = phpstudy_style_dir_name(&name, &version);
    let target_dir = packages_root.join(&dir_name);

    // Boundary: the resolved path must still live under packages_root
    if !target_dir.starts_with(&packages_root) {
        return Err("安全检查失败：目标路径逃出安装根".into());
    }

    if !target_dir.exists() {
        let msg = format!("{} v{} 未安装（目录不存在）", pkg.display_name, version);
        push_log(&state, LogLevel::Info, "store", msg, None, None).await;
        return Ok(());
    }

    // 工具类先清用户 PATH / 环境变量（趁目录还在，可读出绝对路径作 PATH 比对值）
    if matches!(name.as_str(), "composer" | "nvm") {
        naxone_adapters::package::post_install::uninstall(&name, &target_dir);
    }

    // Actually delete
    match std::fs::remove_dir_all(&target_dir) {
        Ok(_) => {
            push_log(
                &state,
                LogLevel::Success,
                "store",
                format!("{} v{} 已卸载", pkg.display_name, version),
                Some(format!("路径: {}", target_dir.display())),
                None,
            )
            .await;
        }
        Err(e) => {
            let msg = format!(
                "卸载 {} v{} 失败: {} (可能有文件被占用)",
                pkg.display_name, version, e
            );
            push_log(&state, LogLevel::Error, "store", msg.clone(), None, None).await;
            return Err(msg);
        }
    }

    // PHP parent dir: if we just removed the last php/phpXXXnts/ subdir,
    // the shared `php/` directory is now empty. Clean it up for tidiness
    // (best-effort; ignore errors).
    if name == "php" {
        let php_parent = packages_root.join("php");
        if let Ok(mut rd) = std::fs::read_dir(&php_parent) {
            if rd.next().is_none() {
                let _ = std::fs::remove_dir(&php_parent);
            }
        }
    }

    // Refresh services list so the Dashboard drops the removed card
    // immediately. Use the shallow-cloned state pattern used elsewhere.
    let state_snap = std::sync::Arc::new(state.inner().clone_shallow());
    let _ = rescan_services_inner(&state_snap).await;
    let _ = app.emit("services-changed", ());

    Ok(())
}

/// 「解除关联」系统已装的 composer/nvm：仅清 PATH / 环境变量，不删任何文件。
/// 解除关联：让 NaxOne 不再把系统/用户级安装的工具当作"已安装"，但**不动**用户实际的
/// 系统 PATH / 文件，避免影响 IDE 等其他依赖。仅在 config.ignored_system_tools 加一条。
#[tauri::command]
pub async fn unlink_system_tool(
    name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<naxone_adapters::package::post_install::UnlinkReport, String> {
    if !matches!(name.as_str(), "composer" | "nvm") {
        return Err(format!("不支持的工具: {}", name));
    }
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let detected = naxone_adapters::package::tool_detect::detect(&name, &packages_root)
        .ok_or_else(|| format!("未检测到系统已装的 {}", name))?;

    // 写入 ignored 列表（去重 + 持久化）
    {
        let mut config = state.config.write().await;
        if !config.general.ignored_system_tools.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            config.general.ignored_system_tools.push(name.clone());
        }
        let cfg_path = crate::state::config_path();
        let _ = config.save(&cfg_path);
    }

    push_log(
        &state,
        LogLevel::Success,
        "store",
        format!("已解除与 {} v{} 的关联", name, detected.version),
        Some(format!(
            "原安装位置不变：{}\nNaxOne 仅在视野内屏蔽该工具，不修改系统 PATH / 文件。",
            detected.install_path
        )),
        None,
    )
    .await;

    let _ = app.emit("services-changed", ());
    Ok(naxone_adapters::package::post_install::UnlinkReport {
        cleared: vec![format!("已加入忽略列表（不再显示）")],
        errors: Vec::new(),
    })
}

/// 「彻底卸载」系统已装的 composer/nvm：删核心本体文件 + 清环境变量。
/// 保留用户数据（COMPOSER_HOME 全局包 / NVM_HOME 下的 node 版本目录）。
#[tauri::command]
pub async fn deep_uninstall_system_tool(
    name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<naxone_adapters::package::post_install::DeepUninstallReport, String> {
    if !matches!(name.as_str(), "composer" | "nvm") {
        return Err(format!("不支持的工具: {}", name));
    }
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let detected = naxone_adapters::package::tool_detect::detect(&name, &packages_root)
        .ok_or_else(|| format!("未检测到系统已装的 {}", name))?;
    let install_path = std::path::PathBuf::from(&detected.install_path);
    let report = naxone_adapters::package::post_install::deep_uninstall(&name, &install_path);

    let level = if report.errors.is_empty() {
        LogLevel::Success
    } else {
        LogLevel::Warn
    };
    push_log(
        &state,
        level,
        "store",
        format!("彻底卸载 {} v{}", name, detected.version),
        Some(format!(
            "删文件 {}，清环境 {}，保留用户数据 {} 项，错误 {} 条",
            report.deleted_files.len(),
            report.cleared_env.len(),
            report.kept_paths.len(),
            report.errors.len()
        )),
        None,
    )
    .await;

    let _ = app.emit("services-changed", ());
    Ok(report)
}

/// 「预览」系统已装工具的彻底卸载详情：返回会删/会保留的路径列表。
/// 前端用来填卸载确认对话框。**不实际操作**任何文件或环境变量。
#[tauri::command]
pub async fn preview_system_tool_uninstall(
    name: String,
    state: State<'_, AppState>,
) -> Result<UninstallPreview, String> {
    if !matches!(name.as_str(), "composer" | "nvm") {
        return Err(format!("不支持的工具: {}", name));
    }
    let config = state.config.read().await;
    let packages_root = resolve_packages_root(&config);
    drop(config);
    let detected = naxone_adapters::package::tool_detect::detect(&name, &packages_root)
        .ok_or_else(|| format!("未检测到系统已装的 {}", name))?;
    let install_path = std::path::PathBuf::from(&detected.install_path);

    let mut will_delete: Vec<String> = Vec::new();
    let mut will_keep: Vec<String> = Vec::new();

    match name.as_str() {
        "composer" => {
            // 列出会被删的 composer 文件
            if let Ok(rd) = std::fs::read_dir(&install_path) {
                for entry in rd.flatten() {
                    let fname = entry.file_name();
                    let lower = fname.to_string_lossy().to_ascii_lowercase();
                    let is_composer_file = lower == "composer"
                        || lower.starts_with("composer.")
                        || lower.starts_with("composer-");
                    if is_composer_file && entry.path().is_file() {
                        will_delete.push(entry.path().display().to_string());
                    }
                }
            }
            // COMPOSER_HOME 列为保留
            if let Ok(home) = std::env::var("USERPROFILE") {
                let composer_home = std::path::PathBuf::from(home)
                    .join("AppData")
                    .join("Roaming")
                    .join("Composer");
                if composer_home.exists() {
                    will_keep.push(composer_home.display().to_string());
                }
            }
        }
        "nvm" => {
            if let Ok(rd) = std::fs::read_dir(&install_path) {
                for entry in rd.flatten() {
                    let fname = entry.file_name();
                    let s = fname.to_string_lossy().to_string();
                    let p = entry.path();
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

                    // node 版本目录 → 保留
                    if is_dir
                        && s.starts_with('v')
                        && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
                    {
                        will_keep.push(p.display().to_string());
                        continue;
                    }
                    // current symlink + 核心本体 → 删
                    if s == "nodejs"
                        || NVM_PREVIEW_CORE_FILES
                            .iter()
                            .any(|f| f.eq_ignore_ascii_case(&s))
                    {
                        will_delete.push(p.display().to_string());
                    }
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(UninstallPreview {
        name,
        version: detected.version,
        install_path: detected.install_path,
        will_delete,
        will_keep,
    })
}

/// 与 `post_install::NVM_CORE_FILES` 同步的预览版本（提供给 IPC 前端展示用）。
const NVM_PREVIEW_CORE_FILES: &[&str] = &[
    "nvm.exe",
    "elevate.cmd",
    "elevate.vbs",
    "setuserenv.vbs",
    "unsetuserenv.vbs",
    "settings.txt",
    "LICENSE",
    "README.md",
    "nvm.ico",
    "nodejs.ico",
    "alert.ico",
    "author.ico",
    "download.ico",
    "success.ico",
    "author-nvm.exe",
    "install.cmd",
    "run.cmd",
];

#[derive(Debug, Clone, Serialize)]
pub struct UninstallPreview {
    pub name: String,
    pub version: String,
    pub install_path: String,
    pub will_delete: Vec<String>,
    pub will_keep: Vec<String>,
}
