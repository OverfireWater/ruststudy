//! 本地开发用的 HTTPS 证书签发（mkcert 等价）。
//!
//! 工作流：
//! 1. 首次：生成本地 root CA → 写到 `<out_dir>/_ca/`，调 `certutil` 装当前用户的根证书库
//! 2. 后续：复用磁盘上的 CA，给每个 vhost 签 leaf 证书
//! 3. 浏览器看到 CA 在信任根 → 绿锁不弹警告
//!
//! 设计权衡：
//! - 用户级（`-user`）信任而非机器级（`-machine`）：避免每次弹 UAC，代价是只对当前 Win 账户生效
//! - 不做 NSS/Firefox 集成：Win 上 Firefox 默认 `security.enterprise_roots.enabled=true` 会读系统库
//! - leaf 397 天（浏览器对 leaf 硬上限），CA 10 年

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};

const CA_DIR_NAME: &str = "_ca";
const CA_FILE_STEM: &str = "naxone-rootCA";
const INSTALLED_STAMP: &str = ".installed";
pub const CABUNDLE_NAME: &str = "cabundle.pem";

/// 合成 PHP 用的 CA bundle = Mozilla cacert.pem 全集 + NaxOne 本地 CA。
/// 写入 `<certs_dir>/cabundle.pem`，返回路径供 php.ini 的 curl.cainfo / openssl.cafile 引用。
///
/// 幂等：内容没变就不重写（避免 ini 重新指向同样路径时的无意义写）。
/// 若 NaxOne CA 还没生成（_ca/naxone-rootCA.crt 不存在）→ 返回 Err，调用方决定是否跳过。
pub fn ensure_cabundle(certs_dir: &Path, bundled_cacert: &Path) -> Result<PathBuf, String> {
    let ca_cert_path = certs_dir.join(CA_DIR_NAME).join(format!("{}.crt", CA_FILE_STEM));
    if !ca_cert_path.is_file() {
        return Err(format!(
            "NaxOne CA 尚未生成（{} 不存在），请先在任一 vhost 生成 SSL 证书",
            ca_cert_path.display()
        ));
    }
    if !bundled_cacert.is_file() {
        return Err(format!(
            "Mozilla cacert.pem 资源缺失: {}",
            bundled_cacert.display()
        ));
    }
    std::fs::create_dir_all(certs_dir)
        .map_err(|e| format!("创建证书目录失败 {}: {}", certs_dir.display(), e))?;

    let base = std::fs::read_to_string(bundled_cacert)
        .map_err(|e| format!("读 Mozilla cacert.pem 失败: {}", e))?;
    let ca_pem = std::fs::read_to_string(&ca_cert_path)
        .map_err(|e| format!("读 NaxOne CA 失败: {}", e))?;

    let mut bundle = String::with_capacity(base.len() + ca_pem.len() + 256);
    bundle.push_str(&base);
    if !base.ends_with('\n') {
        bundle.push('\n');
    }
    bundle.push_str("\n# === NaxOne Local Dev CA ===\n");
    bundle.push_str(&ca_pem);
    if !ca_pem.ends_with('\n') {
        bundle.push('\n');
    }

    let out = certs_dir.join(CABUNDLE_NAME);
    // 内容一致就跳过写入，避免 ini watcher 误触发
    if let Ok(existing) = std::fs::read_to_string(&out) {
        if existing == bundle {
            return Ok(out);
        }
    }
    std::fs::write(&out, bundle.as_bytes())
        .map_err(|e| format!("写 cabundle 失败 {}: {}", out.display(), e))?;
    Ok(out)
}

