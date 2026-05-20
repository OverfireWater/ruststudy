use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::service::{ServiceInstance, ServiceStatus};
use crate::error::Result;
use crate::ports::log_reporter::LogReporter;

/// Abstraction over OS-level process lifecycle management
#[async_trait]
pub trait ProcessManager: Send + Sync {
    /// Start a service process, returns the OS PID
    async fn start(&self, instance: &ServiceInstance) -> Result<u32>;

    /// Stop a running service
    async fn stop(&self, instance: &ServiceInstance) -> Result<()>;

    /// Restart a service (stop + start)
    async fn restart(&self, instance: &ServiceInstance) -> Result<u32>;

    /// Check current status of a service
    async fn status(&self, instance: &ServiceInstance) -> Result<ServiceStatus>;

    /// Send a reload signal (e.g., nginx -s reload)
    async fn reload(&self, instance: &ServiceInstance) -> Result<()>;

    /// 注入 LogReporter。Watchdog 重启 / 端口冲突等后台事件会通过这条通道
    /// 推到用户可见的活动日志。默认 no-op，未实现的 adapter 直接忽略。
    fn set_reporter(&self, _reporter: Arc<dyn LogReporter>) {}

    /// 接管已在跑的服务进程（典型场景：NaxOne 退出后 php-cgi 子进程作为孤儿继续监听端口；
    /// 重启 NaxOne 后通过端口探测 + exe 路径校验认领，并启动 watchdog 监控）。
    /// 返回 true 表示完成认领，false 表示没什么可认领的（端口空 / 不是 PHP / 已经被监控）。
    async fn adopt_if_running(&self, _instance: &ServiceInstance) -> Result<bool> {
        Ok(false)
    }
}
