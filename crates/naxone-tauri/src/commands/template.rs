//! 站点模板初始化：在新建 vhost 后给空目录铺起始代码。
//!
//! 流程：前端创建 vhost 成功后，若用户选了非 None 模板，调用 init_site_template。
//! 后端按模板分发，过程中通过 Tauri event "site-template-log" 流式推日志到前端。
//! 所有过程同时写入 sink，结束时连同 push_log 一起写入活动日志（关掉 modal 后仍可回查）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;

use naxone_core::domain::log::LogLevel;

use crate::commands::logger::push_log;
use crate::state::AppState;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const WP_ZIP_URL: &str = "https://cn.wordpress.org/latest-zh_CN.zip";

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum SiteTemplate {
    Blank,
    Wordpress,
    Laravel,
    Thinkphp,
    Webman,
}

impl SiteTemplate {
    fn label(&self) -> &'static str {
        match self {
            SiteTemplate::Blank => "空白",
            SiteTemplate::Wordpress => "WordPress",
            SiteTemplate::Laravel => "Laravel",
            SiteTemplate::Thinkphp => "ThinkPHP",
            SiteTemplate::Webman => "Webman",
        }
    }
}

type Sink = Arc<Mutex<Vec<String>>>;

fn emit_log(app: &AppHandle, sink: &Sink, line: String) {
    let _ = app.emit("site-template-log", line.clone());
    if let Ok(mut g) = sink.lock() {
        g.push(line);
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
    phase: &'static str,
    current: Option<u64>,
    total: Option<u64>,
    unit: Option<&'static str>,
}

fn emit_progress(
    app: &AppHandle,
    phase: &'static str,
    current: Option<u64>,
    total: Option<u64>,
    unit: Option<&'static str>,
) {
    let _ = app.emit(
        "site-template-progress",
        ProgressEvent { phase, current, total, unit },
    );
}

fn drain_sink(sink: &Sink) -> String {
    sink.lock().map(|g| g.join("\n")).unwrap_or_default()
}

#[tauri::command]
pub async fn init_site_template(
    app: AppHandle,
    target_dir: String,
    template: SiteTemplate,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let dir = PathBuf::from(&target_dir);
    let label = template.label();

    push_log(
        &state,
        LogLevel::Info,
        "site-template",
        format!("开始初始化 {} 模板 → {}", label, target_dir),
        None,
        None,
    )
    .await;

    if !dir.exists() {
        let msg = format!("目录不存在: {}", target_dir);
        push_log(
            &state,
            LogLevel::Error,
            "site-template",
            format!("{} 模板初始化失败", label),
            Some(msg.clone()),
            None,
        )
        .await;
        return Err(msg);
    }
    // 校验目录为空 —— 避免覆盖用户已有项目。
    // 但 NaxOne 在 create_vhost 时会先写 nginx.htaccess / .htaccess（伪静态规则）到 document_root，
    // 这俩是自家文件，不算"用户已有内容"，必须排除，否则刚 create_vhost 完模板就装不进去。
    let real_files: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("读取目录失败: {}", e))?
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            s != "nginx.htaccess" && s != ".htaccess"
        })
        .collect();
    if !real_files.is_empty() {
        let msg = "目录非空，请先清空再选择模板".to_string();
        push_log(
            &state,
            LogLevel::Error,
            "site-template",
            format!("{} 模板初始化失败", label),
            Some(format!("{}: {}", msg, dir.display())),
            None,
        )
        .await;
        return Err(msg);
    }

    let sink: Sink = Arc::new(Mutex::new(Vec::new()));

    emit_log(&app, &sink, format!("▶ 开始初始化模板: {}", label));
    emit_log(&app, &sink, format!("目标目录: {}", dir.display()));

    let result = match template {
        SiteTemplate::Blank => {
            emit_progress(&app, "running", None, None, None);
            init_blank(&app, &sink, &dir).await
        }
        SiteTemplate::Wordpress => init_wordpress(&app, &sink, &dir).await,
        SiteTemplate::Laravel => {
            emit_progress(&app, "running", None, None, None);
            init_composer(&app, &sink, &dir, "laravel/laravel", &state).await
        }
        SiteTemplate::Thinkphp => {
            emit_progress(&app, "running", None, None, None);
            init_composer(&app, &sink, &dir, "topthink/think", &state).await
        }
        SiteTemplate::Webman => {
            emit_progress(&app, "running", None, None, None);
            init_composer(&app, &sink, &dir, "workerman/webman", &state).await
        }
    };

    match &result {
        Ok(_) => emit_log(&app, &sink, "✔ 初始化完成".to_string()),
        Err(e) => emit_log(&app, &sink, format!("✗ 初始化失败: {}", e)),
    }

    let full_log = drain_sink(&sink);
    match &result {
        Ok(_) => {
            push_log(
                &state,
                LogLevel::Success,
                "site-template",
                format!("{} 模板初始化完成 → {}", label, dir.display()),
                Some(full_log),
                None,
            )
            .await
        }
        Err(e) => {
            push_log(
                &state,
                LogLevel::Error,
                "site-template",
                format!("{} 模板初始化失败", label),
                Some(format!("错误: {}\n\n──── 完整日志 ────\n{}", e, full_log)),
                None,
            )
            .await
        }
    }

    result
}

