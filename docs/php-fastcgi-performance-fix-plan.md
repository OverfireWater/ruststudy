# PHP FastCGI 性能与并发修复计划

> 状态：核心并发与进程池生命周期已实施；OPcache/Xdebug 可观测性待后续
> 创建日期：2026-07-23
> 适用范围：NaxOne 管理的 Windows Nginx + PHP-CGI
> 明确不包含：MySQL、远程数据库、业务接口 SQL 优化

## 1. 问题结论

当前性能问题不是 Nginx 连接能力不足，而是每个 PHP 版本实际只有一个
`php-cgi.exe` worker：

```text
Nginx 多 worker
    ↓
127.0.0.1:9000+
    ↓
单个 php-cgi.exe
    ↓
一次只能执行一个 PHP 请求
```

只要某个请求发生慢 IO、远程调用或长时间计算，同一 PHP 版本上的其他请求都会
在 FastCGI 监听队列中等待，形成队头阻塞。

NaxOne 当前启动命令只包含：

```text
php-cgi.exe -b 127.0.0.1:<port> -c <php.ini>
```

没有设置：

```text
PHP_FCGI_CHILDREN
PHP_FCGI_MAX_REQUESTS
```

因此 PHP FastCGI 将并发能力报告为 1。

项目虽然已经定义了 `PhpInstanceConfig.workers`，但它目前没有接入服务扫描、
`ServiceInstance`、启动命令、设置 DTO 或前端，所以配置始终不生效。

此外，当前被测 PHP 7.4.3：

- 未加载 Zend OPcache；
- 加载了 Xdebug 2.9.4；
- 开启了 `xdebug.collect_params` 和 `xdebug.collect_return`；
- NaxOne 传入的 `opcache.file_cache` 参数因扩展未加载而无效。

这些问题抬高了单请求基线，但本计划不处理 MySQL 或具体业务接口内部耗时。

## 2. 修复目标

### 2.1 必须达到

1. 每个 PHP 版本支持可配置的 FastCGI worker 池。
2. 默认启用 PHPStudy 兼容模式：每个活跃 PHP 版本固定 16 workers；可关闭后使用全局预算模式。
3. 默认 `PHP_FCGI_MAX_REQUESTS=1000`，可配置范围为 100–10000。
4. 多 worker 模式下启动、停止、重启、watchdog 和孤儿进程接管均正确。
5. 一个慢请求不能阻塞同一 PHP 版本的全部请求。
6. 能检测 OPcache 是否真实加载，而不是只写入无效的 `-d` 参数。
7. 能检测 Xdebug，并明确提示其性能影响。
8. 所有启停、配置变更和自动修复继续遵循活动日志规范。

### 2.2 不作为本次目标

- 不修改应用数据库地址。
- 不优化业务 SQL。
- 不解决远程 MySQL 的网络波动或连接超时。
- 不引入 PHP-FPM；PHP-FPM 不是 Windows 官方运行方案。
- 不修改 Apache `mod_fcgid` 的进程模型。
- 不以升级业务项目 PHP 版本作为本次修复前提。

## 3. 总体设计

### 3.1 FastCGI worker 池

NaxOne 启动 PHP-CGI 时增加进程环境变量：

```text
PHP_FCGI_CHILDREN=<workers>
PHP_FCGI_MAX_REQUESTS=<max_requests>
```

Windows PHP-CGI 会保留一个父进程，并创建指定数量的子 worker，共享同一个
FastCGI 监听 socket。

默认策略：

```text
phpstudy_compatible_workers = true
workers_per_active_version = 16
max_requests = 1000
```

只把启用站点引用的 PHP 版本视为活跃版本。每个活跃版本先分配 1 个
池；未被站点引用的已安装 PHP 不随 Nginx 启动。

例如 3 个活跃 PHP 版本：

```text
16 + 16 + 16 = 48 workers
```

Windows PHP-CGI 还会为每个活跃版本保留 1 个父进程，所以此例任务管理器中
会看到 51 个 `php-cgi.exe`。若 5 个已安装版本中只有 2 个被启用站点使用，
则只启动这 2 个版本，共 34 个进程。

关闭 PHPStudy 兼容模式后，恢复全局预算策略：每个活跃版本先分配 1 个
worker，剩余名额按启用站点数量加权分配。

边界：

```text
total_worker_budget: 1..=64
max_workers_per_version: 1..=16
max_requests: 100..=10000
```

不继续使用当前代码中的默认 `workers=16`，也不对每个已安装版本固定分配 4。
全局预算防止多版本场景快速产生几十个 PHP 进程。

### 3.2 配置模型

增加全局运行池配置：

```rust
pub struct PhpRuntimeConfig {
    pub phpstudy_compatible_workers: bool,
    pub total_worker_budget: u16,
    pub max_workers_per_version: u16,
    pub max_requests: u32,
}
```

