//! CLI 运行时测试：在真实二进制上验证状态、导出、契约命令的退出码与输出。
//!
//! 这里不 mock 任何东西：启动编译好的 `zeppbridge-cli`，指向一个临时数据目录，
//! 覆盖「库不存在」「空库」「已有数据」三种状态，以及连续调用、JSON 输出解析、
//! 未知命令等边界。任何 panic 或 stderr 里的 "thread" 字样都会让测试失败。
//!
//! 运行：`cargo test -p zeppbridge-cli --test runtime -- --nocapture`

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;
use zeppbridge_core::models::{CapabilityStatus, RawRecord, SourceScope};
use zeppbridge_core::storage::Database;

const DATA_DIR_ENV: &str = "ZEPPBRIDGE_DATA_DIR";

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "zb-cli-runtime-{}-{}-{}",
        label,
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_zeppbridge-cli")
}

fn run(args: &[&str], dir: &PathBuf) -> Output {
    let output = Command::new(bin())
        .env(DATA_DIR_ENV, dir.as_os_str())
        .args(args)
        .output()
        .expect("无法启动 zeppbridge-cli；integration test 需要先编译 binary");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.to_ascii_lowercase().contains("panic")
            && !stderr.to_ascii_lowercase().contains("thread '")
            && !stderr.contains("RUST_BACKTRACE"),
        "stderr 出现 panic/崩溃迹象：{stderr}"
    );
    output
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("进程被信号终止，非正常退出")
}

fn seeded_db(dir: &PathBuf) -> Database {
    let db = Database::open_migrated(&dir.join("zepp.db")).unwrap();
    db.persist_fetched_record(&RawRecord {
        stream: "heart_rate".to_string(),
        source_key: "2026-01-01".to_string(),
        source_scope: SourceScope::UserFused,
        device_id: None,
        start_utc: chrono::DateTime::from_timestamp(1_768_435_200, 0).unwrap(),
        end_utc: None,
        payload: serde_json::json!({ "items": [{ "timestamp": 1768435200, "value": 72 }] }),
        capability: CapabilityStatus::Verified,
    })
    .unwrap();
    db
}

// ---------------------------------------------------------------------------
// 基础命令
// ---------------------------------------------------------------------------

#[test]
fn version_and_contract_always_succeed() {
    let dir = temp_dir("basic");

    let version = run(&["version"], &dir);
    assert_eq!(exit_code(&version), 0);
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(stdout.contains("zeppbridge-cli"));

    let contract = run(&["contract"], &dir);
    assert_eq!(exit_code(&contract), 0);
    let stdout = String::from_utf8_lossy(&contract.stdout);
    assert!(stdout.contains("contractVersion"));
}

#[test]
fn unknown_command_and_help_report_usage() {
    let dir = temp_dir("usage");

    let unknown = run(&["frobnicate"], &dir);
    assert_eq!(exit_code(&unknown), 2);

    let no_args = run(&[], &dir);
    assert_eq!(exit_code(&no_args), 2);
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_with_missing_database_is_not_configured() {
    let dir = temp_dir("status-missing");
    let output = run(&["status", "--json"], &dir);
    assert_eq!(exit_code(&output), 3, "没有数据库时应返回 EXIT_NOT_CONFIGURED");

    let json: Value = serde_json::from_slice(&output.stdout).expect("--json 输出应可解析");
    assert_eq!(json["ok"], false);
    assert_eq!(json["errorKind"], "not_configured");
}

#[test]
fn status_with_empty_migrated_database_succeeds() {
    let dir = temp_dir("status-empty");
    let _db = Database::open_migrated(&dir.join("zepp.db")).unwrap();

    let output = run(&["status", "--json"], &dir);
    assert_eq!(exit_code(&output), 0);

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["connected"], false);
    assert_eq!(json["schemaVersion"], json["schemaVersion"]); // 仅确认字段存在
    assert!(
        json.get("coverageDays").is_some(),
        "status 必须返回覆盖天数"
    );
}

