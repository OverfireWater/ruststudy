use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::commands::logger::push_log;
use crate::state::AppState;
use naxone_core::domain::log::LogLevel;
use naxone_core::domain::service::{
    PhpRuntimeOptions, ServiceInstance, ServiceKind, ServiceOrigin, ServiceStatus,
};

#[derive(Debug, Clone, Serialize)]
pub struct ServiceInfo {
    pub id: String,
    pub kind: ServiceKind,
    pub display_name: String,
    pub version: String,
    pub variant: Option<String>,
    pub port: u16,
    pub status: ServiceStatus,
    pub install_path: String,
    /// "phpstudy" | "store" | "manual"
    pub origin: String,
    pub php_workers: Option<u16>,
    pub php_max_requests: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceBatchProgress {
    operation: String,
    id: String,
    status: ServiceStatus,
}

fn emit_batch_progress(app: &AppHandle, operation: &str, service: &ServiceInstance) {
    let _ = app.emit(
        "service-batch-progress",
        ServiceBatchProgress {
            operation: operation.to_string(),
            id: service.id(),
            status: service.status.clone(),
        },
    );
}

fn origin_str(o: &ServiceOrigin) -> &'static str {
    match o {
        ServiceOrigin::PhpStudy => "phpstudy",
        ServiceOrigin::Store => "store",
        ServiceOrigin::Manual => "manual",
        ServiceOrigin::System => "system",
    }
}

fn to_info(s: &ServiceInstance) -> ServiceInfo {
    ServiceInfo {
        id: s.id(),
        kind: s.kind,
        display_name: format!("{} {}", s.kind.display_name(), s.version),
        version: s.version.clone(),
        variant: s.variant.clone(),
        port: s.port,
        status: s.status.clone(),
        install_path: s.install_path.display().to_string(),
        origin: origin_str(&s.origin).to_string(),
        php_workers: s.php_runtime.as_ref().map(|runtime| runtime.workers),
        php_max_requests: s.php_runtime.as_ref().map(|runtime| runtime.max_requests),
    }
}

fn all_infos(services: &[ServiceInstance]) -> Vec<ServiceInfo> {
    services.iter().map(to_info).collect()
}

fn find_service_index(services: &[ServiceInstance], id: &str) -> Result<usize, String> {
    services
        .iter()
        .position(|s| s.id() == id)
        .ok_or_else(|| format!("Service not found: {}", id))
}

fn sync_updated_statuses(
    services: &mut [ServiceInstance],
    target: &ServiceInstance,
    others: &[ServiceInstance],
) {
    for svc in services.iter_mut() {
        if svc.id() == target.id() {
            *svc = target.clone();
            continue;
        }
        if let Some(updated) = others.iter().find(|o| o.id() == svc.id()) {
            *svc = updated.clone();
        }
    }
}

fn sync_all_statuses(dest: &mut [ServiceInstance], src: &[ServiceInstance]) {
    for svc in dest.iter_mut() {
        if let Some(updated) = src.iter().find(|s| s.id() == svc.id()) {
            *svc = updated.clone();
        }
    }
}

/// Build a service snapshot with the active-vhost PHP worker allocation applied.
pub async fn configured_services_snapshot(state: &AppState) -> Vec<ServiceInstance> {
    let mut services = state.services.read().await.clone();
    let vhosts = state.vhosts.read().await.clone();
    let php_runtime = state.config.read().await.php_runtime.clone();
    naxone_core::use_cases::php_runtime::apply_php_runtime_policy(
        &mut services,
        &vhosts,
        &php_runtime,
    );
    services
}