/// 给 vhost 生成 leaf 证书。本地 CA 不存在则创建并装进系统信任库。
/// 返回 (cert_path, key_path)。
pub fn generate_self_signed(
    server_name: &str,
    aliases: &[String],
    out_dir: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("创建证书目录失败 {}: {}", out_dir.display(), e))?;

    let ca_dir = out_dir.join(CA_DIR_NAME);
    let (ca_cert, ca_key) = ensure_local_ca(&ca_dir)?;

    let sans = build_sans(server_name, aliases);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, server_name);
    dn.push(DnType::OrganizationName, "NaxOne Local Dev");

    let mut params = CertificateParams::default();
    params.distinguished_name = dn;
    params.subject_alt_names = sans;
    params.use_authority_key_identifier_extension = true;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(397);

    let leaf_key = KeyPair::generate().map_err(|e| format!("生成 leaf 密钥失败: {}", e))?;
    let leaf_cert = params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .map_err(|e| format!("CA 签发证书失败: {}", e))?;

    let safe_name = sanitize_filename(server_name);
    let cert_path = out_dir.join(format!("{}.crt", safe_name));
    let key_path = out_dir.join(format!("{}.key", safe_name));

    std::fs::write(&cert_path, leaf_cert.pem())
        .map_err(|e| format!("写证书失败 {}: {}", cert_path.display(), e))?;
    std::fs::write(&key_path, leaf_key.serialize_pem())
        .map_err(|e| format!("写私钥失败 {}: {}", key_path.display(), e))?;

    #[cfg(target_os = "windows")]
    restrict_acl_owner_only(&key_path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok((cert_path, key_path))
}

/// 拿到本地 CA。CA 文件不存在则生成；存在则从 PEM 重建并在内存重新构造 `Certificate` 对象
/// （重新 `self_signed` 不会改公钥/DN，所以 leaf 的 issuer 字段仍指向同一 CA，浏览器验签通过）。
/// 任一情况下都尝试装到系统信任库（已装过则跳过）。
fn ensure_local_ca(ca_dir: &Path) -> Result<(Certificate, KeyPair), String> {
    std::fs::create_dir_all(ca_dir)
        .map_err(|e| format!("创建 CA 目录失败 {}: {}", ca_dir.display(), e))?;
    let ca_cert_path = ca_dir.join(format!("{}.crt", CA_FILE_STEM));
    let ca_key_path = ca_dir.join(format!("{}.key", CA_FILE_STEM));

    let reuse = ca_cert_path.exists() && ca_key_path.exists();
    let ca_key = if reuse {
        let ca_key_pem = std::fs::read_to_string(&ca_key_path)
            .map_err(|e| format!("读 CA 私钥失败: {}", e))?;
        KeyPair::from_pem(&ca_key_pem).map_err(|e| format!("解析 CA 私钥失败: {}", e))?
    } else {
        KeyPair::generate().map_err(|e| format!("生成 CA 密钥失败: {}", e))?
    };

    let ca_params = build_ca_params();
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|e| format!("自签 CA 失败: {}", e))?;

    if !reuse {
        std::fs::write(&ca_cert_path, ca_cert.pem())
            .map_err(|e| format!("写 CA 证书失败: {}", e))?;
        std::fs::write(&ca_key_path, ca_key.serialize_pem())
            .map_err(|e| format!("写 CA 私钥失败: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    restrict_acl_owner_only(&ca_key_path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&ca_key_path, std::fs::Permissions::from_mode(0o600));
    }

    try_install_ca_trust(&ca_cert_path, ca_dir);
    Ok((ca_cert, ca_key))
}

/// CA 证书参数：固定 DN/Usage，复用与新建走同一份。
fn build_ca_params() -> CertificateParams {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "NaxOne Local Dev CA");
    dn.push(DnType::OrganizationName, "NaxOne");

    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(3650);
    params
}

