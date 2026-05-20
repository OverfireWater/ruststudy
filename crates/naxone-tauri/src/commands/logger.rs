use crate::state::{resolve_log_dir, AppState};
use naxone_core::domain::log::{LogEntry, LogLevel, LogStats};
use naxone_core::ports::log_reporter::LogReporter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::State;

const MAX_BUFFER: usize = 1000;

/// Push a log entry to memory buffer + file (non-blocking)
pub async fn push_log(
    state: &AppState,
    level: LogLevel,
    category: &str,
    message: impl Into<String>,
    details: Option<String>,
    context: Option<serde_json::Value>,
) {
    let id = state.log_id_counter.fetch_add(1, Ordering::SeqCst) + 1;
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let entry = LogEntry {
        id,
        timestamp: ts,
        level,
        category: category.to_string(),
        message: message.into(),
        details,
        context,
    };

    // Push to ring buffer
    {
        let mut logs = state.logs.write().await;
        if logs.len() >= MAX_BUFFER {
            logs.pop_front();
        }
        logs.push_back(entry.clone());
    }

    // Send to file writer (non-blocking)
    let tx_guard = state.log_writer_tx.lock().await;
    if let Some(tx) = tx_guard.as_ref() {
        let _ = tx.send(entry);
    }
}

/// 一劳永逸的命令日志包装：在 return 前调一行，自动记录成功/失败。
///
/// 用法：
/// ```ignore
/// let result = do_something().await;
/// logged(&state, "tool", format!("切换 Node 到 v{}", ver), result).await
/// ```
pub async fn logged<T>(
    state: &AppState,
    category: &str,
    action: impl Into<String>,
    result: Result<T, String>,
) -> Result<T, String> {
    let action = action.into();
    match &result {
        Ok(_) => push_log(state, LogLevel::Info, category, &action, None, None).await,
        Err(e) => {
            push_log(
                state,
                LogLevel::Error,
                category,
                format!("{} 失败", action),
                Some(e.clone()),
                None,
            )
            .await
        }
    }
    result
}

/// 把 AppState 包成 LogReporter，供 naxone-adapters 层（watchdog 等）
/// 通过依赖反转往用户活动日志推事件。
pub struct AppStateLogReporter {
    state: Arc<AppState>,
}

impl AppStateLogReporter {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl LogReporter for AppStateLogReporter {
    async fn report(
        &self,
        level: LogLevel,
        category: &str,
        message: String,
        details: Option<String>,
    ) {
        push_log(&self.state, level, category, message, details, None).await;
    }
}

/// Spawn background task to write log entries to daily files
pub fn spawn_log_writer(state: std::sync::Arc<AppState>) -> tokio::sync::mpsc::UnboundedSender<LogEntry> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LogEntry>();
    let config = state.config.clone();

    tauri::async_runtime::spawn(async move {
        while let Some(entry) = rx.recv().await {
            let config_snap = config.read().await.clone();
            let dir = resolve_log_dir(&config_snap);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!("无法创建日志目录 {}: {}", dir.display(), e);
                continue;
            }
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let file_path = dir.join(format!("app-{}.log", today));
            let line = match serde_json::to_string(&entry) {
                Ok(s) => s,
                Err(_) => continue,
            };
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file_path) {
                let _ = writeln!(f, "{}", line);
            }
        }
    });

    tx
}

/// Clean up log files older than retention_days
pub fn cleanup_old_logs(log_dir: &std::path::Path, retention_days: u32) {
    let Ok(entries) = std::fs::read_dir(log_dir) else { return; };
    let cutoff = chrono::Local::now() - chrono::Duration::days(retention_days as i64);
    let cutoff_str = cutoff.format("%Y-%m-%d").to_string();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue; };
        if !name.starts_with("app-") || !name.ends_with(".log") { continue; }
        let date_part = &name[4..name.len() - 4]; // strip "app-" and ".log"
        if date_part.len() == 10 && date_part < cutoff_str.as_str() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ==================== Commands ====================

#[tauri::command]
pub async fn get_logs(
    limit: Option<usize>,
    level: Option<String>,
    category: Option<String>,
    since_id: Option<u64>,
    state: State<'_, AppState>,
) -> Result<Vec<LogEntry>, String> {
    let logs = state.logs.read().await;
    let limit = limit.unwrap_or(100);
    let min_level = level.as_deref().and_then(LogLevel::from_str);

    let filtered: Vec<LogEntry> = logs
        .iter()
        .rev()
        .filter(|e| {
            if let Some(sid) = since_id {
                if e.id <= sid { return false; }
            }
            if let Some(ml) = min_level {
                if e.level.priority() < ml.priority() { return false; }
            }
            if let Some(cat) = &category {
                if !cat.is_empty() && &e.category != cat { return false; }
            }
            true
        })
        .take(limit)
        .cloned()
        .collect();

    Ok(filtered)
}

#[tauri::command]
pub async fn clear_logs(state: State<'_, AppState>) -> Result<(), String> {
    state.logs.write().await.clear();
    Ok(())
}

#[tauri::command]
pub async fn get_log_stats(state: State<'_, AppState>) -> Result<LogStats, String> {
    let logs = state.logs.read().await;
    let mut by_level: std::collections::HashMap<String, usize> = Default::default();
    let mut by_category: std::collections::HashMap<String, usize> = Default::default();
    for e in logs.iter() {
        *by_level.entry(e.level.as_str().to_string()).or_insert(0) += 1;
        *by_category.entry(e.category.clone()).or_insert(0) += 1;
    }
    Ok(LogStats {
        total: logs.len(),
        by_level,
        by_category,
        oldest: logs.front().map(|e| e.timestamp.clone()),
        newest: logs.back().map(|e| e.timestamp.clone()),
    })
}

#[tauri::command]
pub async fn open_log_dir(state: State<'_, AppState>) -> Result<(), String> {
    let config = state.config.read().await;
    let dir = resolve_log_dir(&config);
    let _ = std::fs::create_dir_all(&dir);
    std::process::Command::new("explorer.exe")
        .arg(dir.to_string_lossy().replace('/', "\\"))
        .spawn()
        .map_err(|e| format!("无法打开目录: {}", e))?;
    Ok(())
}
