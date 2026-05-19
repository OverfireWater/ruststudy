use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};

/// Windows release 模式下，子进程不能弹 CMD 窗口。
/// 给所有 Command::new() 加这个 creation flag。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(target_os = "windows")]
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}

#[cfg(target_os = "windows")]
impl NoWindow for Command {
    fn no_window(&mut self) -> &mut Self {
        self.creation_flags(CREATE_NO_WINDOW)
    }
}

use naxone_core::domain::service::{ServiceInstance, ServiceKind, ServiceStatus};
use naxone_core::error::{Result, NaxOneError};
use naxone_core::ports::process::ProcessManager;

struct ProcessInfo {
    pid: u32,
}

/// status 缓存 TTL：低于前端 5s 轮询间隔，用户操作后下一轮就能看到变化
const STATUS_CACHE_TTL: Duration = Duration::from_millis(1000);
/// netstat snapshot 的合并窗口：同一轮 refresh_all 内所有 status 复用一份
const NETSTAT_SNAPSHOT_TTL: Duration = Duration::from_millis(500);

/// 模块级共享的 sysinfo System 实例。
/// 所有进程查询（内存、name、exe 路径）都走它，绕开 tasklist/wmic 的 WMI 调用。
/// 复用同一个 System 省去重建结构的 ~5ms 开销。
static SYS: std::sync::LazyLock<tokio::sync::Mutex<sysinfo::System>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(sysinfo::System::new()));

pub struct WindowsProcessManager {
    processes: Arc<RwLock<HashMap<String, ProcessInfo>>>,
    /// 按 instance.id() 缓存上次 status 结果，降低重复探测成本
    status_cache: Arc<RwLock<HashMap<String, (Instant, ServiceStatus)>>>,
    /// 最近一次 netstat 结果（port → pid），短 TTL 合并同窗口重复调用
    netstat_cache: Arc<Mutex<Option<(Instant, HashMap<u16, u32>)>>>,
    /// 最近一次 tasklist 结果（pid → memory_mb），给所有服务共享
    tasklist_cache: Arc<Mutex<Option<(Instant, HashMap<u32, u64>)>>>,
    /// PHP auto-restart: service_id → true while watchdog should keep it alive
    auto_restart: Arc<RwLock<HashMap<String, bool>>>,
}