/// Reconcile running PHP pools after vhost changes.
///
/// This is intentionally limited to Nginx mode. Apache uses mod_fcgid and
/// manages its own php-cgi children.
pub async fn reconcile_php_runtime(state: &AppState) -> Result<(), String> {
    let _reconcile_guard = state.php_reconcile_lock.lock().await;
    let current = state.services.read().await.clone();
    let nginx_running = current
        .iter()
        .any(|s| s.kind == ServiceKind::Nginx && s.status.is_running());
    let desired = configured_services_snapshot(state).await;

    if !nginx_running {
        let mut services = state.services.write().await;
        for service in services.iter_mut().filter(|s| s.kind == ServiceKind::Php) {
            if let Some(planned) = desired.iter().find(|s| s.id() == service.id()) {
                service.php_runtime = planned.php_runtime.clone();
            }
        }
        return Ok(());
    }

    let mut updated = current;
    let mut errors = Vec::new();
    let mut changed = false;
    for planned in desired.iter().filter(|s| s.kind == ServiceKind::Php) {
        let Some(idx) = updated.iter().position(|s| s.id() == planned.id()) else {
            continue;
        };
        let was_running = updated[idx].status.is_running();
        let runtime_changed = updated[idx].php_runtime != planned.php_runtime;
        updated[idx].php_runtime = planned.php_runtime.clone();

        let result = match (&planned.php_runtime, was_running, runtime_changed) {
            (Some(_), false, _) => {
                changed = true;
                state.service_manager.start_service(&mut updated[idx]).await
            }
            (Some(_), true, true) => {
                changed = true;
                state.service_manager.restart_service(&mut updated[idx]).await
            }
            (None, true, _) => {
                changed = true;
                state.service_manager.stop_service(&mut updated[idx]).await
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            errors.push(format!("PHP {}: {}", planned.version, error));
        }
    }

    {
        let mut services = state.services.write().await;
        sync_all_statuses(&mut services, &updated);
    }

    if errors.is_empty() {
        if !changed {
            return Ok(());
        }
        let summary = desired
            .iter()
            .filter_map(|s| {
                s.php_runtime
                    .as_ref()
                    .map(|runtime| format!("{}={} workers", s.version, runtime.workers))
            })
            .collect::<Vec<_>>()
            .join(", ");
        push_log(
            state,
            LogLevel::Info,
            "service",
            "PHP FastCGI 池已按启用站点重新分配",
            (!summary.is_empty()).then_some(summary),
            None,
        )
        .await;
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

pub fn schedule_php_runtime_reconcile(state: &AppState) {
    let state = state.clone_shallow();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = reconcile_php_runtime(&state).await {
            push_log(
                &state,
                LogLevel::Warn,
                "service",
                "PHP FastCGI 池自动调整失败",
                Some(error),
                None,
            )
            .await;
        }
    });
}

fn ensure_manual_php_runtime(
    target: &mut ServiceInstance,
    config: &naxone_core::config::PhpRuntimeConfig,
) {
    if target.kind != ServiceKind::Php || target.php_runtime.is_some() {
        return;
    }
    target.php_runtime = Some(PhpRuntimeOptions {
        workers: if config.phpstudy_compatible_workers {
            16
        } else {
            config
                .total_worker_budget
                .min(config.max_workers_per_version)
                .clamp(1, 16)
        },
        max_requests: config.max_requests.clamp(100, 10_000),
    });
}

/// 后台并行刷新所有服务状态。single-flight：已有任务在跑时直接跳过。
/// 先在不持锁的情况下并行跑 status()，最后只短暂拿 write 锁写字段，避免锁阻塞。
pub async fn refresh_all_services_bg(state: AppState) {
    use std::sync::atomic::Ordering;
    if state
        .refresh_in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let snapshot = { state.services.read().await.clone() };
    let results = futures_util::future::join_all(snapshot.iter().map(|inst| {
        let mgr = &state.service_manager;
        async move { (inst.id(), mgr.refresh_status_value(inst).await) }
    }))
    .await;

    {
        let mut services = state.services.write().await;
        for (id, status) in results {
            if let Some(s) = services.iter_mut().find(|s| s.id() == id) {
                if let Ok(st) = status {
                    s.status = st;
                }
            }
        }
    }

    state.refresh_in_flight.store(false, Ordering::Release);
}

#[tauri::command]
pub async fn get_services(state: State<'_, AppState>) -> Result<Vec<ServiceInfo>, String> {
    let snapshot = { state.services.read().await.clone() };
    let bg_state = state.inner().clone_shallow();
    tauri::async_runtime::spawn(async move {
        refresh_all_services_bg(bg_state).await;
    });
    Ok(all_infos(&snapshot))
}

#[tauri::command]
pub async fn get_services_fresh(state: State<'_, AppState>) -> Result<Vec<ServiceInfo>, String> {
    refresh_all_services_bg(state.inner().clone_shallow()).await;
    let services = state.services.read().await;
    Ok(all_infos(&services))
}

#[allow(dead_code)]
async fn get_services_legacy(state: State<'_, AppState>) -> Result<Vec<ServiceInfo>, String> {
    let mut services = state.services.write().await;
    for instance in services.iter_mut() {
        let _ = state.service_manager.refresh_status(instance).await;
    }
    Ok(all_infos(&services))
}

#[tauri::command]
pub async fn start_service(id: String, state: State<'_, AppState>) -> Result<Vec<ServiceInfo>, String> {
    let snapshot = configured_services_snapshot(state.inner()).await;
    let idx = find_service_index(&snapshot, &id)?;
    let mut target = snapshot[idx].clone();
    let php_runtime = state.config.read().await.php_runtime.clone();
    ensure_manual_php_runtime(&mut target, &php_runtime);
    let mut others: Vec<_> = snapshot
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, s)| s.clone())
        .collect();

    let name = format!("{} {}", target.kind.display_name(), target.version);
    push_log(&state, LogLevel::Info, "service", format!("启动 {}", name), None, None).await;

    match state.service_manager.start_with_deps(&mut target, &mut others).await {
        Ok(_) => {
            let pid = match &target.status {
                ServiceStatus::Running { pid, .. } => *pid,
                _ => 0,
            };
            push_log(
                &state,
                LogLevel::Success,
                "service",
                format!("{} 启动成功（PID {}）", name, pid),
                None,
                None,
            )
            .await;
        }
        Err(e) => {
            let msg = e.to_string();
            push_log(
                &state,
                LogLevel::Error,
                "service",
                format!("{} 启动失败", name),
                Some(msg.clone()),
                None,
            )
            .await;
            return Err(msg);
        }
    }

    let mut services = state.services.write().await;
    sync_updated_statuses(&mut services, &target, &others);
    Ok(all_infos(&services))
}

