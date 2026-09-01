//! auth 运行时强度测试（region_host / 凭据后端 / AuthManager 生命周期 / HAR）。
//!
//! 与 `src/auth/mod.rs` 里的单元测试互补：这里全部走公开 API + 临时目录，
//! 针对的是「单测能过、但在边界输入 / 坏凭据文件 / 重复保存下会出问题」的缺陷：
//! region_host 非法形态漏过、凭据错误信息泄漏 token、AuthInfo Debug 泄漏、
//! 保存元数据失败后凭据存储未回滚、旧版 auth.json 里的 token 未从磁盘清除。
//!
//! 运行：
//! - 日常级：`cargo test -p zeppbridge-core --test auth_stress`
//! - 压力级：`cargo test -p zeppbridge-core --test auth_stress -- --ignored --nocapture`
//!
//! 原则：只用合成凭据；不触碰真实用户数据目录；错误信息里不得出现 token。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use zeppbridge_core::auth::{AuthManager, CredentialBackend, normalize_region_host};
use zeppbridge_core::models::AuthInfo;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "zb-auth-stress-{}-{}-{}",
        label,
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[derive(Default)]
struct MemoryBackend(Mutex<HashMap<String, String>>);

impl CredentialBackend for MemoryBackend {
    fn set(&self, user_id: &str, token: &str) -> std::result::Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(user_id.to_string(), token.to_string());
        Ok(())
    }

    fn get(&self, user_id: &str) -> std::result::Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().get(user_id).cloned())
    }

    fn delete(&self, user_id: &str) -> std::result::Result<(), String> {
        self.0.lock().unwrap().remove(user_id);
        Ok(())
    }
}

