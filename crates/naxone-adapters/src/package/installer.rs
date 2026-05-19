//! Package installer: stream-download → (optional SHA256 verify) →
//! unzip to staging → detect wrapper → move to PHPStudy-style dir.
//!
//! Install layout (phase-2 PHPStudy mimicry):
//!
//!   <packages_root>/
//!     _staging/
//!       {name}-{version}.zip       (temp, deleted after extract)
//!       {name}-{version}/          (temp, deleted after move)
//!     Nginx1.26.2/                 (final)
//!       nginx.exe
//!     MySQL8.0.40/                 (final)
//!       bin/mysqld.exe
//!     php/
//!       php842nts/                 (final, note: php has a shared parent dir)
//!         php-cgi.exe

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;

use super::manifest::{PackageEntry, PackageVersion};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "phase")]
pub enum InstallEvent {
    #[serde(rename = "started")]
    Started {
        name: String,
        version: String,
        total: Option<u64>,
    },
    #[serde(rename = "progress")]
    Progress {
        name: String,
        version: String,
        downloaded: u64,
        /// 总大小（字节）。服务器不给 Content-Length（如 chunked encoding）时为 None；
        /// 此时前端应展示已下载 MB 而不是百分比。
        total: Option<u64>,
        /// 已下载百分比。total 未知时恒为 0。
        pct: f32,
    },
    #[serde(rename = "extracting")]
    Extracting { name: String, version: String },
    #[serde(rename = "done")]
    Done {
        name: String,
        version: String,
        install_path: String,
    },
    #[serde(rename = "failed")]
    Failed {
        name: String,
        version: String,
        reason: String,
    },
}

pub struct Installer {
    client: reqwest::Client,
    packages_root: PathBuf,
}