#[tauri::command]
pub async fn stop_service(id: String, state: State<'_, AppState>) -> Result<Vec<ServiceInfo>, String> {
    let snapshot = { state.services.read().await.clone() };
    let idx = find_service_index(&snapshot, &id)?;
    let mut target = snapshot[idx].clone();
    let mut others: Vec<_> = snapshot
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, s)| s.clone())
        .collect();

    let name = format!("{} {}", target.kind.display_name(), target.version);
    push_log(&state, LogLevel::Info, "service", format!("停止 {}", name), None, None).await;

    let result = if target.kind == ServiceKind::Nginx {
        state.service_manager.stop_with_deps(&mut target, &mut others).await
    } else {
        state.service_manager.stop_service(&mut target).await
    };

    match result {
        Ok(_) => {
            push_log(
                &state,
                LogLevel::Success,
                "service",
                format!("{} 已停止", name),
                None,
                None,
            )
            .await;
        }
        Err(e) => {
            let msg = e.to_string();
            push_log(
                &state,
                LogLevel::Error,
                "service",
                format!("{} 停止失败", name),
                Some(msg.clone()),
                None,
            )
            .await;
            return Err(msg);
        }
    }

    let mut services = state.services.write().await;
    sync_updated_statuses(&mut services, &target, &others);
    Ok(all_infos(&services))
}