/// 固定返回指定错误的后端，用来验证错误路径。
struct FailingBackend(&'static str);

impl CredentialBackend for FailingBackend {
    fn set(&self, _user_id: &str, _token: &str) -> std::result::Result<(), String> {
        Err(self.0.to_string())
    }

    fn get(&self, _user_id: &str) -> std::result::Result<Option<String>, String> {
        Err(self.0.to_string())
    }

    fn delete(&self, _user_id: &str) -> std::result::Result<(), String> {
        Err(self.0.to_string())
    }
}

/// 记录 set 调用以便验证回滚。
struct RecordingBackend {
    state: Mutex<Option<String>>,
}

impl RecordingBackend {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    fn last(&self) -> Option<String> {
        self.state.lock().unwrap().clone()
    }
}

impl CredentialBackend for RecordingBackend {
    fn set(&self, _user_id: &str, token: &str) -> std::result::Result<(), String> {
        *self.state.lock().unwrap() = Some(token.to_string());
        Ok(())
    }

    fn get(&self, _user_id: &str) -> std::result::Result<Option<String>, String> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn delete(&self, _user_id: &str) -> std::result::Result<(), String> {
        *self.state.lock().unwrap() = None;
        Ok(())
    }
}

fn valid_auth() -> AuthInfo {
    AuthInfo {
        app_token: "  synthetic-token-42  ".to_string(),
        user_id: "  synthetic-user-1  ".to_string(),
        region_host: "  https://API-MIFIT.ZEPP.COM/  ".to_string(),
    }
}

// ---------------------------------------------------------------------------
// region_host 边界
// ---------------------------------------------------------------------------

#[test]
fn region_host_accepts_valid_https_origins() {
    for (input, expected) in [
        ("https://api-mifit.zepp.com", "https://api-mifit.zepp.com"),
        ("https://API-MIFIT.ZEPP.COM/", "https://api-mifit.zepp.com"),
        (
            "https://api-mifit.zepp.com:443",
            "https://api-mifit.zepp.com",
        ),
        (
            "https://api-mifit.zepp.com:8443",
            "https://api-mifit.zepp.com:8443",
        ),
        ("https://[::1]", "https://[::1]"),
        ("https://[::1]:8443", "https://[::1]:8443"),
    ] {
        assert_eq!(
            normalize_region_host(input).unwrap(),
            expected,
            "input={input}"
        );
    }
}

#[test]
fn region_host_rejects_illegal_shapes() {
    for input in [
        "",
        "   ",
        "not-a-url",
        "http://api-mifit.zepp.com",
        "ftp://api-mifit.zepp.com",
        "https://user:pass@api-mifit.zepp.com",
        "https://api-mifit.zepp.com/path",
        "https://api-mifit.zepp.com?query=1",
        "https://api-mifit.zepp.com#frag",
        "api-mifit.zepp.com",
        "//api-mifit.zepp.com",
    ] {
        assert!(
            normalize_region_host(input).is_err(),
            "input={input} 应当被拒绝"
        );
    }
}

#[test]
fn region_host_is_idempotent() {
    let raw = "  https://API-MIFIT-EU2.ZEPP.COM:8443/  ";
    let once = normalize_region_host(raw).unwrap();
    let twice = normalize_region_host(&once).unwrap();
    assert_eq!(once, twice);
    assert_eq!(twice, "https://api-mifit-eu2.zepp.com:8443");
}

// ---------------------------------------------------------------------------
// AuthManager 生命周期与敏感信息
// ---------------------------------------------------------------------------

#[test]
fn auth_manager_roundtrip_trims_input_and_masks_token() {
    let dir = temp_dir("roundtrip");
    let backend = Arc::new(MemoryBackend::default());
    let manager = AuthManager::with_credential_backend(dir.clone(), backend.clone());

    manager.save_auth(&valid_auth()).unwrap();

    let loaded = manager.load_auth().unwrap().unwrap();
    assert_eq!(loaded.app_token, "synthetic-token-42");
    assert_eq!(loaded.user_id, "synthetic-user-1");
    assert_eq!(loaded.region_host, "https://api-mifit.zepp.com");

    let status = manager.status().unwrap();
    assert!(status.configured);
    assert_eq!(status.user_id.as_deref(), Some("synthetic-user-1"));
    assert_eq!(status.token_masked.as_deref(), Some("sy…42"));

    // auth.json 里绝不能出现 token。
    let auth_json = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    assert!(
        !auth_json.contains("synthetic-token-42"),
        "auth.json 泄漏了 token: {auth_json}"
    );

    manager.clear_auth().unwrap();
    assert!(!dir.join("auth.json").exists());
    assert_eq!(backend.get("synthetic-user-1").unwrap(), None);
}

#[test]
fn auth_manager_rolls_back_credential_store_when_metadata_write_fails() {
    let dir = temp_dir("rollback");
    // 让 auth.json 的父目录是一个已存在的文件，create_dir_all 会失败，
    // 从而触发 save_auth 里的 write_stored 失败与回滚。
    let obstacle = dir.join("obstacle");
    std::fs::write(&obstacle, "not a directory").unwrap();
    let bad_data_dir = obstacle.join("nested");

    let backend = Arc::new(RecordingBackend::new());
    let manager = AuthManager::with_credential_backend(bad_data_dir, backend.clone());

    let result = manager.save_auth(&valid_auth());
    assert!(result.is_err(), "元数据写失败时 save_auth 应当报错");

    // 回滚必须清掉刚刚写进凭据存储的 token。
    assert_eq!(
        backend.last(),
        None,
        "凭据存储应当被回滚到未保存状态"
    );
}

#[test]
fn auth_manager_migrates_legacy_token_off_disk() {
    let dir = temp_dir("legacy");
    let backend = Arc::new(MemoryBackend::default());
    let manager = AuthManager::with_credential_backend(dir.clone(), backend.clone());

    // 模拟旧版 auth.json：明文存 token，version=0，updated_at 为空。
    std::fs::write(
        dir.join("auth.json"),
        serde_json::json!({
            "version": 0,
            "user_id": "legacy-user",
            "region_host": "https://api-mifit.zepp.com",
            "app_token": "legacy-secret-token",
        })
        .to_string(),
    )
    .unwrap();

    let loaded = manager.load_auth().unwrap().unwrap();
    assert_eq!(loaded.app_token, "legacy-secret-token");
    assert_eq!(loaded.user_id, "legacy-user");

    // token 必须被迁进凭据后端。
    assert_eq!(
        backend.get("legacy-user").unwrap().as_deref(),
        Some("legacy-secret-token")
    );

    // 磁盘上的 auth.json 重写后必须不再包含 token。
    let rewritten = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    assert!(
        !rewritten.contains("legacy-secret-token"),
        "迁移后 auth.json 仍残留 token: {rewritten}"
    );
    assert!(rewritten.contains("version"));
}

#[test]
fn auth_manager_status_does_not_consider_legacy_token_configured_without_metadata() {
    // 只有凭据后端有 token、auth.json 不存在时，应报告未配置。
    // 否则一个被删掉的库会因为残留凭据被误判为已登录。
    let dir = temp_dir("orphan-token");
    let backend = Arc::new(MemoryBackend::default());
    backend.set("some-user", "some-token").unwrap();

    let manager = AuthManager::with_credential_backend(dir, backend);
    let status = manager.status().unwrap();
    assert!(!status.configured);
}

#[test]
fn auth_manager_clear_auth_falls_back_to_user_id_hint() {
    let dir = temp_dir("hint");
    let backend = Arc::new(MemoryBackend::default());
    let manager = AuthManager::with_credential_backend(dir.clone(), backend.clone());

    manager.save_auth(&valid_auth()).unwrap();
    // 把 auth.json 写坏，模拟「元数据损坏但 hint 还在」。
    std::fs::write(dir.join("auth.json"), "not json").unwrap();

    manager.clear_auth().unwrap();
    assert!(!dir.join("auth.json").exists());
    assert!(!dir.join("auth.user-id").exists());
    assert_eq!(backend.get("synthetic-user-1").unwrap(), None);
}

#[test]
fn auth_manager_rejects_invalid_inputs_early() {
    let dir = temp_dir("invalid-input");
    let backend = Arc::new(MemoryBackend::default());
    let manager = AuthManager::with_credential_backend(dir, backend);

    let bad_cases = [
        AuthInfo {
            app_token: "ok-token".to_string(),
            user_id: "".to_string(),
            region_host: "https://api-mifit.zepp.com".to_string(),
        },
        AuthInfo {
            app_token: "".to_string(),
            user_id: "ok-user".to_string(),
            region_host: "https://api-mifit.zepp.com".to_string(),
        },
        AuthInfo {
            app_token: "ok-token".to_string(),
            user_id: "ok/user".to_string(),
            region_host: "https://api-mifit.zepp.com".to_string(),
        },
        AuthInfo {
            app_token: "ok-token".to_string(),
            user_id: "ok-user".to_string(),
            region_host: "http://api-mifit.zepp.com".to_string(),
        },
    ];

    for auth in bad_cases {
        assert!(manager.save_auth(&auth).is_err(), "{:?} 应当被拒绝", auth);
    }
}

#[test]
fn auth_info_debug_never_leaks_token() {
    // R1 的回归门。历史 bug：AuthInfo 曾直接 derive(Debug)，app_token 会被
    // format!("{auth:?}")、panic 消息或崩溃报告原样带出去。v2.0.0 改为手写
    // Debug 永久打码（连长度都不给）——这个测试防止未来有人把 derive 加回来。
    let auth = AuthInfo {
        app_token: "super-secret-token-xyz".to_string(),
        user_id: "u1".to_string(),
        region_host: "https://api-mifit.zepp.com".to_string(),
    };
    let debug = format!("{:?}", auth);
    assert!(
        !debug.contains("super-secret-token-xyz"),
        "AuthInfo 的 Debug 输出绝不能包含完整 token；当前输出：{debug}"
    );
    assert!(
        debug.contains("app_token"),
        "打码输出仍应保留字段名以便调试；当前输出：{debug}"
    );
}

#[test]
fn credential_backend_errors_do_not_contain_token_values() {
    let dir = temp_dir("no-token-in-error");
    let backend = Arc::new(FailingBackend("凭据存储故意失败".to_string().leak()));
    let manager = AuthManager::with_credential_backend(dir, backend);

    let leak_token = "leak-check-token";
    let result = manager.save_auth(&AuthInfo {
        app_token: leak_token.to_string(),
        user_id: "u".to_string(),
        region_host: "https://api-mifit.zepp.com".to_string(),
    });

    let error = result.unwrap_err().to_string();
    assert!(
        !error.contains(leak_token),
        "错误信息泄漏了 token: {error}"
    );
}

// ---------------------------------------------------------------------------
// Linux 文件凭据后端边界（只在 unix + 非 macOS 编译）
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
mod file_backend_stress {
    use super::*;
    use zeppbridge_core::auth::FileCredentialBackend;

    #[test]
    fn corrupt_credentials_json_does_not_leak_token() {
        let dir = temp_dir("corrupt-creds");
        // 文件内容前半截是合法 JSON 里的 token，后半截截断；解析失败时不能把它印出来。
        std::fs::write(
            dir.join("credentials.json"),
            r#"{"version":1,"tokens":{"u1":"leaked-from-file-token""#,
        )
        .unwrap();

        let backend = FileCredentialBackend::new(&dir);
        let error = backend.get("u1").unwrap_err();
        assert!(
            !error.contains("leaked-from-file-token"),
            "坏凭据文件解析错误泄漏了 token: {error}"
        );
    }

    #[test]
    fn file_backend_roundtrip_does_not_truncate_long_but_valid_token() {
        let dir = temp_dir("long-token");
        let backend = FileCredentialBackend::new(&dir);
        // 1280 个 UTF-16 码元是系统存储上限；文件存储本身没有这个上限，
        // 但要确保它不会默默截断合法 token。
        let token = "a".repeat(2000);
        backend.set("u", &token).unwrap();
        assert_eq!(backend.get("u").unwrap().as_deref(), Some(token.as_str()));
    }
}

// ---------------------------------------------------------------------------
// 压力级（#[ignore]）
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn region_host_repeated_normalization_is_stable() {
    let raw = "https://API-MIFIT-HUAMI.COM:8443/";
    let mut current = normalize_region_host(raw).unwrap();
    for _ in 0..10_000 {
        let next = normalize_region_host(&current).unwrap();
        assert_eq!(current, next);
        current = next;
    }
}

#[test]
#[ignore]
fn auth_manager_save_load_clear_repeated_is_idempotent() {
    let dir = temp_dir("repeat");
    let backend = Arc::new(MemoryBackend::default());
    let manager = AuthManager::with_credential_backend(dir.clone(), backend);

    for i in 0..1_000 {
        let auth = AuthInfo {
            app_token: format!("token-{i}"),
            user_id: "repeat-user".to_string(),
            region_host: "https://api-mifit.zepp.com".to_string(),
        };
        manager.save_auth(&auth).unwrap();
        let loaded = manager.load_auth().unwrap().unwrap();
        assert_eq!(loaded.app_token, auth.app_token);
        manager.clear_auth().unwrap();
        assert!(!dir.join("auth.json").exists());
    }
}