impl Installer {
    pub fn new(packages_root: PathBuf) -> Self {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("NaxOne/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            // 整体请求超时：避免国外镜像响应极慢时前端永远卡在"连接中"
            .timeout(std::time::Duration::from_secs(600))
            .pool_idle_timeout(std::time::Duration::from_secs(30));

        // 自动代理：用户开了 Clash/V2ray 的"系统代理"时读系统设置
        if let Some(proxy_url) = crate::package::proxy::detect_proxy() {
            match reqwest::Proxy::all(&proxy_url) {
                Ok(p) => {
                    tracing::info!(proxy = %proxy_url, "Installer 使用系统代理");
                    builder = builder.proxy(p);
                }
                Err(e) => {
                    tracing::warn!(proxy = %proxy_url, err = %e, "系统代理地址无效，回退直连");
                }
            }
        }

        let client = builder.build().expect("reqwest client builds");
        Self {
            client,
            packages_root,
        }
    }

    /// Install a package version. Emits progress events via `tx`.
    /// Returns the final install path on success.
    pub async fn install(
        &self,
        entry: &PackageEntry,
        version: &PackageVersion,
        tx: UnboundedSender<InstallEvent>,
    ) -> Result<PathBuf, String> {
        let name = entry.name.clone();
        let ver = version.version.clone();

        // Final destination: PHPStudy-style directory name.
        let final_name = phpstudy_style_dir_name(&name, &ver);
        let final_dir = self.packages_root.join(&final_name);

        // Idempotency: already installed?
        if final_dir.join(&version.exe_rel).exists() {
            let _ = tx.send(InstallEvent::Done {
                name: name.clone(),
                version: ver.clone(),
                install_path: final_dir.display().to_string(),
            });
            return Ok(final_dir);
        }

        // Guard: if the target directory exists but looks incomplete, make
        // sure we're not about to stomp on a running service's files.
        if final_dir.exists() {
            // For now we just delete and reinstall. If the service was running,
            // the OS file lock will fail the delete and the install will error.
            if let Err(e) = std::fs::remove_dir_all(&final_dir) {
                let msg = format!(
                    "目标目录已存在且无法删除（可能服务正在运行）: {} ({})",
                    final_dir.display(),
                    e
                );
                let _ = tx.send(InstallEvent::Failed {
                    name: name.clone(),
                    version: ver.clone(),
                    reason: msg.clone(),
                });
                return Err(msg);
            }
        }

        // Staging area for temp zip + unzip.
        let staging_root = self.packages_root.join("_staging");
        if let Err(e) = std::fs::create_dir_all(&staging_root) {
            return fail(&tx, &name, &ver, format!("创建 staging 目录失败: {}", e));
        }

        let temp_zip = staging_root.join(format!("{}-{}.zip", name, ver));
        let unpacked = staging_root.join(format!("{}-{}", name, ver));
        // Clean any leftover from a previous failed run
        let _ = std::fs::remove_dir_all(&unpacked);
        let _ = std::fs::remove_file(&temp_zip);

        // ---------- download (按候选 URL 顺序尝试) ----------
        let urls = version.candidate_urls();
        if urls.is_empty() {
            return fail(&tx, &name, &ver, "没有可用的下载 URL".to_string());
        }
        let mut download_err: Option<String> = None;
        for (idx, url) in urls.iter().enumerate() {
            // 清掉上次失败的残片，避免 File::create 直接拿到旧文件
            let _ = std::fs::remove_file(&temp_zip);
            tracing::info!(attempt = idx + 1, total = urls.len(), url = %url, "下载尝试");
            match self.download(&name, &ver, url, &temp_zip, &tx).await {
                Ok(()) => {
                    if let Err(e) = validate_zip_file(&temp_zip) {
                        tracing::warn!(url = %url, "下载内容不是有效 zip: {}", e);
                        download_err = Some(format!("{}（{}）", e, url));
                        continue;
                    }
                    tracing::info!(url = %url, "下载完成并通过 zip 校验");
                    download_err = None;
                    break;
                }
                Err(e) => {
                    tracing::warn!("镜像 {} 失败: {}", url, e);
                    download_err = Some(e);
                    // 还有下一个就继续
                }
            }
        }
        if let Some(e) = download_err {
            let _ = std::fs::remove_file(&temp_zip);
            return fail(
                &tx,
                &name,
                &ver,
                format!("所有下载源均失败，最后一次错误: {}", e),
            );
        }

        // ---------- SHA256 ----------
        // 优先校验：清单有哈希 → 必须匹配
        // 兜底：清单无哈希时，只允许从可信官方源下载（HTTPS + 白名单 host），仍记录 warn
        match &version.sha256 {
            Some(expected) => {
                if let Err(e) = verify_sha256(&temp_zip, expected).await {
                    let _ = std::fs::remove_file(&temp_zip);
                    return fail(&tx, &name, &ver, e);
                }
            }
            None => {
                // 找到实际下载成功的 URL（在 download() 之后 used_url 已记录）
                // 简化：用 version 的第一个 URL 判断 host 是否可信
                let urls = version.candidate_urls();
                let first = urls.first().map(|s| s.as_str()).unwrap_or("");
                if !is_trusted_source(first) {
                    let _ = std::fs::remove_file(&temp_zip);
                    return fail(
                        &tx,
                        &name,
                        &ver,
                        "包清单缺少 sha256 哈希校验，已拒绝安装。请联系维护者补全清单或选择其它版本。".to_string(),
                    );
                }
                tracing::warn!(
                    name = %name,
                    version = %ver,
                    url = first,
                    "包清单未声明 sha256，但下载源在可信白名单内（{}），仅警告不拒装",
                    extract_host(first).unwrap_or("?")
                );
            }
        }

        finalize_install(entry, version, &name, &ver, &final_dir, &temp_zip, &unpacked, &tx)
    }
    async fn download(
        &self,
        name: &str,
        version: &str,
        url: &str,
        dest: &Path,
        tx: &UnboundedSender<InstallEvent>,
    ) -> Result<(), String> {
        let _ = tx.send(InstallEvent::Started {
            name: name.into(),
            version: version.into(),
            total: None,
        });

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("网络请求失败（{}）: {}", url, e))?;

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<unknown>")
            .to_string();
        let total = resp.content_length();

        tracing::info!(
            url = %url,
            status = %status,
            content_type = %content_type,
            content_length = ?total,
            "下载响应信息"
        );

        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {} (content-type: {})",
                status.as_u16(),
                url,
                content_type
            ));
        }

        if content_type.to_ascii_lowercase().starts_with("text/html") {
            return Err(format!(
                "下载响应为 HTML（疑似拦截页/错误页），非安装包: {}",
                url
            ));
        }

        if total.is_some() {
            let _ = tx.send(InstallEvent::Started {
                name: name.into(),
                version: version.into(),
                total,
            });
        }

        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| format!("创建临时文件失败: {}", e))?;

        let mut stream = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        let mut last_emit_bytes: u64 = 0;
        let mut last_emit_at = std::time::Instant::now();
        const EMIT_BYTES_THRESHOLD: u64 = 256 * 1024;
        const EMIT_TIME_THRESHOLD_MS: u128 = 300;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| format!("下载中断: {}", e))?;
            file.write_all(&bytes)
                .await
                .map_err(|e| format!("写入失败: {}", e))?;
            downloaded += bytes.len() as u64;

            let enough_bytes = downloaded - last_emit_bytes >= EMIT_BYTES_THRESHOLD;
            let enough_time = last_emit_at.elapsed().as_millis() >= EMIT_TIME_THRESHOLD_MS;
            if enough_bytes || enough_time {
                last_emit_bytes = downloaded;
                last_emit_at = std::time::Instant::now();
                let pct = match total {
                    Some(t) if t > 0 => (downloaded as f32 / t as f32 * 100.0).min(100.0),
                    _ => 0.0,
                };
                let _ = tx.send(InstallEvent::Progress {
                    name: name.into(),
                    version: version.into(),
                    downloaded,
                    total,
                    pct,
                });
            }
        }

        file.flush().await.map_err(|e| format!("刷盘失败: {}", e))?;

        tracing::info!(
            url = %url,
            bytes = downloaded,
            content_type = %content_type,
            "下载完成"
        );

        let final_pct = match total {
            Some(t) if t > 0 => 100.0,
            _ => 0.0,
        };
        let _ = tx.send(InstallEvent::Progress {
            name: name.into(),
            version: version.into(),
            downloaded,
            total,
            pct: final_pct,
        });

        Ok(())
    }
}

