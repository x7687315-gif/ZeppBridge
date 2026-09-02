//! export_formats / contract 运行时强度测试。
//!
//! 对同一大数据集反复导出 JSON/CSV/GPX，验证：
//! - 多次导出的**关键语义**一致（缺失值语义、数值语义不得漂移；
//!   generated_at 之类的时间戳允许不同）；
//! - 缺失值绝不出现在导出里变成 0；
//! - 大文件导出可完成且可再解析；
//! - 没有可输出内容时返回 Err 而不是空文件；
//! - contract 的指标枚举与单位映射稳定。
//!
//! 运行：
//! - 日常级：`cargo test -p zeppbridge-core --test export_contract_stress`
//! - 压力级：`cargo test -p zeppbridge-core --test export_contract_stress -- --ignored --nocapture`
//!
//! 播种只走公开 API（persist_fetched_record / replace_workout_series），
//! 与 GUI/CLI 实际写入路径完全相同。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use zeppbridge_core::decoder::decode_workout_detail;
use zeppbridge_core::export_formats::{to_csv, to_gpx};
use zeppbridge_core::models::{
    CapabilityStatus, ExportDetail, ExportScope, ExportSelection, RawRecord, SourceScope,
};
use zeppbridge_core::storage::Database;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zb-export-stress-{}-{}-{}",
            label,
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDirGuard { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("zepp.db")
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

/// 造一个有真实数据的库：
/// - heart_rate：N 条逐分钟样本（走真实归一化路径写入）；
/// - workouts：一条 30 分钟跑步（trackid 作为 workout_id）+ 合成 GPS 轨迹与逐秒样本。
fn seeded_db(label: &str, heart_minutes: usize) -> Database {
    let dir = TempDirGuard::new(label);
    let db = Database::open_migrated(&dir.db_path()).unwrap();
    // TempDirGuard 在 drop 时删除目录；把目录生命周期泄漏掉（进程级临时目录，
    // 测试进程退出后由系统清理），因为 Database 要持有到测试结束。
    std::mem::forget(dir);

    let items: Vec<Value> = (0..heart_minutes)
        .map(|minute| {
            json!({
                "timestamp": 1_700_000_000i64 + (minute as i64) * 60,
                "value": 60.0 + (minute % 30) as f64,
            })
        })
        .collect();
    db.persist_fetched_record(&fetched(
        "heart_rate",
        "hr-batch-1",
        json!({ "items": items }),
    ))
    .unwrap();

    // 一条运动：type=1（跑步），起止各 30 分钟。
    let track_id = 1_700_100_000i64;
    db.persist_fetched_record(&fetched(
        "workouts",
        "workouts-batch-1",
        json!({ "items": [ {
            "trackid": track_id,
            "type": 1,
            "start_time": track_id,
            "end_time": track_id + 1800,
        }]}),
    ))
    .unwrap();

    // 合成 GPS 轨迹 + 心率序列，与 storage 真实写入路径相同。
    let mut time = String::from("0;");
    let mut coords = String::new();
    let mut heart = String::new();
    let mut distance = String::new();
    for second in 0..1800usize {
        if second > 0 {
            time.push_str("1;");
            coords.push_str("1,1;");
            distance.push_str(&format!("1,{};", second * 33));
        } else {
            coords.push_str("4004663552,11629333504;");
            distance.push_str("0,0;");
        }
        heart.push_str(&format!("{},{};", if second == 0 { 0 } else { 1 }, 120 + second % 40));
    }
    let detail = json!({ "data": {
        "trackid": track_id,
        "time": time,
        "longitude_latitude": coords,
        "altitude": "-2000000;1500;1502;",
        "heart_rate": heart,
        "currentDistance": distance,
    }});
    let decoded = decode_workout_detail(&detail, None).unwrap();
    db.replace_workout_series(&track_id.to_string(), &decoded)
        .unwrap();
    db
}

fn make_selection(types: &[&str], detail: ExportDetail) -> ExportSelection {
    ExportSelection {
        scope: Some(ExportScope::date_range("2023-11-01", "2023-11-30")),
        start_date: None,
        end_date: None,
        data_types: types.iter().map(|value| value.to_string()).collect(),
        detail,
    }
}

/// 把导出 JSON 里允许轮次间变化的字段剥掉，剩下的必须逐字节一致。
fn semantic_view(export: &Value) -> Value {
    let mut view = export.clone();
    if let Some(object) = view.as_object_mut() {
        object.remove("generated_at");
        if let Some(data) = object.get_mut("data") {
            if let Some(data) = data.as_object_mut() {
                data.remove("generated_at");
            }
        }
    }
    view
}

#[test]
fn repeated_export_of_the_same_dataset_is_semantically_identical() {
    let db = seeded_db("repeat-daily", 3 * 24 * 60); // 三天逐分钟心率
    let selection = make_selection(&["heart_rate", "workouts"], ExportDetail::Full);
    let (first_encoded, first_size) = db.build_ai_export(&selection).unwrap();
    assert!(first_size > 0);
    let first = serde_json::from_str::<Value>(&first_encoded).unwrap();

    for round in 0..4 {
        let (encoded, size) = db.build_ai_export(&selection).unwrap();
        let again = serde_json::from_str::<Value>(&encoded).unwrap();
        assert_eq!(size, first_size, "第 {round} 轮导出大小漂移");
        assert_eq!(
            semantic_view(&again),
            semantic_view(&first),
            "第 {round} 轮导出的语义内容漂移"
        );
    }

    // Summary 视图同样稳定，且不携带逐点序列。
    let selection = make_selection(&["heart_rate", "workouts"], ExportDetail::Summary);
    let (encoded, _) = db.build_ai_export(&selection).unwrap();
    let summary = serde_json::from_str::<Value>(&encoded).unwrap();
    assert!(summary["data"]["workouts"][0].get("samples").is_none());
}

#[test]
fn csv_export_is_stable_reparseable_and_missing_values_stay_absent() {
    let db = seeded_db("csv-daily", 2 * 24 * 60);
    let selection = make_selection(&["heart_rate", "workouts"], ExportDetail::Summary);
    let (first_json, _) = db.build_ai_export(&selection).unwrap();
    let first_export = serde_json::from_str::<Value>(&first_json).unwrap();

    let (csv_first, rows_first) = to_csv(&first_export).unwrap();
    assert!(rows_first > 0);
    assert!(csv_first.starts_with('\u{feff}'), "CSV 必须带 UTF-8 BOM");
    assert!(
        csv_first.contains("record_type,record_id,start_time,end_time,metric,value,unit"),
        "CSV 表头固定"
    );

    for round in 0..4 {
        let (json_again, _) = db.build_ai_export(&selection).unwrap();
        let export = serde_json::from_str::<Value>(&json_again).unwrap();
        let (csv_again, rows_again) = to_csv(&export).unwrap();
        assert_eq!(csv_again, csv_first, "第 {round} 轮 CSV 内容漂移");
        assert_eq!(rows_again, rows_first);
    }

    // 可再解析：每行字段数一致、value 列必须有值（缺失行被跳过而不是写成 0/空）。
    let mut lines = csv_first.trim_start_matches('\u{feff}').lines();
    let header = lines.next().unwrap();
    let columns = header.split(',').count();
    let mut data_lines = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        data_lines += 1;
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields.len(), columns, "行字段数必须与表头一致: {line}");
        assert!(
            !fields[5].trim().is_empty(),
            "value 列出现空值（缺失行应被跳过而不是留空）: {line}"
        );
        assert_ne!(
            fields[5].trim(),
            "0",
            "播种区间内不该出现 0 值（缺失被写成 0？）: {line}"
        );
    }
    assert!(data_lines == rows_first, "数据行数与 to_csv 返回值一致");
}

