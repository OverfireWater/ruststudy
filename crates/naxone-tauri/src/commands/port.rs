//! 端口诊断：查询某端口的监听进程、终止进程。
//!
//! Windows 上调 `netstat -ano` 拿 LISTENING + PID，再用 sysinfo 拿进程名/路径
//! （已有 `naxone_adapters::process::windows::{get_process_name, get_process_exe_path}` 走 sysinfo，
//! 不再触发 WmiPrvSE 高 CPU）。

use std::os::windows::process::CommandExt;
use std::process::Command;

use serde::Serialize;
use tauri::State;

use naxone_adapters::process::windows::{get_process_exe_path, get_process_name};
use naxone_core::domain::log::LogLevel;

use crate::commands::logger::push_log;
use crate::state::AppState;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize)]
pub struct PortListener {
    pub pid: u32,
    pub process_name: Option<String>,
    pub exe_path: Option<String>,
    /// 识别得出的归属来源，如 "PHPStudy"。识别不出时为 None，前端不显示。
    pub source: Option<String>,
    pub local_address: String,
}

#[derive(Serialize)]
pub struct PortDiagnosis {
    pub port: u16,
    pub in_use: bool,
    pub listeners: Vec<PortListener>,
}

#[tauri::command]
pub async fn diagnose_port(port: u16) -> Result<PortDiagnosis, String> {
    if port == 0 {
        return Err("端口号必须大于 0".into());
    }

    let output = Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("调用 netstat 失败: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // 解析行：  TCP    0.0.0.0:80     0.0.0.0:0     LISTENING       7052
    // IPv6 :   TCP    [::]:80        [::]:0        LISTENING       7052
    let mut seen_pids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut raw: Vec<(u32, String)> = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 || cols[0] != "TCP" {
            continue;
        }
        if !cols.iter().any(|c| *c == "LISTENING") {
            continue;
        }
        let local_addr = cols[1];
        let listening_port: u16 = match local_addr.rsplit(':').next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if listening_port != port {
            continue;
        }
        let pid: u32 = match cols.last().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if seen_pids.insert(pid) {
            raw.push((pid, local_addr.to_string()));
        }
    }

    let mut listeners = Vec::with_capacity(raw.len());
    for (pid, local_address) in raw {
        let process_name = get_process_name(pid).await;
        let exe_path = get_process_exe_path(pid).await;
        let source = identify_source(exe_path.as_deref());
        listeners.push(PortListener {
            pid,
            process_name,
            exe_path,
            source,
            local_address,
        });
    }

    Ok(PortDiagnosis {
        port,
        in_use: !listeners.is_empty(),
        listeners,
    })
}

fn identify_source(exe_path: Option<&str>) -> Option<String> {
    let path = exe_path?.to_ascii_lowercase();
    if path.contains("phpstudy") {
        return Some("PHPStudy".into());
    }
    None
}

/// 拒绝杀的系统关键进程（小写匹配）。杀这些会蓝屏 / 注销 / 杀软告警。
/// PID 0 / 4 / 8 是 System Idle / System / Secure System，本就杀不掉但显式拒绝避免误操作。
const SYSTEM_CRITICAL_PROCESSES: &[&str] = &[
    "system",
    "registry",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "lsaiso.exe",
    "fontdrvhost.exe",
    "dwm.exe",
];

#[tauri::command]
pub async fn kill_process_by_pid(
    pid: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if pid == 0 || pid == 4 || pid == 8 {
        push_log(&state, LogLevel::Error, "port", "结束进程失败：拒绝杀系统进程", None, None).await;
        return Err("拒绝结束系统关键进程".into());
    }
    // 拿一下进程名加进日志，方便用户回溯
    let proc_name = get_process_name(pid).await.unwrap_or_else(|| "(未知)".to_string());
    // 系统关键进程白名单拦截。即使任务管理器以管理员身份也不允许从 NaxOne 杀这些。
    let proc_lower = proc_name.to_ascii_lowercase();
    if SYSTEM_CRITICAL_PROCESSES.iter().any(|p| *p == proc_lower) {
        let msg = format!("拒绝结束系统关键进程 {} (PID {})", proc_name, pid);
        push_log(&state, LogLevel::Error, "port", msg.clone(), None, None).await;
        return Err(msg);
    }

    let output = Command::new("taskkill")
        .args(["/F", "/PID"])
        .arg(pid.to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("调用 taskkill 失败: {}", e))?;

    if output.status.success() {
        push_log(
            &state,
            LogLevel::Success,
            "port",
            format!("结束进程 {} (PID {})", proc_name, pid),
            None,
            None,
        )
        .await;
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        stdout.trim().to_string()
    };
    push_log(
        &state,
        LogLevel::Error,
        "port",
        format!("结束进程 {} (PID {}) 失败", proc_name, pid),
        Some(detail.clone()),
        None,
    )
    .await;
    Err(format!("结束进程失败: {}", detail))
}