impl WindowsProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            status_cache: Arc::new(RwLock::new(HashMap::new())),
            netstat_cache: Arc::new(Mutex::new(None)),
            tasklist_cache: Arc::new(Mutex::new(None)),
            auto_restart: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 一次 `netstat -ano` 把所有 LISTENING 行的 port→pid 全抓出来，供并行 status 复用
    pub async fn snapshot_listening_ports(&self) -> HashMap<u16, u32> {
        // 命中未过期缓存 → 直接返回
        {
            let guard = self.netstat_cache.lock().await;
            if let Some((ts, map)) = guard.as_ref() {
                if ts.elapsed() < NETSTAT_SNAPSHOT_TTL {
                    return map.clone();
                }
            }
        }
        // 重新扫
        let map = netstat_snapshot().await;
        let mut guard = self.netstat_cache.lock().await;
        *guard = Some((Instant::now(), map.clone()));
        map
    }

    /// 失效单个服务的 status 缓存（在 start/stop/restart 后调用，避免返回 stale 数据）
    async fn invalidate_status(&self, instance_id: &str) {
        self.status_cache.write().await.remove(instance_id);
        // netstat 快照也过期掉，下次刷新重新扫
        *self.netstat_cache.lock().await = None;
    }

    /// Spawn a background task that monitors a PHP process and restarts it if it dies.
    async fn spawn_watchdog(&self, instance: &ServiceInstance) {
        let id = instance.id();
        self.auto_restart.write().await.insert(id, true);

        let processes = self.processes.clone();
        let auto_restart = self.auto_restart.clone();
        let status_cache = self.status_cache.clone();
        let netstat_cache = self.netstat_cache.clone();
        let inst = instance.clone();

        tokio::spawn(async move {
            let mut crashes = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;

                // User stopped the service → exit watchdog
                if !auto_restart
                    .read()
                    .await
                    .get(&inst.id())
                    .copied()
                    .unwrap_or(false)
                {
                    break;
                }

                // Still alive → reset crash counter
                if probe_port(inst.port).await {
                    crashes = 0;
                    continue;
                }

                // Process died, try restart
                crashes += 1;
                if crashes > 5 {
                    tracing::warn!(
                        service = %inst.kind.display_name(),
                        port = inst.port,
                        "Auto-restart giving up (5 consecutive crashes)"
                    );
                    auto_restart.write().await.remove(&inst.id());
                    break;
                }

                tracing::warn!(
                    service = %inst.kind.display_name(),
                    port = inst.port,
                    attempt = crashes,
                    "Process died, auto-restarting..."
                );

                if let Ok(mut cmd) = Self::build_start_command(&inst) {
                    cmd.no_window();
                    match cmd
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                    {
                        Ok(mut child) => {
                            let new_pid = child.id().unwrap_or(0);
                            // 显式 drop stdout/stderr 管道避免长期持有 → 底层 OS 句柄能尽早释放
                            drop(child.stdout.take());
                            drop(child.stderr.take());
                            // detach child handle，让 OS 自行清理（PHP-CGI 是长期进程，watchdog 不 wait）
                            drop(child);
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            processes
                                .write()
                                .await
                                .insert(inst.id(), ProcessInfo { pid: new_pid });
                            *netstat_cache.lock().await = None;
                            status_cache.write().await.remove(&inst.id());
                            tracing::info!(
                                pid = new_pid,
                                attempt = crashes,
                                "PHP auto-restarted"
                            );
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "PHP restart spawn failed");
                        }
                    }
                }

                // Backoff: 2s → 4s → 8s → 16s → 32s between attempts
                tokio::time::sleep(Duration::from_secs(2u64.pow(crashes.min(5)))).await;
            }
        });
    }

    fn build_start_command(instance: &ServiceInstance) -> Result<Command> {
        let install_path = &instance.install_path;

        match instance.kind {
            ServiceKind::Nginx => {
                let exe = install_path.join("nginx.exe");
                let conf = instance
                    .config_path
                    .clone()
                    .unwrap_or_else(|| install_path.join("conf").join("nginx.conf"));
                let mut cmd = Command::new(&exe);
                cmd.arg("-c").arg(&conf);
                cmd.current_dir(install_path);
                Ok(cmd)
            }
            ServiceKind::Apache => {
                let exe = install_path.join("bin").join("httpd.exe");
                let conf = instance
                    .config_path
                    .clone()
                    .unwrap_or_else(|| install_path.join("conf").join("httpd.conf"));
                let mut cmd = Command::new(&exe);
                cmd.arg("-f").arg(&conf);
                cmd.current_dir(install_path);
                Ok(cmd)
            }
            ServiceKind::Php => {
                let exe = install_path.join("php-cgi.exe");
                let bind = format!("127.0.0.1:{}", instance.port);
                let mut cmd = Command::new(&exe);
                cmd.arg("-b").arg(&bind);
                if let Some(conf) = &instance.config_path {
                    cmd.arg("-c").arg(conf);
                }
                // PHP 8 + Windows ASLR 下 OPcache 共享内存分配会 fatal，
                // 必须给一个可写的 file_cache 目录走文件回退缓存（fallback=1 是默认值，显式声明）。
                // 不显式设的话 php-cgi 直接退出，nginx fastcgi 全军覆没。
                // 放在 ~/.naxone/opcache/<install_path_hash>/，跟 PHP install 目录解耦，
                // 用户卸载/重装 PHP 不会留旧 opcode 文件污染。
                let cache_dir = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut hasher = DefaultHasher::new();
                    install_path.to_string_lossy().hash(&mut hasher);
                    crate::platform::dirs::naxone_home_dir()
                        .join("opcache")
                        .join(format!("{:x}", hasher.finish()))
                };
                let _ = std::fs::create_dir_all(&cache_dir);
                cmd.arg("-d")
                    .arg(format!("opcache.file_cache={}", cache_dir.display()));
                cmd.arg("-d").arg("opcache.file_cache_fallback=1");
                cmd.current_dir(install_path);
                Ok(cmd)
            }
            ServiceKind::Mysql => {
                let exe = install_path.join("bin").join("mysqld.exe");
                let conf = instance
                    .config_path
                    .clone()
                    .unwrap_or_else(|| install_path.join("my.ini"));
                let mut cmd = Command::new(&exe);
                cmd.arg(format!("--defaults-file={}", conf.display()));
                cmd.current_dir(install_path);
                Ok(cmd)
            }
            ServiceKind::Redis => {
                let exe = install_path.join("redis-server.exe");
                let conf = instance
                    .config_path
                    .clone()
                    .unwrap_or_else(|| install_path.join("redis.windows.conf"));
                let mut cmd = Command::new(&exe);
                cmd.arg(&conf);
                cmd.current_dir(install_path);
                Ok(cmd)
            }
        }
    }

    fn build_stop_command(instance: &ServiceInstance) -> Option<Command> {
        let install_path = &instance.install_path;

        match instance.kind {
            ServiceKind::Nginx => {
                let exe = install_path.join("nginx.exe");
                let mut cmd = Command::new(&exe);
                cmd.arg("-s").arg("quit");
                cmd.current_dir(install_path);
                Some(cmd)
            }
            ServiceKind::Apache => {
                let exe = install_path.join("bin").join("httpd.exe");
                let mut cmd = Command::new(&exe);
                cmd.arg("-k").arg("stop");
                cmd.current_dir(install_path);
                Some(cmd)
            }
            ServiceKind::Redis => {
                let exe = install_path.join("redis-cli.exe");
                let mut cmd = Command::new(&exe);
                cmd.arg("-p")
                    .arg(instance.port.to_string())
                    .arg("shutdown");
                Some(cmd)
            }
            ServiceKind::Mysql => {
                let exe = install_path.join("bin").join("mysqladmin.exe");
                let mut cmd = Command::new(&exe);
                cmd.arg("-u").arg("root").arg("shutdown");
                cmd.current_dir(install_path);
                Some(cmd)
            }
            // PHP-CGI: no graceful shutdown, kill by PID
            ServiceKind::Php => None,
        }
    }

    /// 真正的状态探测（无缓存）。
    /// 若已有 netstat snapshot 则复用，否则只做 port probe + 必要时才 fallback 到 netstat。
    async fn probe_status(&self, instance: &ServiceInstance) -> ServiceStatus {
        // 1) 快速 TCP 探针
        if !probe_port(instance.port).await {
            return ServiceStatus::Stopped;
        }

        // 2) 决定 PID：优先自己启动的；否则从 netstat snapshot 反查
        let pid = {
            let procs = self.processes.read().await;
            procs.get(&instance.id()).map(|info| info.pid)
        };
        let pid = match pid {
            Some(p) => p,
            None => {
                let snapshot = self.snapshot_listening_ports().await;
                let p = snapshot.get(&instance.port).copied().unwrap_or(0);
                if p == 0 {
                    return ServiceStatus::Stopped;
                }
                // 非 PHP 服务（共享端口 80/3306/6379 等）必须校验 PID 真的属于本实例的安装目录，
                // 否则同 kind 多版本会一起报"运行中"。PHP 每个实例独占端口，信端口即可。
                if instance.kind != ServiceKind::Php
                    && !pid_matches_instance(p, instance).await
                {
                    return ServiceStatus::Stopped;
                }
                p
            }
        };

        // 3) 查内存（best-effort；失败就给 None，不影响状态）
        let memory_mb = self.tasklist_memory_map().await.get(&pid).copied();
        ServiceStatus::Running { pid, memory_mb }
    }

    /// 一次 `tasklist /FO CSV /NH` 把所有进程 PID → 内存（MB）抓全，短 TTL 缓存
    pub async fn tasklist_memory_map(&self) -> HashMap<u32, u64> {
        {
            let guard = self.tasklist_cache.lock().await;
            if let Some((ts, map)) = guard.as_ref() {
                if ts.elapsed() < NETSTAT_SNAPSHOT_TTL {
                    return map.clone();
                }
            }
        }
        let map = tasklist_memory_snapshot().await;
        let mut guard = self.tasklist_cache.lock().await;
        *guard = Some((Instant::now(), map.clone()));
        map
    }
}