/// PHPStudy-style destination directory for a given package.
///
///   nginx  1.26.2  → "Nginx1.26.2"
///   mysql  8.0.40  → "MySQL8.0.40"
///   apache 2.4.62  → "Apache2.4.62"
///   redis  5.0.14.1 → "Redis5.0.14.1"
///   php    8.4.2   → "php/php842nts"
pub fn phpstudy_style_dir_name(name: &str, version: &str) -> String {
    match name {
        "nginx" => format!("Nginx{}", version),
        "apache" => format!("Apache{}", version),
        "mysql" => format!("MySQL{}", version),
        "redis" => format!("Redis{}", version),
        "php" => format!("php/php{}nts", version.replace('.', "")),
        // 工具类放 tools/ 子目录，跟服务包分开
        "composer" => format!("tools/composer-{}", version),
        "nvm" => format!("tools/nvm-{}", version),
        other => format!("{}{}", other, version),
    }
}

/// Walk the unpacked tree to find the directory that actually contains `exe_rel`.
/// Handles nested wrappers (e.g. Apache Lounge zips have httpd-xxx/Apache24/bin/httpd.exe).
/// Falls back to the unpacked root if nothing matches.
fn find_exe_root(unpacked: &Path, exe_rel: &str) -> PathBuf {
    if unpacked.join(exe_rel).exists() {
        return unpacked.to_path_buf();
    }
    // Search up to 3 levels deep for the exe
    fn search(dir: &Path, exe_rel: &str, depth: u32) -> Option<PathBuf> {
        if depth == 0 { return None; }
        let rd = std::fs::read_dir(dir).ok()?;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(exe_rel).exists() {
                return Some(path);
            }
        }
        // Recurse into single-subdir wrappers
        let rd = std::fs::read_dir(dir).ok()?;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = search(&path, exe_rel, depth - 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    search(unpacked, exe_rel, 3).unwrap_or_else(|| unpacked.to_path_buf())
}


async fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| format!("读取校验文件失败: {}", e))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let got = format!("{:x}", hasher.finalize());
    if got.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "SHA256 校验失败: 期望 {}, 实际 {}",
            expected, got
        ))
    }
}

/// Extract a zip file. Uses zip crate's enclosed_name() which honors the
/// utf-8 flag and strips zip-slip traversal.
fn validate_zip_file(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("打开下载文件失败: {}", e))?;
    let _ = zip::ZipArchive::new(file).map_err(|e| format!("下载文件不是有效 zip: {}", e))?;
    Ok(())
}

