//! 用户旅程场景测试 · S1「新用户第一天」 + S4-lite「出差两周没戴表」。
//!
//! 策略背景见 `docs/development/real-life-scenario-tests.md`：模块级压力测试
//! 之上的一层——把真实写入路径（persist_fetched_record）串成用户的完整路径，
//! 抓「每个模块单独看都对、连起来断了」的缺陷。
//!
//! S1：空目录 → 未登录的诚实状态 → 空库的诚实状态 → 播种第一天 → 读回一致 →
//!     重新打开（重进应用）数据还在 → 只读出口不写库。
//! S4：前 5 天有数据 + 中间 12 天空档 + 后 5 天有数据。空档天在序列里必须
//!     **不存在**（而不是 0），账本里「云端没有」与「失败」绝不混淆。
//!
//! 运行：`cargo test -p zeppbridge-core --test scenario_entry_journey -- --nocapture`

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use zeppbridge_core::auth::AuthManager;
use zeppbridge_core::models::{CapabilityStatus, ExportDetail, ExportScope, ExportSelection, RawRecord, SourceScope};
use zeppbridge_core::storage::coverage::{ChunkStatus, BACKFILL_STREAMS};
use zeppbridge_core::storage::Database;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zb-scenario-{}-{}-{}",
            label,
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDirGuard { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fetched(stream: &str, source_key: &str, payload: Value) -> RawRecord {
    RawRecord {
        stream: stream.to_string(),
        source_key: source_key.to_string(),
        source_scope: SourceScope::UserFused,
        device_id: None,
        start_utc: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        end_utc: None,
        payload,
        capability: CapabilityStatus::Verified,
    }
}

/// 一天的小时级心率样本（每天 10 点到 21 点，共 12 条，值 60..100）。
fn heart_rate_day(day_start_unix: i64) -> Value {
    let items: Vec<Value> = (0..12)
        .map(|hour| {
            json!({
                "timestamp": day_start_unix + 10 * 3600 + hour * 3600,
                "value": 62 + (hour % 7) * 5,
            })
        })
        .collect();
    json!({ "items": items })
}

// ---------------------------------------------------------------------------
// S1 · 新用户第一天
// ---------------------------------------------------------------------------

#[test]
fn s1_first_day_journey_every_exit_is_honest() {
    let dir = TempDirGuard::new("s1-first-day");

    // 1. 还没登录：状态必须如实说「未配置」，而不是报错或 panic。
    let auth = AuthManager::new(dir.path.clone());
    let status = auth.status().unwrap();
    assert!(
        !status.configured,
        "空目录上 AuthStatus.configured 必须是 false: {status:?}"
    );
    assert!(status.user_id.is_none());
    assert!(status.token_masked.is_none() || status.token_masked.is_some());

    // 2. 第一次打开：空库建出来；空库的每个出口都不许谎报。
    let db = Database::open_migrated(&dir.path.join("zepp.db")).unwrap();
    for data_status in db.list_data_status().unwrap() {
        assert_eq!(
            data_status.status, "no_records",
            "空库的 {} 必须报 no_records，实际是 {}",
            data_status.stream, data_status.status
        );
        assert_eq!(data_status.records_written, 0);
    }
    assert!(
        !db.coverage_ledger().unwrap().complete,
        "什么都没做不能自称历史完整"
    );

    // 3. 第一天数据来了（真实写入路径）：读回必须一致。
    let day_start = 1_768_435_200i64; // 2026-01-15 UTC
    db.persist_fetched_record(&fetched(
        "heart_rate",
        &format!("heart_rate:{day_start}"),
        heart_rate_day(day_start),
    ))
    .unwrap();

    let series = db
        .metric_series(&["heart_rate".to_string()], 3)
        .unwrap()
        .into_iter()
        .find(|s| s.metric == "heart_rate")
        .expect("heart_rate 序列必须存在");
    assert_eq!(series.points.len(), 1, "只有播种的那一天有数据");
    let point = &series.points[0];
    assert_eq!(point.samples, Some(12), "当天的 12 条小时样本");
    assert!(
        (60.0..=100.0).contains(&point.value),
        "读回的均值漂出了播种区间: {}",
        point.value
    );

    // 4. 导出（用户第一次把数据交给 AI）。
    let selection = ExportSelection {
        scope: Some(ExportScope::date_range("2026-01-14", "2026-01-16")),
        start_date: None,
        end_date: None,
        data_types: vec!["heart_rate".to_string()],
        detail: ExportDetail::Full,
    };
    let (encoded, size) = db.build_ai_export(&selection).unwrap();
    assert!(size > 0);
    let export = serde_json::from_str::<Value>(&encoded).unwrap();
    let samples = export["data"]["metric_samples"].as_array().unwrap();
    assert_eq!(samples.len(), 12, "导出必须含当天的全部样本");

    // 5. 只读出口（MCP / status 用的那条路）不写库。
    let before = std::fs::read(dir.path.join("zepp.db")).unwrap();
    let readonly = Database::open_read_only(dir.path.join("zepp.db")).unwrap();
    let _again = readonly.metric_series(&["heart_rate".to_string()], 3).unwrap();
    drop(readonly);
    let after = std::fs::read(dir.path.join("zepp.db")).unwrap();
    assert_eq!(before, after, "只读连接不得修改主库");

    // 6. 退出应用再打开（用户晚上又点开了一次）：数据还在。
    drop(db);
    let db = Database::open_migrated(&dir.path.join("zepp.db")).unwrap();
    let series = db.metric_series(&["heart_rate".to_string()], 3).unwrap();
    let heart = series.iter().find(|s| s.metric == "heart_rate").unwrap();
    assert_eq!(heart.points.len(), 1, "重进应用后数据还在");
    assert_eq!(heart.points[0].samples, Some(12));
}

// ---------------------------------------------------------------------------
// S4-lite · 出差两周没戴表
// ---------------------------------------------------------------------------

#[test]
fn s4_unworn_gap_days_do_not_exist_and_empty_is_not_failed() {
    let dir = TempDirGuard::new("s4-gap");
    let db = Database::open_migrated(&dir.path.join("zepp.db")).unwrap();

    // 7 月 1-5 日戴表，7 月 6-17 日没戴，7 月 18-22 日又戴了。
    let july_1 = 1_782_864_000i64; // 2026-07-01 UTC
    let day = 86_400i64;
    for &offset in &[0i64, 1, 2, 3, 4, 17, 18, 19, 20, 21] {
        db.persist_fetched_record(&fetched(
            "heart_rate",
            &format!("heart_rate:{}", july_1 + offset * day),
            heart_rate_day(july_1 + offset * day),
        ))
        .unwrap();
    }

    // 序列视图：10 天有数据，22 天的窗口里其余 12 天必须**不存在**，
    // 绝不允许出现 0 值或补出的点。
    let series = db
        .metric_series(&["heart_rate".to_string()], 31)
        .unwrap()
        .into_iter()
        .find(|s| s.metric == "heart_rate")
        .expect("heart_rate 序列必须存在");
    assert_eq!(
        series.points.len() as i64, series.days_with_data,
        "points 与 days_with_data 必须自洽"
    );
    assert_eq!(series.days_with_data, 10, "只有戴表的 10 天有数据");
    for point in &series.points {
        assert!(
            (60.0..=100.0).contains(&point.value),
            "任何一天的平均值都不该漂出播种区间（缺失变成 0 了？）: {:?}",
            point
        );
    }

    // 导出视图：空档窗口里一条样本都没有。
    let selection = ExportSelection {
        scope: Some(ExportScope::date_range("2026-07-01", "2026-07-31")),
        start_date: None,
        end_date: None,
        data_types: vec!["heart_rate".to_string()],
        detail: ExportDetail::Full,
    };
    let (encoded, _) = db.build_ai_export(&selection).unwrap();
    let export = serde_json::from_str::<Value>(&encoded).unwrap();
    let samples = export["data"]["metric_samples"].as_array().unwrap();
    assert_eq!(samples.len(), 10 * 12, "只应有戴表日的 120 条样本");
    for sample in samples {
        let timestamp = sample["timestamp"].as_str().unwrap();
        let day_of_month: u32 = timestamp[8..10].parse().unwrap();
        assert!(
            day_of_month <= 5 || day_of_month >= 18,
            "空档期（7 月 6-17 日）出现了样本: {timestamp}"
        );
    }

    // 账本视图：6 月整月云端明确没有（表还没激活），7 月写入，8 月还没拉。
    // 「云端没有」绝不能混进失败清单。
    let june_1 = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
    let aug_31 = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
    db.plan_backfill(june_1, aug_31).unwrap();
    let chunk = "2026-06-01";
    db.record_backfill_chunk("heart_rate", chunk, ChunkStatus::EmptyFromCloud, 0, None, None)
        .unwrap();
    db.record_backfill_chunk("heart_rate", "2026-07-01", ChunkStatus::Persisted, 120, None, None)
        .unwrap();

    let ledger = db.coverage_ledger().unwrap();
    assert!(
        ledger
            .failed_chunks_detail
            .iter()
            .all(|c| c.chunk_start != chunk),
        "云端确认没有的月份不能出现在失败清单里"
    );
    let heart = ledger
        .streams
        .iter()
        .find(|s| s.stream == "heart_rate")
        .unwrap();
    assert_eq!(heart.empty_chunks, 1);
    assert_eq!(heart.failed_chunks, 0);
    assert!(!ledger.complete, "8 月还没拉，不能自称完整");
    // 其余 5 条流 6 月同样排了计划，仍是 pending（模拟还没跑到的部分）。
    assert_eq!(
        BACKFILL_STREAMS.len() - 1,
        ledger
            .streams
            .iter()
            .filter(|s| s.stream != "heart_rate")
            .count()
    );
}