#[async_trait]
impl ProcessManager for WindowsProcessManager {
    async fn start(&self, instance: &ServiceInstance) -> Result<u32> {
        let t_total = std::time::Instant::now();
        // 端口预检：PHP 可多端口共存不查；其余 kind 端口被外部进程占着的话直接 bail，
        // 避免 redis/nginx/mysqld 因 bind 失败 exit 1，把"端口冲突"伪装成"启动失败 exit code 1"。
        //
        // 优化：先用 TcpListener::bind 试探（毫秒级），端口可用时**完全跳过 netstat**（netstat
        // 子进程在系统连接数多时常常 200-2000ms，是启动慢的主要源头）。
        // bind 失败时再回退到 netstat 找占用方 PID 给详细错误信息（慢路径只在端口真冲突时走）。
        let _t_precheck = std::time::Instant::now();
        if instance.kind != ServiceKind::Php {
            use std::net::{Ipv4Addr, SocketAddr, TcpListener};
            let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, instance.port));
            let bind_ok = TcpListener::bind(addr).map(|l| drop(l)).is_ok();
            if !bind_ok {
                let snap = self.snapshot_listening_ports().await;
                if let Some(&existing_pid) = snap.get(&instance.port) {
                    if existing_pid != 0 && !pid_matches_instance(existing_pid, instance).await {
                        let exe = get_process_exe_path(existing_pid)
                            .await
                            .unwrap_or_else(|| "未知进程".to_string());
                        return Err(NaxOneError::Process(format!(
                            "{} 启动失败：端口 {} 已被外部进程占用 (PID {}, {})。请先在仪表板结束外部进程后重试。",
                            instance.kind.display_name(),
                            instance.port,
                            existing_pid,
                            exe,
                        )));
                    }
                }
                // bind 失败但 netstat 找不到对应 PID（罕见）→ 兜底报通用错
                return Err(NaxOneError::Process(format!(
                    "{} 启动失败：端口 {} 被占用，但定位不到具体进程。请到仪表板「陌生进程」查看。",
                    instance.kind.display_name(),
                    instance.port,
                )));
            }
        }

        tracing::debug!(
            service = instance.kind.display_name(),
            precheck_ms = _t_precheck.elapsed().as_millis() as u64,
            "start: 端口预检完成"
        );

        let t_spawn = std::time::Instant::now();
        let mut cmd = Self::build_start_command(instance)?;
        cmd.no_window();

        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                NaxOneError::Process(format!(
                    "无法启动 {}: {}",
                    instance.kind.display_name(),
                    e
                ))
            })?;

        let pid = child.id().unwrap_or(0);
        tracing::debug!(
            service = instance.kind.display_name(),
            pid,
            spawn_ms = t_spawn.elapsed().as_millis() as u64,
            "start: spawn 完成"
        );

        // For short-lived processes (Nginx/Apache may exit quickly on config error),
        // wait briefly and check if process is still alive。500ms 是固定兜底，配置错误时
        // 进程会在这段时间内退出（nginx 进程通常 < 100ms 自检完）。
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Check if process exited immediately (config error etc.)
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                // 同时读 stdout 和 stderr —— Redis on Windows 把 fatal 打到 stdout
                use tokio::io::AsyncReadExt;
                let mut stderr_buf = String::new();
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr_buf).await;
                }
                let mut stdout_buf = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut stdout_buf).await;
                }
                let combined = format!("{}\n{}", stderr_buf.trim(), stdout_buf.trim());
                let combined = combined.trim();
                let code = status.code().unwrap_or(-1);
                let msg = if combined.is_empty() {
                    // stderr/stdout 都空时，退回去补一次端口探测，给个更有信息量的根因
                    let snap = self.snapshot_listening_ports().await;
                    if let Some(&p) = snap.get(&instance.port) {
                        if p != 0 && !pid_matches_instance(p, instance).await {
                            let exe = get_process_exe_path(p).await.unwrap_or_default();
                            format!(
                                "{} 启动失败 (exit code {})：端口 {} 已被外部进程占用 (PID {}, {})",
                                instance.kind.display_name(),
                                code,
                                instance.port,
                                p,
                                exe
                            )
                        } else {
                            format!("{} 启动失败 (exit code {})", instance.kind.display_name(), code)
                        }
                    } else {
                        format!("{} 启动失败 (exit code {})", instance.kind.display_name(), code)
                    }
                } else {
                    format!(
                        "{} 启动失败: {}",
                        instance.kind.display_name(),
                        combined.lines().last().unwrap_or("unknown error")
                    )
                };
                return Err(NaxOneError::Process(msg));
            }
            _ => {} // Still running or exited successfully
        }

        let mut procs = self.processes.write().await;
        procs.insert(instance.id(), ProcessInfo { pid });
        drop(procs);

        // 主动失效 status 缓存，下次 status() 重新探测得到 Running
        self.invalidate_status(&instance.id()).await;

        // PHP auto-restart watchdog: if php-cgi.exe dies, respawn it automatically
        if instance.kind == ServiceKind::Php {
            self.spawn_watchdog(instance).await;
        }

        tracing::info!(
            service = instance.kind.display_name(),
            pid,
            total_ms = t_total.elapsed().as_millis() as u64,
            "Service started"
        );

        Ok(pid)
    }

    async fn stop(&self, instance: &ServiceInstance) -> Result<()> {
        // Disable auto-restart before stopping (prevents watchdog from immediately respawning)
        self.auto_restart.write().await.remove(&instance.id());

        // Try graceful stop command first (nginx -s quit, httpd -k stop, redis-cli shutdown)
        let mut graceful_ok = false;
        if let Some(mut cmd) = Self::build_stop_command(instance) {
            cmd.no_window();
            let result = cmd
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;

            if let Ok(status) = result {
                if status.success() {
                    graceful_ok = true;
                    tracing::info!(
                        service = instance.kind.display_name(),
                        "Service stopped gracefully"
                    );
                }
            }
        }

        // 优雅命令成功时给进程时间退出，避免接下去 taskkill 撞上"刚退出"的竞态。
        // 端口释放即视为成功，无需再 taskkill。最长等约 3s。
        if graceful_ok {
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                if !probe_port(instance.port).await {
                    break;
                }
            }
        }

        // 端口没释放才走 taskkill 兜底（外部启动 / 优雅命令未支持 / 残留 worker）
        if probe_port(instance.port).await {
            let pid = {
                let procs = self.processes.read().await;
                if let Some(info) = procs.get(&instance.id()) {
                    info.pid
                } else {
                    find_pid_by_port(instance.port).await
                }
            };

            if pid > 0 {
                let kill = Command::new("taskkill")
                    .no_window()
                    .args(["/F", "/PID", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await;
                if let Ok(out) = &kill {
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        let lower = stderr.to_lowercase();
                        // taskkill 报 "could not be found" / "找不到" 表示进程刚退出 → 视为成功
                        let already_gone = lower.contains("not be found")
                            || lower.contains("not found")
                            || stderr.contains("找不到");
                        if !already_gone {
                            let hint = if lower.contains("access is denied")
                                || stderr.contains("拒绝访问")
                            {
                                format!(
                                    "无法结束 {} (PID {}): 权限不足（进程可能以管理员身份启动）",
                                    instance.kind.display_name(),
                                    pid
                                )
                            } else {
                                format!(
                                    "无法结束 {} (PID {}): {}",
                                    instance.kind.display_name(),
                                    pid,
                                    stderr
                                )
                            };
                            return Err(NaxOneError::Process(hint));
                        }
                    }
                }
            }
        }

        // 最后确认端口真的释放了（覆盖 pid=0 但端口仍被占用的情况，例如多个同类残留进程）
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if probe_port(instance.port).await {
            // 兜底：把所有 install_path 下的同 exe 名进程一起清掉
            // （nginx master 死了 worker 还活着、apache 服务进程残留 等场景）
            let killed = kill_workers_under_install_path(instance).await;
            if killed > 0 {
                tracing::info!(
                    service = instance.kind.display_name(),
                    install = %instance.install_path.display(),
                    killed,
                    "兜底清理 install_path 下的残留进程"
                );
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }

            // 再探一次，仍占用才真错
            if probe_port(instance.port).await {
                let still_pid = find_pid_by_port(instance.port).await;
                let extra = if still_pid > 0 {
                    format!("（端口仍被 PID {} 占用）", still_pid)
                } else {
                    String::new()
                };
                return Err(NaxOneError::Process(format!(
                    "{} 已发出停止命令但端口 {} 仍被占用{}",
                    instance.kind.display_name(),
                    instance.port,
                    extra
                )));
            }
        }

        let mut procs = self.processes.write().await;
        procs.remove(&instance.id());
        drop(procs);

        // 主动失效 status 缓存
        self.invalidate_status(&instance.id()).await;

        tracing::info!(
            service = instance.kind.display_name(),
            "Service stopped"
        );
        Ok(())
    }

    async fn restart(&self, instance: &ServiceInstance) -> Result<u32> {
        self.stop(instance).await?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let pid = self.start(instance).await?;
        // start/stop 已各自 invalidate 过，这里确保 restart 结束后缓存干净
        self.invalidate_status(&instance.id()).await;
        Ok(pid)
    }

    async fn status(&self, instance: &ServiceInstance) -> Result<ServiceStatus> {
        // 命中有效缓存 → 直接返回，省掉 netstat/tasklist
        {
            let cache = self.status_cache.read().await;
            if let Some((ts, st)) = cache.get(&instance.id()) {
                if ts.elapsed() < STATUS_CACHE_TTL {
                    return Ok(st.clone());
                }
            }
        }

        let status = self.probe_status(instance).await;

        // 写回缓存
        self.status_cache
            .write()
            .await
            .insert(instance.id(), (Instant::now(), status.clone()));

        Ok(status)
    }

    async fn reload(&self, instance: &ServiceInstance) -> Result<()> {
        match instance.kind {
            ServiceKind::Nginx => {
                // `nginx -s reload` 依赖 logs/nginx.pid 找到 master 进程发信号。
                // pid 文件不存在 / 为空 / 0 字节时（master 异常死亡 / 多进程踩踏 / 多次重启
                // 互相覆盖），nginx 会报 `invalid PID number ""`。自愈方案：先 stop（杀任何
                // 残留 nginx 进程，释放端口），再 start（写新的 pid 文件），然后正常 reload。
                let pid_file = instance.install_path.join("logs").join("nginx.pid");
                let pid_ok = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .filter(|&p| p > 0)
                    .is_some();
                if !pid_ok {
                    tracing::warn!(
                        pid_file = %pid_file.display(),
                        "Nginx pid 文件无效，自愈：stop + start 后再 reload"
                    );
                    // 自愈：杀所有 nginx 残留 → 启动一次干净的
                    let _ = self.stop(instance).await; // 即便 stop 失败也继续
                    self.start(instance).await.map_err(|e| {
                        NaxOneError::Process(format!(
                            "Nginx pid 文件损坏，自动重启失败: {e}"
                        ))
                    })?;
                    // start 成功后立即返回（不再 reload）—— 启动本身就加载了最新 conf
                    tracing::info!("Nginx pid 文件无效，已通过自愈完成 reload 等效操作");
                    return Ok(());
                }

                // First run -t to test config
                let exe = instance.install_path.join("nginx.exe");
                let test = Command::new(&exe)
                    .no_window()
                    .arg("-t")
                    .current_dir(&instance.install_path)
                    .output()
                    .await
                    .map_err(|e| NaxOneError::Process(format!("Nginx 测试失败: {e}")))?;
                if !test.status.success() {
                    let stderr = String::from_utf8_lossy(&test.stderr);
                    let msg = stderr.lines().find(|l| l.contains("emerg") || l.contains("error"))
                        .unwrap_or_else(|| stderr.lines().last().unwrap_or("unknown")).to_string();
                    return Err(NaxOneError::Process(format!("Nginx 配置错误: {}", msg)));
                }
                // Then reload
                let reload_out = Command::new(&exe)
                    .no_window()
                    .arg("-s").arg("reload")
                    .current_dir(&instance.install_path)
                    .output()
                    .await
                    .map_err(|e| NaxOneError::Process(format!("Nginx reload 失败: {e}")))?;
                if !reload_out.status.success() {
                    let stderr = String::from_utf8_lossy(&reload_out.stderr);
                    return Err(NaxOneError::Process(format!("Nginx reload 失败: {}", stderr.trim())));
                }
            }
            ServiceKind::Apache => {
                let exe = instance.install_path.join("bin").join("httpd.exe");
                let out = Command::new(&exe)
                    .no_window()
                    .arg("-k").arg("graceful")
                    .current_dir(&instance.install_path)
                    .output()
                    .await
                    .map_err(|e| NaxOneError::Process(format!("Apache reload 失败: {e}")))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(NaxOneError::Process(format!("Apache reload 失败: {}", stderr.trim())));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Quick TCP port probe with 300ms timeout
async fn probe_port(port: u16) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(300),
        tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Find the PID of the process listening on a given port using `netstat`
async fn find_pid_by_port(port: u16) -> u32 {
    netstat_snapshot().await.get(&port).copied().unwrap_or(0)
}

/// 一次 netstat 调用解析出所有 LISTENING 端口 → PID 映射
pub async fn netstat_snapshot() -> HashMap<u16, u32> {
    let mut map = HashMap::new();
    let output = Command::new("netstat")
        .no_window()
        .args(["-ano", "-p", "TCP"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    let Ok(output) = output else { return map };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let line = line.trim();
        if !line.contains("LISTENING") {
            continue;
        }
        // 格式: "  TCP    0.0.0.0:80    0.0.0.0:0    LISTENING    1234"
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        let local = parts[1];
        let Some(colon) = local.rfind(':') else { continue };
        let Ok(port) = local[colon + 1..].parse::<u16>() else { continue };
        let Ok(pid) = parts[parts.len() - 1].parse::<u32>() else { continue };
        // 同一端口可能出现多条（IPv4/IPv6），保留非零 pid 的第一条
        map.entry(port).or_insert(pid);
    }
    map
}

/// 全进程 PID → 工作集内存（MB）。走 sysinfo（NtQuerySystemInformation），不调 tasklist。
pub async fn tasklist_memory_snapshot() -> HashMap<u32, u64> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
    let mut sys = SYS.lock().await;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new().with_memory(),
    );
    sys.processes()
        .iter()
        .map(|(pid, p)| (pid.as_u32(), p.memory() / 1024 / 1024)) // bytes → MB
        .collect()
}

/// Parse `"12,345 K"` 或 `"1,234,567 K"` → MB（四舍五入到整数）
/// 保留以兼容现有测试覆盖（之前 tasklist CSV 解析的单元测试）。
#[allow(dead_code)]
fn parse_tasklist_memory(raw: &str) -> Option<u64> {
    let trimmed = raw.trim().trim_end_matches('K').trim_end_matches(' ').trim();
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    let kb: u64 = digits.parse().ok()?;
    // KB → MB，向上取整（0 内存的进程显示 0 MB）
    Some((kb + 512) / 1024)
}

/// 简易 CSV 行 parser：处理 `"a","b"` 格式，字段里不含逗号的情况
#[allow(dead_code)]
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

/// Get the executable name of a process by PID using `tasklist`
/// 取进程 exe 名（小写，含扩展名，如 `"nginx.exe"`）。走 sysinfo，不调 tasklist。
pub async fn get_process_name(pid: u32) -> Option<String> {
    if pid == 0 { return None; }
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    let target = Pid::from_u32(pid);
    let mut sys = SYS.lock().await;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::new(),
    );
    sys.process(target).map(|p| p.name().to_string_lossy().to_lowercase())
}

/// 取进程的真实 exe 完整路径。走 sysinfo（之前用 wmic 命令，会触发 WmiPrvSE 高 CPU）。
/// 用来识别端口占用者属于哪个安装目录（外部进程检测）。
pub async fn get_process_exe_path(pid: u32) -> Option<String> {
    if pid == 0 { return None; }
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    let target = Pid::from_u32(pid);
    let mut sys = SYS.lock().await;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::new().with_exe(UpdateKind::OnlyIfNotSet),
    );
    sys.process(target)
        .and_then(|p| p.exe())
        .map(|e| e.to_string_lossy().to_string())
}

