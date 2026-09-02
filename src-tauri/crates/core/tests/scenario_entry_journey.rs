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
//! ## 两个容易踩的分层陷阱（本文件曾因这两点全红）
//!
//! 1. **stream 名 ≠ series 指标名**。`persist_fetched_record` 的 stream 是同步层
//!    的概念（`heart_rate`、`hrv` 都合法）；而 `metric_series` 只暴露白名单里的
//!    指标（storage/mod.rs 的 `SERIES_METRICS` / `SAMPLE_ONLY_SERIES_METRICS`），
//!    其中按**样本折叠成日序列**的是 `hrv`。原始 `heart_rate` 样本能进库、能导出，
//!    但不作为日序列指标——用它查 `metric_series` 只会拿到空 Vec。
//! 2. **查询窗口锚定 `Local::now()`**。`metric_series` 的窗口是
//!    `[今天-(days-1), 今天]`，所以播种日期必须**相对今天**计算；写死日历日期
//!    会在几天后静默地全部落到窗口外，测试变成「0 个点」的假通过/假失败。
//!
//! 运行：`cargo test -p zeppbridge-core --test scenario_entry_journey -- --nocapture`

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{Local, TimeZone};
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

/// `days_ago` 天前当地零点的 unix 秒。
///
/// 序列视图按 `date(timestamp, 'localtime')` 分组，所以播种时刻必须对齐到
/// 当地零点，否则一天 12 条样本会跨午夜被拆成两天。
fn local_midnight(days_ago: i64) -> i64 {
    let day = Local::now().date_naive() - chrono::Duration::days(days_ago);
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .unwrap()
        .timestamp()
}

/// `days_ago` 天前的当地日期（`YYYY-MM-DD`），用于导出范围与空档比对。
fn local_date(days_ago: i64) -> String {
    (Local::now().date_naive() - chrono::Duration::days(days_ago))
        .format("%Y-%m-%d")
        .to_string()
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

/// 一天的小时级样本（每天 10 点到 21 点，共 12 条，值 60..100）。
///
/// 走 `hrv` 这条流：它是 `metric_series` 白名单里**按样本折叠**的指标，
/// 因此播种的 12 条读数会折叠成当天一个点，`samples == Some(12)`。
fn sample_day(day_start_unix: i64) -> Value {
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
    let day_start = local_midnight(0);
    db.persist_fetched_record(&fetched(
        "hrv",
        &format!("hrv:{day_start}"),
        sample_day(day_start),
    ))
    .unwrap();

    let series = db
        .metric_series(&["hrv".to_string()], 3)
        .unwrap()
        .into_iter()
        .find(|s| s.metric == "hrv")
        .expect("hrv 序列必须存在");
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
        scope: Some(ExportScope::date_range(&local_date(1), &local_date(0))),
        start_date: None,
        end_date: None,
        data_types: vec!["hrv".to_string()],
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
    let _again = readonly.metric_series(&["hrv".to_string()], 3).unwrap();
    drop(readonly);
    let after = std::fs::read(dir.path.join("zepp.db")).unwrap();
    assert_eq!(before, after, "只读连接不得修改主库");

    // 6. 退出应用再打开（用户晚上又点开了一次）：数据还在。
    drop(db);
    let db = Database::open_migrated(&dir.path.join("zepp.db")).unwrap();
    let series = db.metric_series(&["hrv".to_string()], 3).unwrap();
    let hrv = series.iter().find(|s| s.metric == "hrv").unwrap();
    assert_eq!(hrv.points.len(), 1, "重进应用后数据还在");
    assert_eq!(hrv.points[0].samples, Some(12));
}

// ---------------------------------------------------------------------------
// S4-lite · 出差两周没戴表
// ---------------------------------------------------------------------------

#[test]
fn s4_unworn_gap_days_do_not_exist_and_empty_is_not_failed() {
    let dir = TempDirGuard::new("s4-gap");
    let db = Database::open_migrated(&dir.path.join("zepp.db")).unwrap();

    // 22 天窗口里：最老的 5 天（21..17 天前）戴表，中间 12 天（16..5 天前）
    // 空档，最近 5 天（4..0 天前）又戴了。日期相对今天算，见文件头说明。
    let worn_offsets = [21i64, 20, 19, 18, 17, 4, 3, 2, 1, 0];
    for &offset in &worn_offsets {
        let day_start = local_midnight(offset);
        db.persist_fetched_record(&fetched(
            "hrv",
            &format!("hrv:{day_start}"),
            sample_day(day_start),
        ))
        .unwrap();
    }

    // 序列视图：10 天有数据，22 天的窗口里其余 12 天必须**不存在**，
    // 绝不允许出现 0 值或补出的点。
    let series = db
        .metric_series(&["hrv".to_string()], 22)
        .unwrap()
        .into_iter()
        .find(|s| s.metric == "hrv")
        .expect("hrv 序列必须存在");
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
    // 空档天必须**不在** points 里——不是 value = 0，是压根没有这个点。
    let present: BTreeSet<&str> = series.points.iter().map(|p| p.date.as_str()).collect();
    for offset in 5..=16i64 {
        let gap = local_date(offset);
        assert!(
            !present.contains(gap.as_str()),
            "空档天 {gap} 不该出现在序列里（实际点位: {present:?}）"
        );
    }

    // 导出视图：空档窗口里一条样本都没有。
    let selection = ExportSelection {
        scope: Some(ExportScope::date_range(&local_date(21), &local_date(0))),
        start_date: None,
        end_date: None,
        data_types: vec!["hrv".to_string()],
        detail: ExportDetail::Full,
    };
    let (encoded, _) = db.build_ai_export(&selection).unwrap();
    let export = serde_json::from_str::<Value>(&encoded).unwrap();
    let samples = export["data"]["metric_samples"].as_array().unwrap();
    assert_eq!(samples.len(), 10 * 12, "只应有戴表日的 120 条样本");
    let expected_days: BTreeSet<String> =
        worn_offsets.iter().map(|&offset| local_date(offset)).collect();
    for sample in samples {
        let timestamp = sample["timestamp"].as_str().unwrap();
        let day = &timestamp[..10];
        assert!(
            expected_days.contains(day),
            "空档期出现了样本: {timestamp}（期望日期: {expected_days:?}）"
        );
    }

    // 账本视图：这一段用的是真正的补拉流 `heart_rate`，与上面播种的
    // `hrv` 无关——记录补拉结果不需要先有数据。
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
