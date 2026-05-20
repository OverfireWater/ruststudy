use std::sync::Arc;

use crate::domain::service::{ServiceInstance, ServiceKind, ServiceStatus};
use crate::error::{Result, NaxOneError};
use crate::ports::process::ProcessManager;

/// Orchestrates service lifecycle operations
#[derive(Clone)]
pub struct ServiceManager {
    process_mgr: Arc<dyn ProcessManager>,
}

impl ServiceManager {
    pub fn new(process_mgr: Arc<dyn ProcessManager>) -> Self {
        Self { process_mgr }
    }

    pub async fn start_service(&self, instance: &mut ServiceInstance) -> Result<()> {
        if instance.status.is_running() {
            return Ok(());
        }
        instance.status = ServiceStatus::Starting;
        let pid = self.process_mgr.start(instance).await?;
        instance.status = ServiceStatus::Running { pid, memory_mb: None };
        Ok(())
    }

    pub async fn stop_service(&self, instance: &mut ServiceInstance) -> Result<()> {
        if !instance.status.is_running() {
            return Ok(());
        }
        instance.status = ServiceStatus::Stopping;
        self.process_mgr.stop(instance).await?;
        instance.status = ServiceStatus::Stopped;
        Ok(())
    }

    pub async fn restart_service(&self, instance: &mut ServiceInstance) -> Result<()> {
        self.stop_service(instance).await?;
        self.start_service(instance).await?;
        Ok(())
    }

    pub async fn refresh_status(&self, instance: &mut ServiceInstance) -> Result<()> {
        instance.status = self.process_mgr.status(instance).await?;
        Ok(())
    }

    /// 只读版：返回最新 status，不修改 instance。供只读快照 + 后台并行刷新使用。
    pub async fn refresh_status_value(&self, instance: &ServiceInstance) -> Result<ServiceStatus> {
        self.process_mgr.status(instance).await
    }