async fn init_blank(app: &AppHandle, sink: &Sink, dir: &Path) -> Result<(), String> {
    let path = dir.join("index.php");
    let body = "<?php\nphpinfo();\n";
    std::fs::write(&path, body).map_err(|e| format!("写入 index.php 失败: {}", e))?;
    emit_log(app, sink, format!("已创建 {}", path.display()));
    Ok(())
}

async fn init_wordpress(app: &AppHandle, sink: &Sink, dir: &Path) -> Result<(), String> {
    emit_log(app, sink, format!("下载 WordPress: {}", WP_ZIP_URL));
    emit_progress(app, "downloading", Some(0), None, Some("MB"));

    let resp = reqwest::get(WP_ZIP_URL)
        .await
        .map_err(|e| format!("下载失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status()));
    }
    let total = resp.content_length();
    emit_progress(app, "downloading", Some(0), total, Some("MB"));

    let mut bytes: Vec<u8> = Vec::with_capacity(total.unwrap_or(25 * 1024 * 1024) as usize);
    let mut stream = resp.bytes_stream();
    let mut last_emit: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("下载中断: {}", e))?;
        bytes.extend_from_slice(&chunk);
        let cur = bytes.len() as u64;
        if cur - last_emit >= 1024 * 1024 {
            emit_progress(app, "downloading", Some(cur), total, Some("MB"));
            last_emit = cur;
        }
    }
    let downloaded = bytes.len() as u64;
    emit_progress(app, "downloading", Some(downloaded), total.or(Some(downloaded)), Some("MB"));
    emit_log(
        app,
        sink,
        format!("已下载 {:.1} MB，开始解压…", downloaded as f64 / 1024.0 / 1024.0),
    );

    let cursor = std::io::Cursor::new(bytes.as_slice());
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("zip 解析失败: {}", e))?;
    let total_entries = archive.len() as u64;
    emit_progress(app, "extracting", Some(0), Some(total_entries), Some("文件"));

    let mut wrote = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取 zip 条目 {} 失败: {}", i, e))?;
        let name = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let mut comps = name.components();
        match comps.next() {
            Some(c) if c.as_os_str() == "wordpress" => {}
            _ => continue,
        }
        let rel: PathBuf = comps.collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = dir.join(rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&dest)
                .map_err(|e| format!("创建目录 {} 失败: {}", dest.display(), e))?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建父目录 {} 失败: {}", parent.display(), e))?;
            }
            let mut out = std::fs::File::create(&dest)
                .map_err(|e| format!("创建文件 {} 失败: {}", dest.display(), e))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("写入 {} 失败: {}", dest.display(), e))?;
            wrote += 1;
        }
        let done = (i + 1) as u64;
        if done % 100 == 0 {
            emit_progress(app, "extracting", Some(done), Some(total_entries), Some("文件"));
        }
    }
    emit_progress(app, "extracting", Some(total_entries), Some(total_entries), Some("文件"));

    emit_log(app, sink, format!("已解压 {} 个文件", wrote));
    emit_log(app, sink, "提示：浏览器访问站点会进入 WordPress 安装向导".to_string());
    Ok(())
}

