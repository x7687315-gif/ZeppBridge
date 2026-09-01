//! MCP server 运行时测试：stdio JSON-RPC 完整握手 + 只读边界。
//!
//! 这里启动真实 `zeppbridge-mcp` 二进制，覆盖：
//! - initialize / tools/list / tools/call 正常响应；
//! - 没有数据库时返回 -32001（ERR_NOT_CONFIGURED），而不是 panic；
//! - 坏 JSON 行不导致进程崩溃，且回一个带 null id 的解析错误；
//! - 工具调用前后 SQLite 主库字节完全一致（只读承诺）；
//! - 同一请求连续两次返回相同结果（幂等）。
//!
//! 运行：`cargo test -p zeppbridge-mcp --test runtime -- --nocapture`

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use zeppbridge_core::models::{CapabilityStatus, RawRecord, SourceScope};
use zeppbridge_core::storage::Database;

const DATA_DIR_ENV: &str = "ZEPPBRIDGE_DATA_DIR";

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "zb-mcp-runtime-{}-{}-{}",
        label,
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_zeppbridge-mcp")
}

fn db_bytes(dir: &PathBuf) -> Vec<u8> {
    std::fs::read(dir.join("zepp.db")).unwrap_or_default()
}

fn seeded_db(dir: &PathBuf) -> Database {
    let db = Database::open_migrated(&dir.join("zepp.db")).unwrap();
    db.persist_fetched_record(&RawRecord {
        stream: "heart_rate".to_string(),
        source_key: "2026-01-01".to_string(),
        source_scope: SourceScope::UserFused,
        device_id: None,
        start_utc: chrono::DateTime::from_timestamp(1_706_784_000, 0).unwrap(),
        end_utc: None,
        payload: serde_json::json!({ "items": [{ "timestamp": 1706784000, "value": 72 }] }),
        capability: CapabilityStatus::Verified,
    })
    .unwrap();
    db
}

struct McpSession {
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn new(dir: &PathBuf) -> Self {
        let mut child = Command::new(bin())
            .env(DATA_DIR_ENV, dir.as_os_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法启动 zeppbridge-mcp；integration test 需要先编译 binary");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        // 把 stderr 读进一个线程，避免管道阻塞；测试里不解析它，只看有没有 panic。
        std::thread::spawn(move || {
            let reader = BufReader::new(child.stderr.take().unwrap());
            for line in reader.lines() {
                if let Ok(text) = line {
                    assert!(
                        !text.to_ascii_lowercase().contains("panic")
                            && !text.to_ascii_lowercase().contains("thread '")
                            && !text.contains("RUST_BACKTRACE"),
                        "MCP stderr 出现崩溃迹象：{text}"
                    );
                }
            }
        });

        Self { stdin, stdout }
    }

    fn request(&mut self, id: impl serde::Serialize, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&req).unwrap();
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line).unwrap();
        serde_json::from_str(&response_line).unwrap_or_else(|_| {
            panic!("MCP 返回非 JSON：{response_line}")
        })
    }

    fn raw_line(&mut self, line: &str) -> Value {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line).unwrap();
        serde_json::from_str(&response_line).unwrap_or_else(|_| {
            panic!("MCP 返回非 JSON：{response_line}")
        })
    }
}

// ---------------------------------------------------------------------------
// 基本协议
// ---------------------------------------------------------------------------

#[test]
fn initialize_returns_boundary_and_contract() {
    let dir = temp_dir("init");
    let mut session = McpSession::new(&dir);

    let resp = session.request(1, "initialize", json!({}));
    assert!(resp["result"].is_object(), "initialize 应返回 result：{resp}");
    assert_eq!(resp["result"]["serverInfo"]["name"], "zeppbridge");
    let instructions = resp["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("不会用 0") || instructions.contains("不会补 0"));
    assert!(instructions.contains("不监听端口"));
}

#[test]
fn tools_list_declares_five_read_only_tools() {
    let dir = temp_dir("tools");
    let mut session = McpSession::new(&dir);

    session.request(1, "initialize", json!({}));
    let resp = session.request(2, "tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools/list 应返回数组");
    assert_eq!(tools.len(), 5);

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for verb in ["sync", "delete", "write", "set", "update", "import", "restore"] {
            assert!(
                !name.contains(verb),
                "{name} 看起来不是只读工具"
            );
        }
    }
}