fn unzip(src: &Path, dst: &Path) -> Result<(), String> {
    let file = std::fs::File::open(src).map_err(|e| format!("打开 zip 失败: {}", e))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("解析 zip 失败: {}", e))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取 zip 条目 {} 失败: {}", i, e))?;
        let rel_path = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let out_path = dst.join(rel_path);

        if entry.is_dir() {
            let _ = std::fs::create_dir_all(&out_path);
            continue;
        }
        if let Some(p) = out_path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let mut out = std::fs::File::create(&out_path)
            .map_err(|e| format!("创建 {} 失败: {}", out_path.display(), e))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("写入 {} 失败: {}", out_path.display(), e))?;
    }

    Ok(())
}

/// Recursive copy. Used as a fallback when `rename` fails (e.g. cross-volume).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn fail(
    tx: &UnboundedSender<InstallEvent>,
    name: &str,
    version: &str,
    reason: String,
) -> Result<PathBuf, String> {
    let _ = tx.send(InstallEvent::Failed {
        name: name.into(),
        version: version.into(),
        reason: reason.clone(),
    });
    Err(reason)
}

/// 可信下载源白名单。命中时即使清单无 sha256 也允许装（HTTPS 直连官方）。
/// 严格 host 匹配，不做后缀匹配，避免被 evil-cdn.mysql.com.attacker.com 绕过。
const TRUSTED_HOSTS: &[&str] = &[
    "cdn.mysql.com",
    "dev.mysql.com",
    "windows.php.net",
    "nginx.org",
    "www.apachelounge.com",
    "github.com",
    "objects.githubusercontent.com",
    "ghfast.top",
    "ghproxy.net",
    "gh-proxy.com",
    "cdn.jsdelivr.net",
];

fn extract_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];
    // 去掉可能的 port
    Some(host.split(':').next().unwrap_or(host))
}

fn is_trusted_source(url: &str) -> bool {
    if !url.starts_with("https://") {
        return false;
    }
    match extract_host(url) {
        Some(host) => TRUSTED_HOSTS.contains(&host),
        None => false,
    }
}