    /// Start a service with dependency and mutual-exclusion awareness:
    /// - Nginx and Apache are mutually exclusive (both use port 80), starting one stops the other
    /// - Starting Nginx/Apache also auto-starts PHP-CGI instances
    pub async fn start_with_deps(
        &self,
        target: &mut ServiceInstance,
        all_services: &mut [ServiceInstance],
    ) -> Result<()> {
        let is_web_server = matches!(target.kind, ServiceKind::Nginx | ServiceKind::Apache);
        // 非 PHP 类服务（web + db + cache）必须是单实例：端口只有一份
        let needs_single_instance = target.kind != ServiceKind::Php;

        if is_web_server {
            // Mutual exclusion: stop the other web server if running (both use port 80).
            // 若停对方失败或实际未停止 → 中止启动，避免 bind 冲突产生误导性 AH00015。
            let rival_kind = match target.kind {
                ServiceKind::Nginx => ServiceKind::Apache,
                _ => ServiceKind::Nginx,
            };
            for svc in all_services.iter_mut() {
                if svc.kind == rival_kind && svc.status.is_running() {
                    if let Err(e) = self.stop_service(svc).await {
                        return Err(NaxOneError::Process(format!(
                            "无法停止 {}（占用端口 80）: {}",
                            rival_kind.display_name(),
                            e
                        )));
                    }
                    // 再验证一次：确认对方确实不在 running 状态
                    // ProcessManager::status 内部已用 TCP probe + 超时，不需要再加延时
                    if let Ok(status) = self.process_mgr.status(svc).await {
                        svc.status = status.clone();
                    }
                    if svc.status.is_running() {
                        return Err(NaxOneError::Process(format!(
                            "{} 停止后仍在运行（可能是以管理员身份启动的外部进程），已中止启动 {}",
                            rival_kind.display_name(),
                            target.kind.display_name()
                        )));
                    }
                }
            }
        }

        // 同 kind 多版本互斥：启动目标版本前，停止同 kind 其他版本（端口只有一份）
        if needs_single_instance {
            let target_id = target.id();
            for svc in all_services.iter_mut() {
                if svc.kind == target.kind && svc.id() != target_id && svc.status.is_running() {
                    if let Err(e) = self.stop_service(svc).await {
                        return Err(NaxOneError::Process(format!(
                            "无法停止同类旧版本 {} {}：{}",
                            svc.kind.display_name(),
                            svc.version,
                            e
                        )));
                    }
                    if let Ok(status) = self.process_mgr.status(svc).await {
                        svc.status = status.clone();
                    }
                    if svc.status.is_running() {
                        return Err(NaxOneError::Process(format!(
                            "{} {} 停止后仍在运行，已中止启动 {}",
                            svc.kind.display_name(),
                            svc.version,
                            target.version
                        )));
                    }
                }
            }
        }

        // Start the target service itself
        self.start_service(target).await?;

        // 仅 Nginx 模式需要连带启 PHP-CGI 常驻：fastcgi_pass 必须有进程在 9000+ 监听。
        // Apache 用 mod_fcgid，自己按需 fork php-cgi 子进程，无需主动启动。
        //
        // 优化：5 个 PHP 串行启动 = 5 × 500ms（每个 start 的固定 sleep 兜底）= 2.5s 阻塞，
        // 改用 futures::join 并行 spawn。各 PHP 用独立端口互不影响。
        if target.kind == ServiceKind::Nginx {
            let php_indices: Vec<usize> = all_services
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == ServiceKind::Php && !s.status.is_running())
                .map(|(i, _)| i)
                .collect();

            if !php_indices.is_empty() {
                let futures = php_indices.iter().map(|&i| {
                    let svc = &all_services[i];
                    let pm = self.process_mgr.clone();
                    async move {
                        let mut local = svc.clone();
                        let res = pm.start(&local).await.map(|pid| {
                            local.status = crate::domain::service::ServiceStatus::Running { pid, memory_mb: None };
                            local
                        });
                        (i, res)
                    }
                });
                let results = futures_util::future::join_all(futures).await;
                // 关键：不再遇错就 return。把所有结果都处理完，成功的写回状态、
                // 失败的收集到一起。否则一个 PHP 启动失败会让其它已经在跑的 PHP
                // 的状态没被更新（虽然进程已经 spawn），用户看到的 UI 数据错位。
                let mut failed: Vec<String> = Vec::new();
                for (idx, res) in results {
                    match res {
                        Ok(updated) => all_services[idx].status = updated.status,
                        Err(e) => {
                            failed.push(format!(
                                "{} {}: {}",
                                all_services[idx].kind.display_name(),
                                all_services[idx].version,
                                e
                            ));
                        }
                    }
                }
                if !failed.is_empty() {
                    return Err(NaxOneError::Process(format!(
                        "联动启动 {} 个 PHP-CGI 失败:\n{}",
                        failed.len(),
                        failed.join("\n")
                    )));
                }
            }
        }

        Ok(())
    }

    /// 停止服务并处理依赖：
    /// - Nginx 停止后连带停所有 PHP-CGI（fastcgi_pass 没人消费了）
    /// - Apache 停止后无需处理（mod_fcgid 子进程随 apache 一起退）
    /// PHP 停止失败只警告，不让整个停止操作 fail —— web 已停，PHP 状态不一致下次刷新会修正。
    pub async fn stop_with_deps(
        &self,
        target: &mut ServiceInstance,
        all_services: &mut [ServiceInstance],
    ) -> Result<()> {
        self.stop_service(target).await?;

        // 停 nginx 后并行停所有 PHP-CGI（无依赖关系，5 个串行 stop 太慢）。
        if target.kind == ServiceKind::Nginx {
            let php_indices: Vec<usize> = all_services
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == ServiceKind::Php && s.status.is_running())
                .map(|(i, _)| i)
                .collect();

            if !php_indices.is_empty() {
                let futures = php_indices.iter().map(|&i| {
                    let svc = all_services[i].clone();
                    let pm = self.process_mgr.clone();
                    async move {
                        let res = pm.stop(&svc).await;
                        (i, svc.version.clone(), res)
                    }
                });
                let results = futures_util::future::join_all(futures).await;
                for (idx, version, res) in results {
                    match res {
                        Ok(_) => {
                            all_services[idx].status = crate::domain::service::ServiceStatus::Stopped;
                        }
                        Err(e) => {
                            tracing::warn!(
                                version = %version,
                                error = %e,
                                "联动停止 PHP-CGI 失败，继续",
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