`AppConfig.php_instances` 保留向后兼容，并把旧默认值从 16 降为 4；自动分配由
`PhpRuntimeConfig` 统一控制。解析后的运行参数附加到活跃 PHP 的
`ServiceInstance`，未使用版本保持 `None`。

为了避免 `ProcessManager` 直接依赖全局配置，给 `ServiceInstance` 增加仅 PHP 使用的
运行参数，或增加一个可序列化的 `PhpRuntimeOptions`。不让 Windows adapter
自行读取 `naxone.toml`。

### 3.3 进程身份模型

多 worker 后不能再假定：

```text
监听端口 PID == NaxOne 启动得到的父 PID
```

需要同时维护：

- `root_pid`：NaxOne 启动的 PHP-CGI 父进程；
- `listener_pid`：`netstat` 返回的监听 socket 所属 PID；
- `worker_pids`：同一安装目录、同一父进程树下的 PHP-CGI 子进程。

健康状态的判定改为：

```text
父进程存在
AND 端口可连接
AND 监听 PID 属于该 PHP 安装目录/进程树
```

不能再用 `listener_pid == root_pid` 作为必要条件。

### 3.4 watchdog

watchdog 只负责监控 PHP-CGI 父进程和端口：

- 单个子 worker 退出：交给 PHP-CGI 父进程自动补充，NaxOne 不启动第二个池。
- 父进程退出且端口释放：NaxOne 重启整个池。
- 父进程仍在但端口无法连接：结束整个父进程树，再重启一个池。
- 监听 PID 是合法子进程时，不得误判为陌生进程。
- 每个 PHP 实例仍只允许一个 watchdog generation。

### 3.5 停止与重启

停止 PHP 服务必须结束整个进程树。

当前 `taskkill /F /PID <pid>` 需要改为：

```text
taskkill /F /T /PID <root_pid>
```

停止后的验收条件：

1. 父进程消失；
2. 所有该进程树的 PHP-CGI 子进程消失；
3. FastCGI 端口释放；
4. 不触发 watchdog 重新拉起；
5. 不误杀同安装目录下由用户独立启动、但不属于该父进程树的 PHP 命令。

安装目录兜底清理只能作为端口仍未释放时的最后手段，并继续记录详细活动日志。

### 3.6 孤儿进程接管

应用重启后若发现 FastCGI 池仍在运行：

1. 通过监听 PID获取其 `php-cgi.exe` 路径；
2. 沿 `ParentProcessId` 向上查找同安装目录的 PHP-CGI 根进程；
3. 将根进程记为 `root_pid`；
4. 校验根进程命令行中的 `-b` 端口与实例一致；
5. 枚举子进程并确认 worker 池完整；
6. 接管根进程并启动 watchdog。

接管后不得因为监听 PID 是子进程而立即重启或重复绑定端口。

## 4. OPcache 与 Xdebug

### 4.1 OPcache 检测

启动 PHP 池前执行轻量检测：

```text
php.exe -c <php.ini> -r "echo extension_loaded('Zend OPcache') ? '1' : '0';"
```

或通过模块列表检测 `Zend OPcache`。

不能把以下参数存在当作 OPcache 已启用：

```text
-d opcache.file_cache=...
-d opcache.file_cache_fallback=1
```

### 4.2 OPcache 修复

新增 `ensure_php_opcache`：

1. 检查 `ext/php_opcache.dll` 是否存在；
2. 检查 `php.ini` 是否已存在有效的 OPcache `zend_extension`；
3. 修改前创建 `.bak`；
4. 启用 OPcache 扩展；
5. 保留现有 Windows file cache 路径隔离逻辑；
6. 启动后再次通过 PHP 命令确认扩展真实加载；
7. 失败时不阻止 PHP 启动，但记录 Warning 活动日志和修复建议。

PHPStudy 来源的 PHP 不应静默覆盖用户的复杂自定义配置。首次自动修复只处理明确的
“DLL 存在、配置行被注释或缺失”场景，其余情况提示用户确认。

### 4.3 Xdebug

本次不直接删除或注释用户的 Xdebug 配置。

先实现：

- 检测 Xdebug 是否加载；
- 在 PHP 服务配置页显示“Web 请求正在加载 Xdebug”警告；
- 展示 `collect_params`、`collect_return`、trace、profiler 等高开销状态；
- 提供明确的“高性能 Web 模式”开关设计，但不在第一阶段自动改用户 php.ini。

后续高性能模式应使用 NaxOne 管理的 FastCGI 专用 ini，而不是破坏 CLI/IDE 调试所用
的原始 php.ini。

## 5. 具体实施步骤

### 阶段 A：配置贯通

涉及文件：

