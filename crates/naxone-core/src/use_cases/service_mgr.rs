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
        match self.process_mgr.start(instance).await {
            Ok(pid) => {
                instance.status = ServiceStatus::Running { pid, memory_mb: None };
                Ok(())
            }
            Err(error) => {
                instance.status = ServiceStatus::Failed {
                    reason: error.to_string(),
                };
                Err(error)
            }
        }
    }

    pub async fn stop_service(&self, instance: &mut ServiceInstance) -> Result<()> {
        if !instance.status.is_running() {
            return Ok(());
        }
        instance.status = ServiceStatus::Stopping;
        match self.process_mgr.stop(instance).await {
            Ok(()) => {
                instance.status = ServiceStatus::Stopped;
                Ok(())
            }
            Err(error) => {
                instance.status = ServiceStatus::Failed {
                    reason: error.to_string(),
                };
                Err(error)
            }
        }
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

        // 仅 Nginx 模式需要连带启 PHP-CGI 常驻：fastcgi_pass 必须有进程在 9000+ 监听。
        // Apache 用 mod_fcgid，自己按需 fork php-cgi 子进程，无需主动启动。
        // Nginx 与 PHP 使用不同端口，没有“必须先启动 Nginx”的进程依赖，故一起并行启动。
        if target.kind == ServiceKind::Nginx {
            let mut jobs: Vec<(Option<usize>, ServiceInstance)> = Vec::new();
            if !target.status.is_running() {
                jobs.push((None, target.clone()));
            }
            jobs.extend(
                all_services
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        s.kind == ServiceKind::Php
                            && s.php_runtime.is_some()
                            && !s.status.is_running()
                    })
                    .map(|(i, s)| (Some(i), s.clone())),
            );

            if !jobs.is_empty() {
                let futures = jobs.into_iter().map(|(index, mut local)| {
                    let pm = self.process_mgr.clone();
                    async move {
                        let label = format!("{} {}", local.kind.display_name(), local.version);
                        let res = pm.start(&local).await.map(|pid| {
                            local.status = crate::domain::service::ServiceStatus::Running {
                                pid,
                                memory_mb: None,
                            };
                            local
                        });
                        (index, label, res)
                    }
                });
                let results = futures_util::future::join_all(futures).await;
                let mut failed: Vec<String> = Vec::new();
                for (index, label, res) in results {
                    match res {
                        Ok(updated) => match index {
                            Some(idx) => all_services[idx].status = updated.status,
                            None => target.status = updated.status,
                        },
                        Err(e) => {
                            failed.push(format!("{}: {}", label, e));
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
        } else {
            self.start_service(target).await?;
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
        if target.kind != ServiceKind::Nginx {
            return self.stop_service(target).await;
        }

        // Nginx 与 PHP 没有停止顺序依赖，并行关闭可避免重启时把两段等待相加。
        let mut jobs: Vec<(Option<usize>, ServiceInstance)> = Vec::new();
        if target.status.is_running() {
            jobs.push((None, target.clone()));
        }
        jobs.extend(
            all_services
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == ServiceKind::Php && s.status.is_running())
                .map(|(i, s)| (Some(i), s.clone())),
        );

        let futures = jobs.into_iter().map(|(index, mut local)| {
            let pm = self.process_mgr.clone();
            async move {
                local.status = ServiceStatus::Stopping;
                let result = pm.stop(&local).await;
                local.status = match &result {
                    Ok(()) => ServiceStatus::Stopped,
                    Err(error) => ServiceStatus::Failed {
                        reason: error.to_string(),
                    },
                };
                (index, local, result)
            }
        });

        let mut target_error = None;
        for (index, updated, result) in futures_util::future::join_all(futures).await {
            match index {
                None => {
                    target.status = updated.status;
                    if let Err(error) = result {
                        target_error = Some(error);
                    }
                }
                Some(idx) => {
                    all_services[idx].status = updated.status;
                    if let Err(error) = result {
                        tracing::warn!(
                            version = %all_services[idx].version,
                            error = %error,
                            "联动停止 PHP-CGI 失败，继续",
                        );
                    }
                }
            }
        }

        if let Some(error) = target_error {
            return Err(error);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::service::{PhpRuntimeOptions, ServiceOrigin};
    use crate::ports::process::ProcessManager;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct TimedProcessManager {
        active: AtomicUsize,
        max_active: AtomicUsize,
        next_pid: AtomicU32,
        fail_start: bool,
    }

    impl TimedProcessManager {
        fn new(fail_start: bool) -> Self {
            Self {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                next_pid: AtomicU32::new(1000),
                fail_start,
            }
        }
    }

    #[async_trait]
    impl ProcessManager for TimedProcessManager {
        async fn start(&self, _instance: &ServiceInstance) -> Result<u32> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.fail_start {
                Err(NaxOneError::Process("模拟启动失败".into()))
            } else {
                Ok(self.next_pid.fetch_add(1, Ordering::SeqCst))
            }
        }

        async fn stop(&self, _instance: &ServiceInstance) -> Result<()> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        async fn restart(&self, instance: &ServiceInstance) -> Result<u32> {
            self.start(instance).await
        }

        async fn status(&self, instance: &ServiceInstance) -> Result<ServiceStatus> {
            Ok(instance.status.clone())
        }

        async fn reload(&self, _instance: &ServiceInstance) -> Result<()> {
            Ok(())
        }
    }

    fn service(kind: ServiceKind, version: &str, port: u16) -> ServiceInstance {
        ServiceInstance {
            kind,
            version: version.into(),
            variant: None,
            install_path: PathBuf::from("D:/test"),
            config_path: None,
            port,
            status: ServiceStatus::Stopped,
            auto_start: false,
            origin: ServiceOrigin::Manual,
            php_runtime: (kind == ServiceKind::Php).then_some(PhpRuntimeOptions {
                workers: 4,
                max_requests: 1000,
            }),
        }
    }

    #[tokio::test]
    async fn nginx_and_php_dependencies_start_in_parallel() {
        let process = Arc::new(TimedProcessManager::new(false));
        let manager = ServiceManager::new(process.clone());
        let mut nginx = service(ServiceKind::Nginx, "1.28.0", 80);
        let mut others = vec![
            service(ServiceKind::Php, "8.4.0", 9001),
            service(ServiceKind::Php, "8.5.0", 9002),
        ];

        let started = Instant::now();
        manager
            .start_with_deps(&mut nginx, &mut others)
            .await
            .unwrap();

        assert!(started.elapsed() < Duration::from_millis(180));
        assert!(process.max_active.load(Ordering::SeqCst) >= 3);
        assert!(nginx.status.is_running());
        assert!(others.iter().all(|service| service.status.is_running()));
    }

    #[tokio::test]
    async fn failed_start_does_not_leave_starting_status() {
        let manager = ServiceManager::new(Arc::new(TimedProcessManager::new(true)));
        let mut redis = service(ServiceKind::Redis, "7.0.0", 6379);

        assert!(manager.start_service(&mut redis).await.is_err());
        assert!(matches!(redis.status, ServiceStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn nginx_and_php_dependencies_stop_in_parallel() {
        let process = Arc::new(TimedProcessManager::new(false));
        let manager = ServiceManager::new(process.clone());
        let running = |mut service: ServiceInstance, pid| {
            service.status = ServiceStatus::Running {
                pid,
                memory_mb: None,
            };
            service
        };
        let mut nginx = running(service(ServiceKind::Nginx, "1.28.0", 80), 100);
        let mut others = vec![
            running(service(ServiceKind::Php, "8.4.0", 9001), 101),
            running(service(ServiceKind::Php, "8.5.0", 9002), 102),
        ];

        let stopped = Instant::now();
        manager
            .stop_with_deps(&mut nginx, &mut others)
            .await
            .unwrap();

        assert!(stopped.elapsed() < Duration::from_millis(180));
        assert!(process.max_active.load(Ordering::SeqCst) >= 3);
        assert_eq!(nginx.status, ServiceStatus::Stopped);
        assert!(others
            .iter()
            .all(|service| service.status == ServiceStatus::Stopped));
    }
}
