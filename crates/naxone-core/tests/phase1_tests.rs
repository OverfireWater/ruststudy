//! Phase 1 verification tests: source tagging, config persistence, and merge behavior.

use naxone_core::config::{AppConfig, ExtraInstallPath, GeneralConfig};
use naxone_core::domain::service::ServiceKind;
use naxone_core::domain::vhost::{VhostSource, VirtualHost};
use naxone_core::use_cases::vhost_mgr::VhostManager;
use std::path::PathBuf;

fn sample_vhost(id: &str, source: VhostSource) -> VirtualHost {
    VirtualHost {
        id: id.into(),
        server_name: id.split('_').next().unwrap().into(),
        aliases: vec![],
        listen_port: 80,
        document_root: PathBuf::from("D:/x"),
        php_version: None,
        php_fastcgi_port: None,
        php_install_path: None,
        index_files: "index.php".into(),
        rewrite_rule: String::new(),
        autoindex: false,
        ssl: None,
        custom_directives: None,
        access_log: None,
        enabled: true,
        created_at: String::new(),
        expires_at: String::new(),
        sync_hosts: true,
        source,
    }
}

// ---------- VhostSource serde ----------

#[test]
fn vhost_source_serde_roundtrip_phpstudy() {
    let s = VhostSource::PhpStudy;
    let j = serde_json::to_string(&s).unwrap();
    let back: VhostSource = serde_json::from_str(&j).unwrap();
    assert_eq!(s, back);
}

#[test]
fn vhost_source_serde_roundtrip_custom() {
    let s = VhostSource::Custom;
    let j = serde_json::to_string(&s).unwrap();
    let back: VhostSource = serde_json::from_str(&j).unwrap();
    assert_eq!(s, back);
}

#[test]
fn vhost_source_serde_roundtrip_standalone() {
    let s = VhostSource::Standalone("nginx".into());
    let j = serde_json::to_string(&s).unwrap();
    let back: VhostSource = serde_json::from_str(&j).unwrap();
    assert_eq!(s, back);
}

#[test]
fn vhost_source_default_is_custom() {
    // Legacy JSON without a source field must deserialize to Custom
    let legacy = r#"{
        "id": "test.nm_80",
        "server_name": "test.nm",
        "aliases": [],
        "listen_port": 80,
        "document_root": "D:/x",
        "php_version": null,
        "php_fastcgi_port": null,
        "php_install_path": null,
        "index_files": "index.php",
        "rewrite_rule": "",
        "autoindex": false,
        "ssl": null,
        "custom_directives": null,
        "access_log": null,
        "enabled": true,
        "created_at": "",
        "expires_at": "",
        "sync_hosts": true
    }"#;
    let v: VirtualHost = serde_json::from_str(legacy).unwrap();
    assert_eq!(v.source, VhostSource::Custom);
}

// ---------- VirtualHost full JSON roundtrip preserves source ----------

#[test]
fn virtual_host_full_roundtrip_preserves_source() {
    let original = sample_vhost("test_80", VhostSource::Standalone("nginx".into()));
    let j = serde_json::to_string(&original).unwrap();
    let back: VirtualHost = serde_json::from_str(&j).unwrap();
    assert_eq!(back.source, VhostSource::Standalone("nginx".into()));
    assert_eq!(back.id, "test_80");
}

// ---------- merge_vhosts behavior ----------

#[test]
fn merge_vhosts_keeps_saved_source_over_scanned() {
    // The scanner reports this vhost as PhpStudy (it's in the PhpStudy dir),
    // but saved metadata says it's Custom (user-created). Saved wins.
    let scanned = vec![sample_vhost("test.nm_80", VhostSource::PhpStudy)];
    let saved = vec![sample_vhost("test.nm_80", VhostSource::Custom)];

    let merged = VhostManager::merge_vhosts(scanned, saved);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].source, VhostSource::Custom);
}

#[test]
fn merge_vhosts_keeps_saved_only_entries() {
    // Previously saved .conf was deleted from disk. Scanner returns nothing, but
    // the saved JSON still has the entry. It must not silently disappear.
    let scanned: Vec<VirtualHost> = vec![];
    let saved = vec![sample_vhost("orphan_80", VhostSource::Custom)];

    let merged = VhostManager::merge_vhosts(scanned, saved);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].id, "orphan_80");
}

#[test]
fn merge_vhosts_preserves_saved_metadata_on_match() {
    let mut scanned = sample_vhost("a.local_80", VhostSource::PhpStudy);
    scanned.expires_at = String::new();
    scanned.enabled = true;

    let mut saved = sample_vhost("a.local_80", VhostSource::Custom);
    saved.expires_at = "2099-01-01".into();
    saved.enabled = false;
    saved.sync_hosts = false;
    saved.created_at = "2024-01-01".into();

    let merged = VhostManager::merge_vhosts(vec![scanned], vec![saved]);
    assert_eq!(merged[0].expires_at, "2099-01-01");
    assert_eq!(merged[0].enabled, false);
    assert_eq!(merged[0].sync_hosts, false);
    assert_eq!(merged[0].created_at, "2024-01-01");
    assert_eq!(merged[0].source, VhostSource::Custom);
}

// ---------- AppConfig TOML roundtrip ----------

#[test]
fn app_config_toml_roundtrip_with_extras() {
    let cfg = AppConfig {
        general: GeneralConfig {
            data_dir: PathBuf::from(r"D:\phpstudy_pro"),
            www_root: PathBuf::from(r"D:\phpstudy_pro\WWW"),
            phpstudy_path: Some(PathBuf::from(r"D:\phpstudy_pro")),
            auto_start: vec!["nginx".into()],
            log_dir: None,
            log_retention_days: 7,
            extra_install_paths: vec![
                ExtraInstallPath {
                    id: "nginx-123".into(),
                    kind: ServiceKind::Nginx,
                    path: PathBuf::from(r"C:\my-nginx"),
                    label: Some("my local nginx".into()),
                },
                ExtraInstallPath {
                    id: "mysql-456".into(),
                    kind: ServiceKind::Mysql,
                    path: PathBuf::from(r"C:\my-mysql"),
                    label: None,
                },
            ],
            package_install_root: None,
            global_php_version: None,
            stop_services_on_exit: false,
            ignored_system_tools: Vec::new(),
        },
        web_server: Default::default(),
        mysql: Default::default(),
        redis: Default::default(),
        php_runtime: Default::default(),
        php_instances: Default::default(),
    };

    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let back: AppConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(back.general.extra_install_paths.len(), 2);
    assert_eq!(back.general.extra_install_paths[0].kind, ServiceKind::Nginx);
    assert_eq!(
        back.general.extra_install_paths[0].path,
        PathBuf::from(r"C:\my-nginx")
    );
    assert_eq!(
        back.general.extra_install_paths[0].label.as_deref(),
        Some("my local nginx")
    );
    assert_eq!(back.general.extra_install_paths[1].kind, ServiceKind::Mysql);
}

#[test]
fn app_config_legacy_toml_has_empty_extras() {
    // Config files written by older versions have no [[general.extra_install_paths]].
    // After load they should present as an empty vec.
    let legacy = r#"
[general]
data_dir = "D:\\phpstudy_pro"
www_root = "D:\\phpstudy_pro\\WWW"
phpstudy_path = "D:\\phpstudy_pro"
auto_start = ["nginx"]
log_retention_days = 7
"#;
    let cfg: AppConfig = toml::from_str(legacy).unwrap();
    assert!(cfg.general.extra_install_paths.is_empty());
    assert!(cfg.general.package_install_root.is_none());
}