#[tauri::command]
pub async fn restart_service(
    id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ServiceInfo>, String> {
    let snapshot = configured_services_snapshot(state.inner()).await;
    let idx = find_service_index(&snapshot, &id)?;

    let mut target = snapshot[idx].clone();
    let php_runtime = state.config.read().await.php_runtime.clone();
    ensure_manual_php_runtime(&mut target, &php_runtime);
    let mut others: Vec<_> = snapshot
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, s)| s.clone())
        .collect();

    let name = format!("{} {}", target.kind.display_name(), target.version);
    push_log(&state, LogLevel::Info, "service", format!("重启 {}", name), None, None).await;

    // Nginx 重启需要 PHP 跟着 stop+start（用户要求），走 stop_with_deps。
    // Apache 不带 PHP，普通 stop+start_with_deps（start_with_deps 内部已对 apache 跳过 PHP 联动）。
    let result = if target.kind == ServiceKind::Nginx {
        match state.service_manager.stop_with_deps(&mut target, &mut others).await {
            Err(e) => Err(e),
            Ok(_) => state.service_manager.start_with_deps(&mut target, &mut others).await,
        }
    } else if target.kind == ServiceKind::Apache {
        if let Err(e) = state.service_manager.stop_service(&mut target).await {
            Err(e)
        } else {
            state
                .service_manager
                .start_with_deps(&mut target, &mut others)
                .await
        }
    } else {
        state.service_manager.restart_service(&mut target).await
    };

    if let Err(e) = result {
        let msg = e.to_string();
        push_log(
            &state,
            LogLevel::Error,
            "service",
            format!("{} 重启失败", name),
            Some(msg.clone()),
            None,
        )
        .await;
        return Err(msg);
    }

    push_log(
        &state,
        LogLevel::Success,
        "service",
        format!("{} 已重启", name),
        None,
        None,
    )
    .await;

    let mut services = state.services.write().await;
    sync_updated_statuses(&mut services, &target, &others);
    Ok(all_infos(&services))
}

#[tauri::command]
pub async fn start_all(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ServiceInfo>, String> {
    let mut working = configured_services_snapshot(state.inner()).await;
    let config = state.config.read().await;
    let active_web = config.web_server.active.clone();
    drop(config);

    push_log(
        &state,
        LogLevel::Info,
        "service",
        format!("全部启动：开始（活跃 web = {}）", active_web),
        None,
        None,
    )
    .await;

    let mut errors = Vec::new();
    let running_kinds: std::collections::HashSet<ServiceKind> = working
        .iter()
        .filter(|s| s.kind != ServiceKind::Php && s.status.is_running())
        .map(|s| s.kind)
        .collect();
    let mut selected_kinds = std::collections::HashSet::new();
    let mut target_indices = Vec::new();

    for idx in 0..working.len() {
        let kind = working[idx].kind;
        match kind {
            ServiceKind::Nginx if active_web != "nginx" => continue,
            ServiceKind::Apache if active_web != "apache" => continue,
            // PHP 由 web server 启动时连带（仅 nginx 模式）；apache 自己 fork 不需要常驻 PHP。
            ServiceKind::Php => continue,
            _ => {}
        }

        // 同 kind 多版本只选一个；若该类已有运行实例，整类无需重复启动。
        if running_kinds.contains(&kind) || !selected_kinds.insert(kind) {
            continue;
        }
        target_indices.push(idx);
    }

    // 先向首页推 Starting，让卡片无需等整个批处理结束。
    for &idx in &target_indices {
        let mut progress = working[idx].clone();
        progress.status = ServiceStatus::Starting;
        emit_batch_progress(&app, "start", &progress);
    }
    if target_indices
        .iter()
        .any(|&idx| working[idx].kind == ServiceKind::Nginx)
    {
        for php in working.iter().filter(|s| {
            s.kind == ServiceKind::Php && s.php_runtime.is_some() && !s.status.is_running()
        }) {
            let mut progress = php.clone();
            progress.status = ServiceStatus::Starting;
            emit_batch_progress(&app, "start", &progress);
        }
    }

    // Web/PHP、MySQL、Redis 端口互不依赖，按服务组并行启动。
    let snapshot = working.clone();
    let futures = target_indices.into_iter().map(|idx| {
        let manager = state.service_manager.clone();
        let base = snapshot.clone();
        let app = app.clone();
        async move {
            let mut target = base[idx].clone();
            let mut others: Vec<_> = base
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, s)| s.clone())
                .collect();

            let result = if matches!(target.kind, ServiceKind::Nginx | ServiceKind::Apache) {
                manager.start_with_deps(&mut target, &mut others).await
            } else {
                manager.start_service(&mut target).await
            };

            let mut updates = vec![target.clone()];
            updates.extend(others.into_iter().filter(|updated| {
                base.iter()
                    .find(|original| original.id() == updated.id())
                    .is_some_and(|original| original.status != updated.status)
            }));
            for updated in &updates {
                emit_batch_progress(&app, "start", updated);
            }
            (target, updates, result)
        }
    });

    for (target, updates, result) in futures_util::future::join_all(futures).await {
        if let Err(e) = result {
            let msg = format!("{} {} 启动失败: {}", target.kind.display_name(), target.version, e);
            errors.push(msg);
            push_log(
                &state,
                LogLevel::Error,
                "service",
                format!("{} {} 启动失败", target.kind.display_name(), target.version),
                Some(e.to_string()),
                None,
            )
            .await;
        }

        for updated in updates {
            if let Some(service) = working.iter_mut().find(|s| s.id() == updated.id()) {
                *service = updated;
            }
        }
    }

    {
        let mut services = state.services.write().await;
        sync_all_statuses(&mut services, &working);
    }

    if errors.is_empty() {
        push_log(
            &state,
            LogLevel::Success,
            "service",
            "全部启动：完成",
            None,
            None,
        )
        .await;
    } else {
        push_log(
            &state,
            LogLevel::Warn,
            "service",
            format!("全部启动：完成（{} 个失败）", errors.len()),
            Some(errors.join("\n")),
            None,
        )
        .await;
    }

    let services = state.services.read().await;
    let infos = all_infos(&services);
    drop(services);

    if errors.is_empty() {
        Ok(infos)
    } else {
        Err(errors.join("\n"))
    }
}