/// Check if a PID belongs to a specific service kind
async fn pid_matches_service(pid: u32, kind: ServiceKind) -> bool {
    if pid == 0 {
        return false;
    }
    let Some(name) = get_process_name(pid).await else {
        return false;
    };
    match kind {
        ServiceKind::Nginx => name.contains("nginx"),
        ServiceKind::Apache => name.contains("httpd"),
        ServiceKind::Php => name.contains("php"),
        ServiceKind::Mysql => name.contains("mysqld"),
        ServiceKind::Redis => name.contains("redis"),
    }
}

/// 比 pid_matches_service 严格一档：除了进程名匹配，还要求 exe 路径在 instance.install_path 下。
/// 解决"同 kind 多版本共享端口"导致的状态错位（如装了两个 Redis，跑着 3.0.504 时 5.0.14.1 也报 Running）。
///
/// 取不到 exe 路径时（典型：dev 模式普通权限读 admin 进程被拒，wmic 返回空）的处理：
/// **降级为 pid_matches_service**——只信进程名 + kind 匹配。
/// 这是在「保守判定 Stopped」（用户痛点：明明在跑显示已停止）和「多版本误报 Running」之间的取舍：
/// admin 进程只在用户机器上存在 PHPStudy 等场景，多版本误报概率低；而 admin 拒访问是高频的。
/// 多版本场景由调用方在 install_path 已知时另行去重。
async fn pid_matches_instance(pid: u32, instance: &ServiceInstance) -> bool {
    if pid == 0 {
        return false;
    }
    if !pid_matches_service(pid, instance.kind).await {
        return false;
    }
    let Some(exe_path) = get_process_exe_path(pid).await else {
        // 取不到 exe path：降级信任进程名 + kind（已 pid_matches_service 过）
        tracing::debug!(pid, kind = ?instance.kind, "pid_matches_instance: exe path 不可读，降级为名字匹配");
        return true;
    };
    let exe = exe_path.replace('/', "\\").to_lowercase();
    let install = instance
        .install_path
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
    if install.is_empty() {
        return false;
    }
    let prefix = if install.ends_with('\\') {
        install.clone()
    } else {
        format!("{}\\", install)
    };
    exe.starts_with(&prefix)
}