- `crates/naxone-core/src/config.rs`
- `crates/naxone-core/src/domain/service.rs`
- `crates/naxone-tauri/src/state.rs`
- `crates/naxone-tauri/src/commands/settings.rs`
- `crates/naxone-tauri/src/commands/php.rs`

任务：

1. 默认启用 PHPStudy 兼容模式，每活跃版本 16 workers。
2. 保留全局预算 8、单版本上限 4 作为可切换的节省资源模式。
3. 增加 `max_requests` 及向后兼容默认值。
4. 根据启用 vhost 动态计算活跃 PHP 和 worker 分配。
5. 保存配置时校验范围，拒绝 0 或异常大值。
6. 对旧 `naxone.toml` 做无损迁移。

验证：

- 旧配置可以加载；
- 空配置启用 PHPStudy 兼容模式，并得到最大请求数 1000；
- 显式配置保持不变；
- 非法配置被夹取或返回明确错误；
- TOML round-trip 测试通过。

### 阶段 B：启动 worker 池

涉及文件：

- `crates/naxone-adapters/src/process/windows.rs`

任务：

1. PHP 启动命令设置 `PHP_FCGI_CHILDREN`。
2. 设置 `PHP_FCGI_MAX_REQUESTS`。
3. 启动日志记录实例、端口、worker 数和父 PID。
4. 启动后确认端口可连接。
5. 枚举进程树并记录实际子 worker 数。
6. 实际 worker 数不足时记录 Warning，但避免无限重启。

验证：

- workers=1 时保持兼容；
- workers=4 时观察到一个父进程和四个子 worker；
- 四个并发请求可以由不同 PID 同时处理；
- 配置值不通过 shell 拼接，不产生命令注入面。

### 阶段 C：生命周期修复

涉及文件：

- `crates/naxone-adapters/src/process/windows.rs`
- `crates/naxone-tauri/src/main.rs`

任务：

1. `ProcessInfo` 保存根 PID，而不是任意监听 PID。
2. watchdog 接受合法子进程作为监听者。
3. 单 worker 崩溃时不重复启动整个池。
4. 父进程崩溃时只拉起一个新池。
5. stop/restart 使用进程树语义。
6. `adopt_if_running` 向上解析根 PID。
7. 活动日志增加 worker 池状态和恢复原因。

验证：

- 手动结束一个子 worker，父进程自动补齐；
- 手动结束父进程，watchdog 只重启一次；
- 连续快速 stop/start 不产生两个 worker 池；
- 退出 NaxOne 再打开可正确接管；
- 停止服务后无残留 PHP-CGI；
- 多个 PHP 版本互不误杀。

### 阶段 D：OPcache 检测与修复

涉及文件：

- `crates/naxone-adapters/src/package/post_install.rs`
- `crates/naxone-tauri/src/main.rs`
- `crates/naxone-tauri/src/commands/php.rs`

任务：

1. 增加 OPcache DLL、ini 和运行时三层检测。
2. 新安装 PHP 默认启用 OPcache。
3. 已安装 PHP 采用安全迁移和 `.bak`。
4. 保留 PHP 8 Windows file cache fallback。
5. 检测 Xdebug 并上报，不自动删除。

验证：

- `php -v` 或模块检测能看到 Zend OPcache；
- FastCGI 请求第二次执行明显命中缓存；
- PHP 7.4、8.1、8.4、8.5 至少各验证一个 NTS 包；
- 配置错误时可从 `.bak` 恢复；
- 无 OPcache DLL 的非标准 PHP 仍能启动。

### 阶段 E：前端配置

涉及文件：

- `crates/naxone-tauri/frontend/src/views/ServiceConfig.vue`
- `crates/naxone-tauri/src/commands/php.rs`

任务：

1. 每个 PHP 版本显示 FastCGI worker 数。
2. 增加最大请求数输入。
3. 显示实际运行 worker 数。
4. 显示 OPcache/Xdebug 状态。
5. 保存后提示“重启对应 PHP 服务后生效”。
6. 提供一键重启当前 PHP 实例。

界面校验：

```text
workers: 1–16
max_requests: 100–10000
```

## 6. 自动化测试

### 6.1 Rust 单元测试

增加以下测试：

- 默认配置迁移；
- worker/max requests 范围校验；
- PHP 启动命令环境变量；
- 根 PID/子 PID 关系解析；
- 合法 listener 子进程识别；
- 非本安装目录进程拒绝接管；
- stop 使用 `/T`；
- watchdog generation 防重复。

### 6.2 Windows 集成测试

建立不依赖业务项目和数据库的 PHP fixture：

```php
<?php
usleep(500000);
header('Content-Type: application/json');
echo json_encode(['pid' => getmypid()]);
```

测试流程：