#[test]
fn gpx_export_is_stable_and_only_carries_real_points() {
    let db = seeded_db("gpx-daily", 60);
    let selection = make_selection(&["workouts"], ExportDetail::Full);
    let (encoded, _) = db.build_ai_export(&selection).unwrap();
    let export = serde_json::from_str::<Value>(&encoded).unwrap();

    let (gpx_first, points_first) = to_gpx(&export).unwrap();
    assert!(points_first > 0, "有 GPS 轨迹就必须导出点");
    assert!(gpx_first.contains("<gpx"), "GPX 1.1 根元素");
    assert!(gpx_first.contains("<trkpt"));

    for round in 0..4 {
        let (encoded_again, _) = db.build_ai_export(&selection).unwrap();
        let export_again = serde_json::from_str::<Value>(&encoded_again).unwrap();
        let (gpx_again, points_again) = to_gpx(&export_again).unwrap();
        assert_eq!(
            strip_metadata(&gpx_again),
            strip_metadata(&gpx_first),
            "第 {round} 轮 GPX 轨迹内容漂移（metadata 时间戳允许不同）"
        );
        assert_eq!(points_again, points_first);
    }

    // 轨迹里不允许 (0,0) 点。
    assert!(!gpx_first.contains("lat=\"0.000000\" lon=\"0.000000\""));
}

/// GPX 的 `<metadata>` 段里是导出时刻的时间戳，逐次导出必然不同；
/// 语义比较必须把它剥掉，而轨迹点本身（含每个 trkpt 的 `<time>`）保持严格一致。
fn strip_metadata(gpx: &str) -> String {
    match (gpx.find("<metadata>"), gpx.find("</metadata>")) {
        (Some(start), Some(end)) => {
            let mut out = String::with_capacity(gpx.len());
            out.push_str(&gpx[..start]);
            out.push_str(&gpx[end + "</metadata>".len()..]);
            out
        }
        _ => gpx.to_string(),
    }
}

