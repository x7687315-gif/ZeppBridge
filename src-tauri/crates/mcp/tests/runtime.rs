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
use std::path::{Path, PathBuf};
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

fn db_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("zepp.db")).unwrap_or_default()
}

fn seeded_db(dir: &Path) -> Database {
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

struct McpSession {
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    child: std::process::Child,
}

impl Drop for McpSession {
    fn drop(&mut self) {
        // 测试结束别把子进程留给运行器：杀掉并收割，stdin/stdout 随之关闭。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl McpSession {
    fn new(dir: &Path) -> Self {
        let mut child = Command::new(bin())
            .env(DATA_DIR_ENV, dir.as_os_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法启动 zeppbridge-mcp；integration test 需要先编译 binary");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = child.stderr.take().unwrap();

        // 把 stderr 读进一个线程，避免管道阻塞；测试里不解析它，只看有没有 panic。
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(text) = line else { break };
                assert!(
                    !text.to_ascii_lowercase().contains("panic")
                        && !text.to_ascii_lowercase().contains("thread '")
                        && !text.contains("RUST_BACKTRACE"),
                    "MCP stderr 出现崩溃迹象：{text}"
                );
            }
        });

        Self {
            stdin,
            stdout,
            child,
        }
    }

    fn request(&mut self, id: impl serde::Serialize, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&req).unwrap();
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line).unwrap();
        serde_json::from_str(&response_line)
            .unwrap_or_else(|_| panic!("MCP 返回非 JSON：{response_line}"))
    }

    fn raw_line(&mut self, line: &str) -> Value {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line).unwrap();
        serde_json::from_str(&response_line)
            .unwrap_or_else(|_| panic!("MCP 返回非 JSON：{response_line}"))
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
    assert!(
        resp["result"].is_object(),
        "initialize 应返回 result：{resp}"
    );
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
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools/list 应返回数组");
    assert_eq!(tools.len(), 5);

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for verb in [
            "sync", "delete", "write", "set", "update", "import", "restore",
        ] {
            assert!(!name.contains(verb), "{name} 看起来不是只读工具");
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
    assert!(
        health["result"].is_object(),
        "get_data_health 应成功：{health}"
    );

    let workouts = session.request(
        3,
        "tools/call",
        json!({ "name": "list_workouts", "arguments": { "limit": 5 } }),
    );
    assert!(
        workouts["result"].is_object(),
        "list_workouts 应成功：{workouts}"
    );

    let metric = session.request(
        4,
        "tools/call",
        json!({ "name": "get_metric_series", "arguments": { "metrics": ["heartRate"], "days": 7 } }),
    );
    assert!(
        metric["result"].is_object(),
        "get_metric_series 应成功：{metric}"
    );

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
    // JSON-RPC 信封里的 id 本来就该回显各自的请求 id（2 vs 3），
    // 幂等性比较的是业务载荷 result。
    assert_eq!(
        first["result"], second["result"],
        "同一请求两次的业务结果应完全一致"
    );
}

#[test]
fn unknown_method_and_tool_are_refused() {
    // v2.0.0 的错误码矩阵（V9，BUGS_FOUND.md）：
    // - 未知**方法**（tools/execute）在 handle() 分派层就被拒绝 → 恒 -32601；
    // - 未知**工具**走 tools/call → call_tool 先 open_db() 再匹配工具名，
    //   所以无库时是 -32001（not_configured），有库时才是 -32601。
    // 无库时「拼错工具名」与「还没同步」分不清，是排障语义的小损失；
    // 这里钉住现状，防止无人知晓地再变。
    let dir = temp_dir("unknown");
    let mut session = McpSession::new(&dir);

    session.request(1, "initialize", json!({}));
    let unknown_method = session.request(2, "tools/execute", json!({}));
    assert_eq!(
        unknown_method["error"]["code"], -32601,
        "未知方法在分派层拒绝，与库无关：{unknown_method}"
    );
    let unknown_tool = session.request(
        3,
        "tools/call",
        json!({ "name": "delete_everything", "arguments": {} }),
    );
    assert_eq!(
        unknown_tool["error"]["code"], -32001,
        "无库时未知工具先撞上 open_db（V9）：{unknown_tool}"
    );

    // 有库之后，未知方法与未知工具都必须明确报 method_not_found。
    let dir = temp_dir("unknown-with-db");
    let _db = seeded_db(&dir);
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
    assert!(
        ping["result"].is_object(),
        "收到通知后仍应正常响应 ping：{ping}"
    );
}

// ---------------------------------------------------------------------------
// v2.0.0：双时代协议（2026-07-28 modern 无握手 + legacy initialize）
// ---------------------------------------------------------------------------

const MODERN_VERSION: &str = "2026-07-28";
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

#[test]
fn modern_discover_serves_the_2026_07_28_surface() {
    let dir = temp_dir("modern-discover");
    let mut session = McpSession::new(&dir);

    // modern 客户端不需要 initialize：第一句话就是 server/discover。
    let resp = session.request(
        1,
        "server/discover",
        json!({ "_meta": { META_PROTOCOL_VERSION: MODERN_VERSION } }),
    );
    let result = resp["result"]
        .as_object()
        .expect("server/discover 应返回 result");
    assert_eq!(
        result["resultType"], "complete",
        "modern 结果必须带 resultType"
    );
    assert_eq!(
        result["_meta"][META_SERVER_INFO]["name"], "zeppbridge",
        "身份在 _meta 里，不再是顶层 serverInfo"
    );
    let versions = result["supportedVersions"].as_array().unwrap();
    assert!(
        versions.iter().any(|v| v == MODERN_VERSION),
        "必须声明支持 2026-07-28"
    );
}

#[test]
fn modern_tools_call_stays_read_only_and_carries_result_type() {
    let dir = temp_dir("modern-call");
    let _db = seeded_db(&dir);
    let before = db_bytes(&dir);

    let mut session = McpSession::new(&dir);
    let health = session.request(
        1,
        "tools/call",
        json!({
            "name": "get_data_health",
            "arguments": { "windowDays": 30 },
            "_meta": { META_PROTOCOL_VERSION: MODERN_VERSION },
        }),
    );
    assert!(
        health["result"]["resultType"] == "complete",
        "modern tools/call 结果必须带 resultType=complete：{health}"
    );

    let after = db_bytes(&dir);
    assert_eq!(before, after, "modern 路径同样必须只读");
}

#[test]
fn mixing_eras_is_refused_not_silently_absorbed() {
    let dir = temp_dir("mixed-era");
    let mut session = McpSession::new(&dir);

    // modern 客户端发 initialize：这一版已经没有握手，必须明确拒绝。
    let resp = session.request(
        1,
        "initialize",
        json!({ "_meta": { META_PROTOCOL_VERSION: MODERN_VERSION } }),
    );
    assert_eq!(resp["error"]["code"], -32601, "混用两个时代要被说清楚");
}

#[test]
fn unsupported_protocol_version_lists_supported_ones() {
    let dir = temp_dir("bad-version");
    let mut session = McpSession::new(&dir);

    let resp = session.request(
        1,
        "tools/list",
        json!({ "_meta": { META_PROTOCOL_VERSION: "1999-01-01" } }),
    );
    let error = resp["error"].as_object().expect("不支持的版本应报错");
    assert_eq!(
        error["code"], -32022,
        "2026-07-28 的 UnsupportedProtocolVersionError"
    );
    let supported = error["data"]["supported"]
        .as_array()
        .expect("错误必须带支持列表");
    assert!(supported.iter().any(|v| v == MODERN_VERSION));
}