async fn init_composer(
    app: &AppHandle,
    sink: &Sink,
    dir: &Path,
    package: &str,
    state: &State<'_, AppState>,
) -> Result<(), String> {
    // composer create-project 自己也校验目录必须空 —— NaxOne create_vhost 写过的
    // nginx.htaccess / .htaccess 会让它拒绝。临时把这俩文件备份起来，跑完再放回。
    let htaccess_names = ["nginx.htaccess", ".htaccess"];
    let mut backups: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for name in htaccess_names {
        let p = dir.join(name);
        if p.exists() {
            if let Ok(content) = std::fs::read(&p) {
                backups.push((p.clone(), content));
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    let result = run_composer_inner(app, sink, dir, package, state).await;

    // 还原 htaccess（不管 composer 成功或失败都要恢复，否则下次启动 nginx reload 会找不到 htaccess 报 emerg）
    for (p, content) in backups {
        if !p.exists() {
            let _ = std::fs::write(&p, &content);
        }
    }

    result
}

async fn run_composer_inner(
    app: &AppHandle,
    sink: &Sink,
    dir: &Path,
    package: &str,
    state: &State<'_, AppState>,
) -> Result<(), String> {
    // 关键：必须用 NaxOne 内置 PHP + composer.phar，绕开 PATH。
    // 否则用户机器上的 C:\ProgramData\ComposerSetup / C:\php 会被 PATH 优先匹中，
    // 而那些往往缺 openssl 等扩展，composer create-project 会直接挂掉。
    let php = resolve_naxone_php(state).await
        .ok_or_else(|| "未找到可用的 PHP（请先在「软件商店」安装 PHP）".to_string())?;
    let phar = resolve_naxone_composer_phar(state).await
        .ok_or_else(|| "未找到 composer.phar（请先在「软件商店」安装 Composer）".to_string())?;

    // 找一个跟 php.exe 同目录的 ini 文件，强制 -c 指定。
    // 否则 PHP 会去注册表 / PHPRC / 系统 PATH 找 php.ini —— 用户机器装了独立 PHP
    // 在 C:\php 时会命中那个的 php.ini（extension_dir 指向 C:\php\ext），扩展全报"找不到"。
    let php_ini = find_php_ini(&php);
    // 商店装的 PHP 包 php.ini 默认把 extension_dir 全注释了 → PHP fallback 到
    // 编译时默认 (`C:\php\ext`)。这里用 -d 强制覆盖，指向 NaxOne PHP 自带的 ext/。
    let ext_dir = php
        .parent()
        .map(|d| d.join("ext"))
        .filter(|p| p.is_dir());

    emit_log(app, sink, format!("PHP:      {}", php.display()));
    if let Some(ref ini) = php_ini {
        emit_log(app, sink, format!("php.ini:  {}", ini.display()));
    } else {
        emit_log(app, sink, "php.ini:  (未找到，使用 PHP 默认查找)".to_string());
    }
    if let Some(ref e) = ext_dir {
        emit_log(app, sink, format!("ext_dir:  {}", e.display()));
    }
    emit_log(app, sink, format!("Composer: {}", phar.display()));
    emit_log(
        app,
        sink,
        format!("执行: composer create-project {} . --no-interaction", package),
    );

    let mut cmd = TokioCommand::new(&php);
    cmd.current_dir(dir);
    if let Some(ref ini) = php_ini {
        cmd.arg("-c").arg(ini);
    }
    if let Some(ref e) = ext_dir {
        cmd.arg("-d").arg(format!("extension_dir={}", e.display()));
    }
    cmd.arg(&phar)
        .arg("create-project")
        .arg(package)
        .arg(".")
        .arg("--no-interaction")
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 composer 失败: {}", e))?;
    // 把 PID 注册到 state，让 cancel_init_site_template 能找到并 taskkill
    if let Some(pid) = child.id() {
        *state.template_child_pid.lock().await = Some(pid);
        emit_log(app, sink, format!("composer 子进程 PID={}", pid));
    }
    let stdout = child.stdout.take().ok_or("无法获取 stdout")?;
    let stderr = child.stderr.take().ok_or("无法获取 stderr")?;

    let app_out = app.clone();
    let sink_out = sink.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = app_out.emit("site-template-log", line.clone());
            if let Ok(mut g) = sink_out.lock() {
                g.push(line);
            }
        }
    });
    let app_err = app.clone();
    let sink_err = sink.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let with_prefix = format!("[stderr] {}", line);
            let _ = app_err.emit("site-template-log", with_prefix.clone());
            if let Ok(mut g) = sink_err.lock() {
                g.push(with_prefix);
            }
        }
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("等待 composer 失败: {}", e))?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    // 清掉 PID 标记
    *state.template_child_pid.lock().await = None;

    if !status.success() {
        // 用 status.code() 判断是否被外部杀掉（taskkill /F 通常 exit code = 1 但来源不可考）。
        // 简单：信任用户操作，stderr 日志里会有 cancel 痕迹。
        return Err(format!(
            "composer create-project 失败 (exit {})",
            status
        ));
    }
    Ok(())
}

/// 杀掉当前正在跑的模板装包子进程及其进程树。
/// 用 `taskkill /F /T /PID` 递归杀（composer 会拉 git/npm 等子进程，单杀 PID 不够）。
#[tauri::command]
pub async fn cancel_init_site_template(state: State<'_, AppState>) -> Result<bool, String> {
    let pid = match *state.template_child_pid.lock().await {
        Some(pid) => pid,
        None => return Ok(false), // 没在跑
    };

    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("taskkill 失败: {}", e))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // PID 已不存在的话 taskkill 也会报错，视为 OK
            if !stderr.contains("not found") && !stderr.contains("找不到") {
                return Err(format!("taskkill 拒绝: {}", stderr.trim()));
            }
        }
    }

    *state.template_child_pid.lock().await = None;
    push_log(
        &state,
        LogLevel::Warn,
        "site-template",
        format!("用户中止模板装包 (PID={})", pid),
        None,
        None,
    )
    .await;
    Ok(true)
}