#[test]
fn tools_call_without_database_returns_not_configured() {
    let dir = temp_dir("no-db");
    let mut session = McpSession::new(&dir);

    session.request(1, "initialize", json!({}));
    let resp = session.request(
        2,
        "tools/call",
        json!({ "name": "list_workouts", "arguments": { "limit": 5 } }),
    );
    assert!(resp["error"].is_object(), "无数据库时应返回 error：{resp}");
    assert_eq!(resp["error"]["code"], -32001);
}

#[test]
fn tools_call_with_database_is_read_only() {
    let dir = temp_dir("readonly");
    let _db = seeded_db(&dir);
    let before = db_bytes(&dir);

    let mut session = McpSession::new(&dir);
    session.request(1, "initialize", json!({}));

    let health = session.request(
        2,
        "tools/call",
        json!({ "name": "get_data_health", "arguments": { "windowDays": 30 } }),
    );
    assert!(health["result"].is_object(), "get_data_health 应成功：{health}");

    let workouts = session.request(
        3,
        "tools/call",
        json!({ "name": "list_workouts", "arguments": { "limit": 5 } }),
    );
    assert!(workouts["result"].is_object(), "list_workouts 应成功：{workouts}");

    let metric = session.request(
        4,
        "tools/call",
        json!({ "name": "get_metric_series", "arguments": { "metrics": ["heartRate"], "days": 7 } }),
    );
    assert!(metric["result"].is_object(), "get_metric_series 应成功：{metric}");

    let after = db_bytes(&dir);
    assert_eq!(
        before, after,
        "MCP 工具调用不应修改 SQLite 主库（只读承诺被破坏）"
    );
}

#[test]
fn bad_json_line_does_not_crash_and_returns_parse_error() {
    let dir = temp_dir("bad-json");
    let mut session = McpSession::new(&dir);

    let resp = session.raw_line("this is not json {{");
    assert!(resp["error"].is_object(), "坏 JSON 应返回 error：{resp}");
    assert_eq!(resp["error"]["code"], -32700);
    assert!(resp["id"].is_null());

    // 进程应当继续服务后续合法请求。
    let ok = session.request(1, "initialize", json!({}));
    assert!(ok["result"].is_object(), "坏 JSON 后进程应继续服务：{ok}");
}

#[test]
fn repeated_identical_requests_are_idempotent() {
    let dir = temp_dir("idempotent");
    let _db = seeded_db(&dir);
    let mut session = McpSession::new(&dir);

    session.request(1, "initialize", json!({}));
    let first = session.request(2, "tools/list", json!({}));
    let second = session.request(3, "tools/list", json!({}));
    assert_eq!(first, second, "同一请求两次结果应完全一致");
}

#[test]
fn unknown_method_and_tool_are_refused() {
    let dir = temp_dir("unknown");
    let mut session = McpSession::new(&dir);

    session.request(1, "initialize", json!({}));
    let unknown_method = session.request(2, "tools/execute", json!({}));
    assert_eq!(unknown_method["error"]["code"], -32601);

    let unknown_tool = session.request(
        3,
        "tools/call",
        json!({ "name": "delete_everything", "arguments": {} }),
    );
    assert_eq!(unknown_tool["error"]["code"], -32601);
}

#[test]
fn notification_without_id_is_silently_ignored() {
    let dir = temp_dir("notification");
    let mut session = McpSession::new(&dir);

    // notifications/initialized 没有 id，按协议不应回复。
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let line = serde_json::to_string(&note).unwrap();
    session.stdin.write_all(line.as_bytes()).unwrap();
    session.stdin.write_all(b"\n").unwrap();
    session.stdin.flush().unwrap();

    // 给进程一点时间处理，然后发一个有 id 的 ping。
    std::thread::sleep(std::time::Duration::from_millis(50));
    let ping = session.request(1, "ping", json!({}));
    assert!(ping["result"].is_object(), "收到通知后仍应正常响应 ping：{ping}");
}