/// 给前端按钮（如手动刷新）写一条日志的小工具命令。前端只能通过命令触发后端 push_log。
#[tauri::command]
pub async fn log_user_action(
    message: String,
    details: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    push_log(&state, LogLevel::Info, "user", message, details, None).await;
    Ok(())
}

#[tauri::command]
pub async fn stop_all(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ServiceInfo>, String> {
    let working = { state.services.read().await.clone() };

    push_log(
        &state,
        LogLevel::Info,
        "service",
        format!("全部停止：开始（{} 个服务）", working.len()),
        None,
        None,
    )
    .await;

    for service in working.iter().filter(|s| s.status.is_running()) {
        let mut progress = service.clone();
        progress.status = ServiceStatus::Stopping;
        emit_batch_progress(&app, "stop", &progress);
    }

    // 并行停止：每个 stop_service 内部会优雅停 + 等端口释放，最长 ~3s。
    // 串行下 N 个服务就要 N×3s，前端 10s 超时直接撞墙；服务互相独立可并行。
    let mgr = state.service_manager.clone();
    let futures = working.into_iter().map(|mut svc| {
        let mgr = mgr.clone();
        let app = app.clone();
        async move {
            let res = mgr.stop_service(&mut svc).await;
            emit_batch_progress(&app, "stop", &svc);
            (svc, res)
        }
    });
    let results = futures_util::future::join_all(futures).await;

    let mut working: Vec<ServiceInstance> = Vec::with_capacity(results.len());
    let mut errors = Vec::new();
    for (svc, res) in results {
        if let Err(e) = res {
            let msg = format!("{} {} 停止失败: {}", svc.kind.display_name(), svc.version, e);
            errors.push(msg.clone());
            push_log(
                &state,
                LogLevel::Error,
                "service",
                format!("{} {} 停止失败", svc.kind.display_name(), svc.version),
                Some(e.to_string()),
                None,
            )
            .await;
        }
        working.push(svc);
    }

    {
        let mut services = state.services.write().await;
        sync_all_statuses(&mut services, &working);
    }

    if errors.is_empty() {
        push_log(
            &state,
            LogLevel::Success,
            "service",
            "全部停止：完成",
            None,
            None,
        )
        .await;
    } else {
        push_log(
            &state,
            LogLevel::Warn,
            "service",
            format!("全部停止：完成（{} 个失败）", errors.len()),
            Some(errors.join("\n")),
            None,
        )
        .await;
    }

    let services = state.services.read().await;
    let infos = all_infos(&services);
    drop(services);

    if errors.is_empty() {
        Ok(infos)
    } else {
        Err(errors.join("\n"))
    }
}