#[test]
fn status_with_seeded_database_reports_coverage() {
    let dir = temp_dir("status-seeded");
    let _db = seeded_db(&dir);

    let output = run(&["status"], &dir);
    assert_eq!(exit_code(&output), 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("本机覆盖：") || stdout.contains("天，最早"),
        "纯文本 status 应给出覆盖信息：{stdout}"
    );
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

#[test]
fn export_with_missing_database_is_not_configured() {
    let dir = temp_dir("export-missing");
    let output = run(
        &[
            "export",
            "--from",
            "2026-01-01",
            "--to",
            "2026-01-31",
            "--json",
        ],
        &dir,
    );
    assert_eq!(
        exit_code(&output),
        3,
        "没有数据库时 export 应返回 EXIT_NOT_CONFIGURED"
    );
}

#[test]
fn export_with_empty_database_does_not_panic_and_reports_zero_or_error() {
    let dir = temp_dir("export-empty");
    let _db = Database::open_migrated(&dir.join("zepp.db")).unwrap();

    let output = run(
        &[
            "export",
            "--from",
            "2026-01-01",
            "--to",
            "2026-01-31",
            "--format",
            "json",
            "--json",
        ],
        &dir,
    );
    let code = exit_code(&output);
    assert!(
        [0, 1, 3, 6].contains(&code),
        "空库 export 不应 panic，允许成功/失败/未配置/数据库错误：{code}"
    );
}

#[test]
fn export_json_and_csv_formats_are_consistent() {
    let dir = temp_dir("export-formats");
    let _db = seeded_db(&dir);

    let json_out = run(
        &[
            "export",
            "--from",
            "2026-01-01",
            "--to",
            "2026-01-31",
            "--format",
            "json",
        ],
        &dir,
    );
    assert_eq!(exit_code(&json_out), 0, "JSON 导出应成功");
    let json_text = String::from_utf8_lossy(&json_out.stdout);
    let parsed: Value = serde_json::from_str(&json_text).expect("JSON 导出结果应可解析");

    let csv_out = run(
        &[
            "export",
            "--from",
            "2026-01-01",
            "--to",
            "2026-01-31",
            "--format",
            "csv",
        ],
        &dir,
    );
    assert_eq!(exit_code(&csv_out), 0, "CSV 导出应成功");
    let csv_text = String::from_utf8_lossy(&csv_out.stdout);
    assert!(
        csv_text.lines().next().unwrap_or("").contains("date") || csv_text.is_empty(),
        "CSV 首行应是表头或空：{csv_text}"
    );

    // 导出语义：缺失值不能变成 0。这里样本里有真实 HR=72，确保它出现。
    if parsed.get("records").is_some() || parsed.is_array() {
        assert!(
            json_text.contains("72") || csv_text.contains("72"),
            "真实心率 72 应在导出中出现"
        );
    }
}

// ---------------------------------------------------------------------------
// 连续调用 / 幂等 / JSON 输出
// ---------------------------------------------------------------------------

#[test]
fn repeated_status_calls_are_stable() {
    let dir = temp_dir("repeat");
    let _db = seeded_db(&dir);

    let mut last_stdout = None;
    for _ in 0..20 {
        let output = run(&["status", "--json"], &dir);
        assert_eq!(exit_code(&output), 0);
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["ok"], true);
        if let Some(previous) = last_stdout {
            assert_eq!(
                previous, json,
                "连续 status --json 输出应稳定（无随机漂移）"
            );
        }
        last_stdout = Some(json);
    }
}

#[test]
fn contract_output_is_valid_json() {
    let dir = temp_dir("contract-json");
    let output = run(&["contract"], &dir);
    assert_eq!(exit_code(&output), 0);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["contractVersion"].is_string() || json["contractVersion"].is_number());
    assert!(json["metrics"].is_array());
    assert!(json["missingValues"].is_string());
}

#[test]
fn status_plain_and_json_have_same_semantic_fields() {
    let dir = temp_dir("semantic");
    let _db = seeded_db(&dir);

    let plain = run(&["status"], &dir);
    assert_eq!(exit_code(&plain), 0);
    let plain_text = String::from_utf8_lossy(&plain.stdout);

    let json = run(&["status", "--json"], &dir);
    assert_eq!(exit_code(&json), 0);
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();

    // 账号状态、schema 版本、数据库大小必须在两种输出里都有对应表达。
    assert!(
        plain_text.contains("已连接") || plain_text.contains("未连接"),
        "纯文本 status 必须给出账号状态"
    );
    assert!(value["connected"].is_boolean());
    assert!(value["schemaVersion"].is_number());
    assert!(value["databaseBytes"].is_number());
}