#[test]
fn exporting_an_empty_library_is_an_error_not_an_empty_file() {
    let dir = TempDirGuard::new("empty-export");
    let db = Database::open_migrated(&dir.db_path()).unwrap();
    std::mem::forget(dir);
    let selection = make_selection(&["heart_rate"], ExportDetail::Full);
    // 空库导出要么在 build 阶段报错，要么产出的 data 段让 to_csv/to_gpx 报错；
    // 唯一不可接受的是「成功写出一个空文件」。
    let export = match db.build_ai_export(&selection) {
        Err(_) => return,
        Ok((encoded, _)) => serde_json::from_str::<Value>(&encoded).unwrap(),
    };
    assert!(
        to_csv(&export).is_err(),
        "没有数据时 to_csv 必须报错而不是产空 CSV"
    );
    assert!(
        to_gpx(&export).is_err(),
        "没有轨迹时 to_gpx 必须报错而不是产空 GPX"
    );
}

#[test]
fn missing_metric_fields_never_export_as_zero() {
    let db = seeded_db("missing-daily", 120);
    // Full 视图逐点心率（字段 value）；Summary 视图按小时聚合（字段
    // avg/min/max，没有 value）。两条路径的每个出现的数值都必须落在播种
    // 区间 60..90 —— 缺失被写成 0 的话会立刻掉出去。
    for detail in [ExportDetail::Summary, ExportDetail::Full] {
        let (encoded, _) = db
            .build_ai_export(&make_selection(&["heart_rate"], detail))
            .unwrap();
        let export = serde_json::from_str::<Value>(&encoded).unwrap();
        let samples = export["data"]["metric_samples"].as_array().unwrap();
        assert!(!samples.is_empty());
        for sample in samples {
            let numbers: Vec<f64> = ["value", "avg", "min", "max"]
                .iter()
                .filter_map(|key| sample.get(*key).and_then(Value::as_f64))
                .collect();
            assert!(
                !numbers.is_empty(),
                "样本行既没有 value 也没有聚合数值: {sample}"
            );
            for number in numbers {
                assert!(
                    (60.0..90.0).contains(&number),
                    "心率数值漂出播种区间（缺失被写成 0？）: {sample}"
                );
            }
        }
    }
}

#[test]
#[ignore = "压力级：30 天逐分钟 + 50 轮导出。cargo test -p zeppbridge-core --test export_contract_stress -- --ignored"]
fn stress_repeated_export_of_a_month_dataset() {
    let db = seeded_db("repeat-stress", 30 * 24 * 60);
    let started = std::time::Instant::now();
    let selection = make_selection(&["heart_rate", "workouts"], ExportDetail::Full);
    let (first_json, size) = db.build_ai_export(&selection).unwrap();
    let first = serde_json::from_str::<Value>(&first_json).unwrap();
    let (csv_baseline, _) = to_csv(
        &serde_json::from_str::<Value>(
            &db.build_ai_export(&make_selection(&["heart_rate", "workouts"], ExportDetail::Summary))
                .unwrap()
                .0,
        )
        .unwrap(),
    )
    .unwrap();
    for round in 0..50 {
        let (encoded, again_size) = db.build_ai_export(&selection).unwrap();
        assert_eq!(again_size, size);
        let again = serde_json::from_str::<Value>(&encoded).unwrap();
        assert_eq!(semantic_view(&again), semantic_view(&first), "第 {round} 轮漂移");
        let (csv_again, _) = to_csv(
            &serde_json::from_str::<Value>(
                &db.build_ai_export(&make_selection(&["heart_rate", "workouts"], ExportDetail::Summary))
                    .unwrap()
                    .0,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(csv_again, csv_baseline, "第 {round} 轮 CSV 漂移");
    }
    println!("50 轮大库导出耗时 {:?}", started.elapsed());
}

// ---------------------------------------------------------------------------
// contract
// ---------------------------------------------------------------------------

#[test]
fn contract_surface_is_stable_across_calls() {
    let first = zeppbridge_core::contract::metric_names();
    for _ in 0..25 {
        assert_eq!(zeppbridge_core::contract::metric_names(), first);
    }
    assert!(!first.is_empty());
    // 每个指标都有单位映射；未知指标返回 None 而不是猜一个。
    for metric in &first {
        assert!(
            zeppbridge_core::contract::unit_for(metric).is_some(),
            "指标 {metric} 必须声明单位"
        );
    }
    assert!(zeppbridge_core::contract::unit_for("not_a_metric").is_none());
    // 契约常量不漂移：缺失值约定必须仍然明说「不会用 0」。
    assert!(zeppbridge_core::contract::MISSING_VALUE_CONVENTION.contains("不会用 0"));
    assert_eq!(zeppbridge_core::contract::CONTRACT_VERSION, "1");
}
