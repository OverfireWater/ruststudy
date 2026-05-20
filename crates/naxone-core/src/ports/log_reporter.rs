use crate::domain::log::LogLevel;

/// 后台模块（如 watchdog、并行启动）向用户活动日志推送事件的通道。
///
/// naxone-adapters / naxone-core 不依赖 tauri，所以无法直接拿到 AppState 调
/// `push_log`。通过此 trait 解耦：naxone-tauri 端实现并通过 setter 注入到
/// ProcessManager 等模块，后台事件就能被用户看到。
///
/// 默认为 no-op：未注入时调用是安全的（fmt 层 tracing 仍会被走到）。
#[async_trait::async_trait]
pub trait LogReporter: Send + Sync {
    async fn report(
        &self,
        level: LogLevel,
        category: &str,
        message: String,
        details: Option<String>,
    );
}