1. 在随机空闲端口启动 workers=4 的 PHP-CGI。
2. 并发发送 6 个请求。
3. 收集响应 PID。
4. 验证至少使用多个不同 PID。
5. 验证总耗时符合两批执行，而不是六次串行。
6. 停止池并确认所有 PID 和端口清理完成。

### 6.3 生命周期测试

- kill 单个子 worker；
- kill 父进程；
- 快速 restart 10 次；
- NaxOne 退出后重新接管；
- 同时运行两个 PHP 版本；
- 一个 worker 执行 20 秒慢请求时，其他 worker 仍能响应健康检查。

## 7. 验收标准

### 7.1 并发

以 500ms sleep fixture、workers=4、6 并发为基准：

```text
修复前：总耗时约 3.0s
修复后：总耗时应 <= 1.3s
```

至少返回 2 个不同 PHP PID；理想情况下返回 4 个。

### 7.2 队头阻塞

同时发送：

- 1 个 10 秒慢请求；
- 3 个普通健康请求。

workers=4 时，健康请求首字节应小于 1 秒，不能全部等待慢请求完成。

### 7.3 稳定性

- 连续 1000 个请求无 PHP 池整体退出；
- worker 达到 `max_requests` 后可被父进程补充；
- watchdog 不重复创建池；
- stop/restart 后没有孤儿进程；
- 活动日志可以回查启动、恢复和失败详情。

### 7.4 性能配置

- OPcache 状态以运行时检测为准；
- Xdebug 状态可见；
- 未加载 OPcache 时不得显示“已启用”；
- 用户原 php.ini 修改前必须有备份。

## 8. 风险与回滚

### 8.1 主要风险

1. worker 数增加会增加内存和远程数据库连接数。
2. 监听 PID 可能是子进程，旧 watchdog 会误判并重复拉起。
3. 旧 stop 逻辑可能只杀一个 PID，留下 worker。
4. 修改 PHPStudy 的 php.ini 可能影响用户现有调试环境。
5. PHP 版本和来源不同，OPcache DLL 名称或默认 ini 可能不同。

### 8.2 控制措施

- 默认与 PHPStudy 一致使用每活跃版本 16 workers；不活跃版本不启动；
- 配置上限 16；
- 先修正进程树识别，再开放前端 worker 设置；
- OPcache 修改前备份；
- Xdebug 第一阶段只检测和提示；
- 每阶段独立提交、独立测试；
- 保留 workers=1 作为兼容回滚开关。

### 8.3 回滚

出现问题时：

1. 将对应 PHP 实例 workers 设置为 1；
2. 重启该 PHP 实例；
3. 从 `.bak` 恢复 php.ini；
4. 停止服务并确认整个 PHP-CGI 进程树已结束；
5. 回滚对应阶段代码，不影响 vhost 和业务项目文件。

## 9. 推荐实施顺序

```text
配置贯通
  → worker 池启动
  → watchdog/停止/接管适配
  → 并发与生命周期测试
  → OPcache 检测和安全修复
  → Xdebug 状态提示
  → 前端配置
```

不能只设置 `PHP_FCGI_CHILDREN` 就结束：如果不同时修正 PID、watchdog、stop 和
adopt 逻辑，多 worker 会造成误判、重复拉起或残留进程。

## 10. 完成定义

满足以下条件才视为修复完成：

- 所有 Rust workspace 测试通过；
- 前端构建通过；
- 新增 FastCGI 并发测试通过；
- 多 worker 生命周期测试通过；
- 实测慢请求不再阻塞所有健康请求；
- OPcache/Xdebug 状态显示与真实运行时一致；
- 活动日志覆盖启动、停止、重启、自动恢复和配置修改；
- 三处版本号按项目规范同步 bump 后再提交。

## 11. 2026-07-23 实施记录

已完成：

- 全局 worker 预算及按启用 vhost 加权分配；
- PHPStudy 兼容模式（每个活跃版本固定 16 workers）；
- Nginx 只联动启动实际使用的 PHP 版本；
- vhost 新增、更新、删除、启停后的运行池自动协调；
- `PHP_FCGI_CHILDREN` / `PHP_FCGI_MAX_REQUESTS` 启动参数；
- 父 PID、监听子 PID、watchdog、孤儿接管及整树停止；
- 设置页的总预算、单版本上限、最大请求数配置；
- 配置迁移、范围校验和分配算法单元测试。

本机 PHP 7.4.3 NTS 隔离实测：

```text
PHP_FCGI_CHILDREN=4
进程树：1 个父进程 + 4 个 worker
同一业务接口 6 并发：297–565ms
总耗时：565ms（4 + 2 两批完成）
taskkill /F /T 后测试端口已释放
```

仍待完成：

- OPcache 运行时加载状态检测与安全启用；
- Xdebug 高开销配置可见性；
- 独立的 Windows CI FastCGI fixture；
- 正式提交前按项目规范统一 bump 版本号。