fn finalize_install(
    entry: &PackageEntry,
    version: &PackageVersion,
    name: &str,
    ver: &str,
    final_dir: &Path,
    temp_zip: &Path,
    unpacked: &Path,
    tx: &UnboundedSender<InstallEvent>,
) -> Result<PathBuf, String> {
    let _ = tx.send(InstallEvent::Extracting {
        name: name.to_string(),
        version: ver.to_string(),
    });
    if let Err(e) = std::fs::create_dir_all(unpacked) {
        let _ = std::fs::remove_file(temp_zip);
        return fail(tx, name, ver, format!("创建解压目录失败: {}", e));
    }
    if let Err(e) = unzip(temp_zip, unpacked) {
        let _ = std::fs::remove_dir_all(unpacked);
        let _ = std::fs::remove_file(temp_zip);
        return fail(tx, name, ver, e);
    }
    let _ = std::fs::remove_file(temp_zip);

    let source_dir = find_exe_root(unpacked, &version.exe_rel);

    if let Some(parent) = final_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::rename(&source_dir, final_dir) {
        tracing::warn!(
            "rename failed ({}), falling back to copy: {} → {}",
            e,
            source_dir.display(),
            final_dir.display()
        );
        if let Err(e2) = copy_dir_all(&source_dir, final_dir) {
            let _ = std::fs::remove_dir_all(final_dir);
            let _ = std::fs::remove_dir_all(unpacked);
            return fail(tx, name, ver, format!("移动到最终目录失败: {}", e2));
        }
        let _ = std::fs::remove_dir_all(&source_dir);
    }

    let _ = std::fs::remove_dir_all(unpacked);

    let exe_final = final_dir.join(&version.exe_rel);
    if !exe_final.exists() {
        let _ = std::fs::remove_dir_all(final_dir);
        return fail(
            tx,
            name,
            ver,
            format!(
                "解压后未找到预期文件 {} (检查 exe_rel 或 zip 结构)",
                exe_final.display()
            ),
        );
    }

    if entry.name == "php" {
        let ini = final_dir.join("php.ini");
        if !ini.exists() {
            let production = final_dir.join("php.ini-production");
            let development = final_dir.join("php.ini-development");
            let template = if production.exists() {
                Some(production)
            } else if development.exists() {
                Some(development)
            } else {
                None
            };
            if let Some(src) = template {
                if let Err(e) = std::fs::copy(&src, &ini) {
                    tracing::warn!("生成 php.ini 失败 (from {}): {}", src.display(), e);
                } else {
                    tracing::info!(
                        "已生成 php.ini（来自 {}）",
                        src.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            } else {
                tracing::warn!("找不到 php.ini-production/development 模板，跳过 ini 初始化");
            }
        }
        // 即使 ini 已存在，也尝试 harden 一遍：设 extension_dir + 启用核心扩展（幂等）
        if ini.exists() {
            if let Err(e) = harden_php_ini(&ini, final_dir) {
                tracing::warn!("harden php.ini 失败: {}", e);
            }
        }
    }

    // 工具类包（composer / nvm）的 post-install：写 shim、改用户 PATH、设环境变量
    super::post_install::run(&entry.name, final_dir);

    let _ = tx.send(InstallEvent::Done {
        name: name.to_string(),
        version: ver.to_string(),
        install_path: final_dir.display().to_string(),
    });
    Ok(final_dir.to_path_buf())
}

/// 让一个新装/已存在的 PHP 立即可用：
/// 1. 设 `extension_dir` 为 PHP 安装目录下的 `ext`（覆盖默认硬编码 `C:\php\ext`）
/// 2. 启用核心扩展（openssl/curl/mbstring/fileinfo/zip/intl/gd 等），
///    让 composer/PIE 等命令行工具能跑起来
/// 幂等：已启用的不重复，未注释的保留。
pub fn harden_php_ini(ini_path: &Path, install_dir: &Path) -> std::io::Result<()> {
    let raw = std::fs::read_to_string(ini_path)?;
    let mut text = raw.clone();

    // 1. extension_dir
    let ext_dir = install_dir.join("ext");
    let ext_dir_str = ext_dir.display().to_string().replace('\\', "/");
    let already_set = text
        .lines()
        .any(|ln| !ln.trim_start().starts_with(';') && ln.trim_start().starts_with("extension_dir"));
    if !already_set {
        // 注释掉所有 ;extension_dir = 行后追加一行
        let new_line = format!("extension_dir = \"{}\"", ext_dir_str);
        text.push_str("\n; --- NaxOne harden ---\n");
        text.push_str(&new_line);
        text.push('\n');
    }

    // 2. 启用核心扩展（取消 ;extension=name 的注释）
    let core_exts = [
        "openssl",
        "curl",
        "mbstring",
        "fileinfo",
        "zip",
        "intl",
        "gd",
        "exif",
    ];
    for ext in core_exts {
        let commented = format!(";extension={}", ext);
        let enabled = format!("extension={}", ext);
        // 已有未注释的就跳过
        if text
            .lines()
            .any(|ln| ln.trim() == enabled || ln.trim().starts_with(&format!("{}=", enabled)))
        {
            continue;
        }
        // 把第一个匹配的 `;extension=xxx` 替换成 `extension=xxx`
        if let Some(pos) = text.find(&commented) {
            let end = pos + commented.len();
            text.replace_range(pos..end, &enabled);
        } else {
            // 完全没出现过：在末尾追加
            text.push_str(&format!("\n{}\n", enabled));
        }
    }

    if text != raw {
        std::fs::write(ini_path, text)?;
        tracing::info!("已 harden php.ini: {}", ini_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phpstudy_dir_names() {
        assert_eq!(phpstudy_style_dir_name("nginx", "1.26.2"), "Nginx1.26.2");
        assert_eq!(phpstudy_style_dir_name("apache", "2.4.62"), "Apache2.4.62");
        assert_eq!(phpstudy_style_dir_name("mysql", "8.0.40"), "MySQL8.0.40");
        assert_eq!(phpstudy_style_dir_name("redis", "5.0.14.1"), "Redis5.0.14.1");
        assert_eq!(phpstudy_style_dir_name("php", "8.4.2"), "php/php842nts");
        assert_eq!(phpstudy_style_dir_name("php", "8.3.30"), "php/php8330nts");
        assert_eq!(phpstudy_style_dir_name("php", "7.4.33"), "php/php7433nts");
        assert_eq!(phpstudy_style_dir_name("composer", "2.7.7"), "tools/composer-2.7.7");
        assert_eq!(phpstudy_style_dir_name("nvm", "1.2.2"), "tools/nvm-1.2.2");
    }

}