/// 兜底清理：扫所有 exe 名匹配 kind 的进程，过滤出 exe 路径在 instance.install_path 下的，
/// 全部 taskkill。处理 nginx master 死了 worker 还活着的残留情况。
/// 返回成功 kill 的进程数。
async fn kill_workers_under_install_path(instance: &ServiceInstance) -> usize {
    let exe_name = match instance.kind {
        ServiceKind::Nginx => "nginx.exe",
        ServiceKind::Apache => "httpd.exe",
        ServiceKind::Mysql => "mysqld.exe",
        ServiceKind::Redis => "redis-server.exe",
        ServiceKind::Php => "php-cgi.exe",
    };

    let install = instance
        .install_path
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
    if install.is_empty() {
        return 0;
    }
    let install_prefix = if install.ends_with('\\') {
        install.clone()
    } else {
        format!("{}\\", install)
    };

    // 1) tasklist 找所有同 exe 名的 PID
    let out = Command::new("tasklist")
        .no_window()
        .args([
            "/FI",
            &format!("IMAGENAME eq {}", exe_name),
            "/FO",
            "CSV",
            "/NH",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    let Ok(out) = out else {
        return 0;
    };
    let text = String::from_utf8_lossy(&out.stdout);

    let mut killed = 0usize;
    for line in text.lines() {
        // CSV: "exe","pid","Console","1","memory"
        let parts: Vec<&str> = line.split(',').map(|p| p.trim_matches('"').trim()).collect();
        if parts.len() < 2 {
            continue;
        }
        let Ok(pid) = parts[1].parse::<u32>() else {
            continue;
        };
        // 校验 PID exe 路径在本 install 下
        let Some(exe_path) = get_process_exe_path(pid).await else {
            continue;
        };
        let exe_lower = exe_path.replace('/', "\\").to_lowercase();
        if !exe_lower.starts_with(&install_prefix) {
            continue;
        }
        let kill = Command::new("taskkill")
            .no_window()
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .await;
        if kill.map(|o| o.status.success()).unwrap_or(false) {
            killed += 1;
        }
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasklist_memory_variants() {
        assert_eq!(parse_tasklist_memory("12,345 K"), Some(12)); // 12345 KB → 12 MB
        assert_eq!(parse_tasklist_memory("1,234,567 K"), Some(1206)); // 1.2 GB
        assert_eq!(parse_tasklist_memory("512 K"), Some(1)); // <1MB 向上凑整
        assert_eq!(parse_tasklist_memory("0 K"), Some(0));
        assert_eq!(parse_tasklist_memory("N/A"), None);
        assert_eq!(parse_tasklist_memory(""), None);
    }

    #[test]
    fn parse_csv_line_handles_quotes() {
        let line = r#""nginx.exe","1234","Console","1","12,345 K""#;
        let fields = parse_csv_line(line);
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[0], "nginx.exe");
        assert_eq!(fields[1], "1234");
        assert_eq!(fields[4], "12,345 K");
    }
}