fn build_sans(server_name: &str, aliases: &[String]) -> Vec<SanType> {
    let mut sans: Vec<SanType> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push_dns = |name: &str, sans: &mut Vec<SanType>| {
        let n = name.trim().to_ascii_lowercase();
        if n.is_empty() {
            return;
        }
        if seen.insert(n.clone()) {
            if let Ok(parsed) = rcgen::Ia5String::try_from(n) {
                sans.push(SanType::DnsName(parsed));
            }
        }
    };
    push_dns(server_name, &mut sans);
    for a in aliases {
        push_dns(a, &mut sans);
    }
    push_dns("localhost", &mut sans);
    sans.push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    sans
}

#[cfg(target_os = "windows")]
fn try_install_ca_trust(ca_cert_path: &Path, ca_dir: &Path) {
    if std::env::var_os("NAXONE_SSL_SKIP_INSTALL").is_some() {
        return;
    }
    let stamp = ca_dir.join(INSTALLED_STAMP);
    if stamp.exists() {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let result = std::process::Command::new("certutil")
        .args(["-user", "-addstore", "Root"])
        .arg(ca_cert_path)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match result {
        Ok(out) if out.status.success() => {
            let _ = std::fs::write(&stamp, "ok\n");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let _ = std::fs::write(
                ca_dir.join(".install_error"),
                format!(
                    "certutil exit {}\nstdout: {}\nstderr: {}",
                    out.status, stdout, stderr
                ),
            );
        }
        Err(e) => {
            let _ = std::fs::write(
                ca_dir.join(".install_error"),
                format!("certutil 启动失败: {}", e),
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn try_install_ca_trust(_ca_cert_path: &Path, _ca_dir: &Path) {
    // 当前目标平台只针对 Windows；macOS/Linux 走 OS 原生证书管理
}

#[cfg(target_os = "windows")]
fn restrict_acl_owner_only(path: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let username = std::env::var("USERNAME").unwrap_or_default();
    if username.is_empty() {
        return;
    }
    let _ = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{}:(R,W)", username))
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_keeps_safe_chars() {
        assert_eq!(sanitize_filename("foo.test"), "foo.test");
        assert_eq!(sanitize_filename("my-site_v1"), "my-site_v1");
        assert_eq!(sanitize_filename("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn generate_creates_ca_and_leaf() {
        // 测试不污染系统信任库
        std::env::set_var("NAXONE_SSL_SKIP_INSTALL", "1");

        let tmp = std::env::temp_dir().join(format!("naxone_ssl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let (cert, key) =
            generate_self_signed("example.test", &["www.example.test".into()], &tmp).expect("ok");
        assert!(cert.exists(), "leaf cert 未生成");
        assert!(key.exists(), "leaf key 未生成");

        let cert_text = std::fs::read_to_string(&cert).unwrap();
        assert!(cert_text.starts_with("-----BEGIN CERTIFICATE-----"));
        let key_text = std::fs::read_to_string(&key).unwrap();
        assert!(key_text.contains("PRIVATE KEY"));

        let ca_crt = tmp.join(CA_DIR_NAME).join(format!("{}.crt", CA_FILE_STEM));
        let ca_key = tmp.join(CA_DIR_NAME).join(format!("{}.key", CA_FILE_STEM));
        assert!(ca_crt.exists(), "CA 证书未生成");
        assert!(ca_key.exists(), "CA 私钥未生成");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn second_call_reuses_existing_ca() {
        std::env::set_var("NAXONE_SSL_SKIP_INSTALL", "1");

        let tmp =
            std::env::temp_dir().join(format!("naxone_ssl_reuse_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let _ = generate_self_signed("a.test", &[], &tmp).expect("ok");
        let ca_crt = tmp.join(CA_DIR_NAME).join(format!("{}.crt", CA_FILE_STEM));
        let ca_pem_1 = std::fs::read_to_string(&ca_crt).unwrap();

        let _ = generate_self_signed("b.test", &[], &tmp).expect("ok");
        let ca_pem_2 = std::fs::read_to_string(&ca_crt).unwrap();

        assert_eq!(ca_pem_1, ca_pem_2, "二次签发应复用同一 CA");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