/// 拿一个可用的 PHP 可执行文件，**绕开系统 PATH**。
/// 优先级：
/// 1. 用户在「全局环境」设置的 `config.global_php_version` 对应的 PHP（首选）
/// 2. services 里第一个 PHP 实例（兜底）
/// 用真实 install_path/php.exe 而不是 shim —— 否则 find_php_ini / extension_dir 找的是
/// shim 同目录（~/.naxone/bin/），里面没有 php.ini / ext。
async fn resolve_naxone_php(state: &State<'_, AppState>) -> Option<PathBuf> {
    use naxone_core::domain::service::ServiceKind;

    let global_ver = state.config.read().await.general.global_php_version.clone();
    let services = state.services.read().await;

    // 1. 优先：用户设置的全局 PHP 版本
    if let Some(ver) = global_ver {
        if let Some(svc) = services
            .iter()
            .find(|s| s.kind == ServiceKind::Php && s.version == ver)
        {
            let exe = svc.install_path.join("php.exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }

    // 2. fallback：services 第一个 PHP（用户没设全局时）
    if let Some(svc) = services.iter().find(|s| s.kind == ServiceKind::Php) {
        let exe = svc.install_path.join("php.exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

/// 在 php.exe 同目录下找一个可用的 php.ini 文件。
/// 优先 php.ini（用户/打包者已配置好的），其次 -production / -development 模板。
fn find_php_ini(php_exe: &Path) -> Option<PathBuf> {
    let dir = php_exe.parent()?;
    for name in ["php.ini", "php.ini-production", "php.ini-development"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 拿 NaxOne 商店里装的 composer.phar 绝对路径。**不**调用 NaxOne 全局 shim
/// （shim 内部会调 PATH 里的 php，那就又被系统 PATH 拦截了）。
async fn resolve_naxone_composer_phar(state: &State<'_, AppState>) -> Option<PathBuf> {
    let config = state.config.read().await;
    let packages_root = crate::state::resolve_packages_root(&config);
    drop(config);
    let tools_dir = packages_root.join("tools");
    let entries = std::fs::read_dir(&tools_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("composer-") {
            continue;
        }
        let phar = entry.path().join("composer.phar");
        if phar.is_file() {
            return Some(phar);
        }
    }
    None
}

/// 清理目标目录里所有"非自家文件"，让重试 / 换镜像装包能通过空目录校验。
///
/// 保留白名单：`nginx.htaccess` + `.htaccess`（跟 `init_site_template` 入口校验白名单
/// 保持一致 —— 清完后那边的 real_files 检查必然通过）。
///
/// 安全门禁：`target_dir` 必须等于**某个已存在 vhost 的 document_root**，否则拒绝清理。
/// 即便前端被注入也无法删任意目录。
///
/// 返回清掉的顶层条目数（文件 + 子目录）。
#[tauri::command]
pub async fn cleanup_template_dir(
    target_dir: String,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let dir = PathBuf::from(&target_dir);
    if !dir.is_absolute() {
        return Err("target_dir 必须是绝对路径".into());
    }
    if !dir.is_dir() {
        return Err(format!("目录不存在: {}", dir.display()));
    }

    // 安全门禁：target_dir 必须匹配某个 vhost 的 document_root
    let norm_target = dir.to_string_lossy().replace('\\', "/").to_lowercase();
    let allowed = {
        let vhosts = state.vhosts.read().await;
        vhosts.iter().any(|v| {
            let r = v.document_root.to_string_lossy().replace('\\', "/").to_lowercase();
            r == norm_target
        })
    };
    if !allowed {
        return Err("拒绝清理：路径不是任何 vhost 的站点目录".into());
    }

    let keep = ["nginx.htaccess", ".htaccess"];
    let mut removed = 0usize;
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("读取目录失败: {}", e))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if keep.iter().any(|k| *k == s.as_ref()) {
            continue;
        }
        let p = entry.path();
        let res = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match res {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!("清理 {} 失败: {}", p.display(), e),
        }
    }

    push_log(
        &state,
        LogLevel::Info,
        "site-template",
        format!("清理模板装包残留 {} 项 → {}", removed, dir.display()),
        None,
        None,
    )
    .await;
    Ok(removed)
}
