//! normalizer / decoder 运行时强度测试。
//!
//! 与模块内单测互补：这里用**合成 fixture 批量喂入**，验证
//! 「单条坏数据不会毒化整批」「缺失保持缺失、绝不补 0」「无效 GPS 点被正确处理」
//! 在连续大量输入下仍然成立。
//!
//! 运行：
//! - 日常级：`cargo test -p zeppbridge-core --test normalizer_decoder_stress`
//! - 压力级：`cargo test -p zeppbridge-core --test normalizer_decoder_stress -- --ignored --nocapture`
//!
//! 原则：只用合成 fixture；不用真实 token / user id / HAR 原文。

use serde_json::{json, Value};
use zeppbridge_core::decoder::decode_workout_detail;
use zeppbridge_core::normalizer::Normalizer;

const BASE_TRACK: i64 = 1_700_000_000;

/// 合成一份 delta 编码的运动详情。
/// `seconds` 个采样点，HR 从 70 起每秒 +1（delta 编码），GPS 每秒向东/北
/// 各挪 1 个最小单位（1e-8 度），距离每秒 +100 cm。
fn synthetic_track(track_id: i64, seconds: usize, hr: bool, gps: bool) -> Value {
    let mut time = String::from("0;");
    let mut coords = String::new();
    let mut heart = String::new();
    let mut distance = String::new();
    if gps {
        coords.push_str("4004663552,11629333504;");
    }
    if hr {
        heart.push_str("0,70;");
    } else {
        heart.push_str("0,0;");
    }
    distance.push_str("0,0;");
    for second in 1..seconds {
        time.push_str("1;");
        if gps {
            coords.push_str("1,1;");
        }
        if hr {
            heart.push_str("1,1;");
        } else {
            heart.push_str("1,0;");
        }
        distance.push_str(&format!("1,{};", second * 100));
    }
    let mut data = json!({
        "trackid": track_id,
        "time": time,
        "currentDistance": distance,
    });
    if gps {
        data["longitude_latitude"] = json!(coords);
        data["altitude"] = json!("-2000000;1500;");
    }
    if hr {
        data["heart_rate"] = json!(heart);
    } else {
        data["heart_rate"] = json!("");
    }
    json!({ "data": data })
}

/// 各种坏数据的合成本。`kind` 决定坏法。
fn broken_payload(kind: &str, index: usize) -> Value {
    match kind {
        // 没有 trackid —— 必须报错。
        "missing_trackid" => json!({ "data": { "time": "0;1;" } }),
        // trackid 非法（0 / 负数）—— 必须报错。
        "invalid_trackid" => json!({ "data": { "trackid": 0i64, "time": "0;1;" } }),
        // data 缺失 / 不是对象。
        "no_data" => json!({ "other": 1 }),
        // 完全空的 detail。
        "empty" => json!({}),
        // 极端 delta：负的时间增量、巨大的坐标跳变。
        "extreme_deltas" => json!({
            "data": {
                "trackid": BASE_TRACK + index as i64,
                "time": "0;-5;999999999;",
                "longitude_latitude": "4004663552,11629333504;-99999999999,99999999999;1,1;",
                "altitude": "-999999999;999999999;",
                "heart_rate": "0,70;-3,300;1,-999;",
            }
        }),
        // 截断的 delta 串（真实网络中断现场）。
        "truncated" => json!({
            "data": {
                "trackid": BASE_TRACK + index as i64,
                "time": "0;1;1;1;",
                "longitude_latitude": "4004663552,1162933350",
                "heart_rate": "0,70;1,1",
            }
        }),
        // 字段类型错误（数字而不是字符串）。
        "wrong_types" => json!({
            "data": {
                "trackid": BASE_TRACK + index as i64,
                "time": 42,
                "longitude_latitude": 7,
                "heart_rate": true,
            }
        }),
        other => panic!("未知的坏数据种类: {other}"),
    }
}

// ---------------------------------------------------------------------------
// decoder：批量坏数据
// ---------------------------------------------------------------------------

#[test]
fn batch_of_mixed_good_and_bad_details_never_poisons_the_batch() {
    let kinds = [
        "missing_trackid",
        "invalid_trackid",
        "no_data",
        "empty",
        "extreme_deltas",
        "truncated",
        "wrong_types",
    ];
    let mut good = 0usize;
    let mut bad = 0usize;
    for index in 0..600usize {
        let raw = match index % 3 {
            0 => synthetic_track(BASE_TRACK + index as i64, 30, true, true),
            1 => broken_payload(kinds[index % kinds.len()], index),
            _ => synthetic_track(BASE_TRACK + index as i64, 5, false, false),
        };
        match decode_workout_detail(&raw, None) {
            Ok(decoded) => {
                good += 1;
                // 能解码出来的必须自洽：样本数 = 时长 + 1，时间单调。
                assert!(decoded.end_time >= decoded.start_time);
                assert_eq!(
                    decoded.samples.len() as i64,
                    (decoded.end_time - decoded.start_time).num_seconds() + 1,
                    "index {index}: 样本数与时长不自洽"
                );
            }
            Err(_) => bad += 1,
        }
    }
    // 每 3 条里 1 条坏数据：600 条中 200 条必须被拒绝、400 条成功。
    assert_eq!(bad, 200, "坏数据必须被拒绝");
    assert_eq!(good, 400, "好数据必须全部解码成功");
}

#[test]
fn missing_fields_stay_missing_never_zero() {
    // 没有 HR / 速度 / 步频 / 功率：每个样本这些字段都必须是 None，
    // 绝不允许出现 Some(0) 这种「编造出来的读数」。
    let raw = synthetic_track(BASE_TRACK, 60, false, true);
    let decoded = decode_workout_detail(&raw, None).unwrap();
    assert!(!decoded.samples.is_empty());
    for sample in &decoded.samples {
        assert_eq!(sample.heart_rate, None, "没有心率就只能是缺失");
        assert_eq!(sample.speed, None);
        assert_eq!(sample.pace, None);
        assert_eq!(sample.cadence, None);
        assert_eq!(sample.stride_cm, None);
        assert_eq!(sample.power_watts, None);
        assert_eq!(sample.ground_contact_ms, None);
    }

    // 没有 GPS：路由为空，但其余字段照常解码。
    let raw = synthetic_track(BASE_TRACK + 1, 30, true, false);
    let decoded = decode_workout_detail(&raw, None).unwrap();
    assert!(decoded.route.is_empty());
    assert!(
        decoded.samples.iter().any(|sample| sample.heart_rate == Some(70)),
        "心率仍然要解码出来"
    );

    // 没有海拔：route 的 altitude_m 全部缺失，不是 0。
    let mut raw = synthetic_track(BASE_TRACK + 2, 30, false, true);
    raw["data"].as_object_mut().unwrap().remove("altitude");
    raw["data"]["time_delta_altitude"] = json!("");
    let decoded = decode_workout_detail(&raw, None).unwrap();
    assert!(decoded.route.iter().all(|point| point.altitude_m.is_none()));
}

#[test]
fn invalid_or_absent_gps_points_never_landed_at_zero_zero() {
    // 坐标串里混入空段（真实报文常见）：空段没有坐标，必须被跳过，
    // 绝不能落到 (0, 0)。
    let raw = json!({
        "data": {
            "trackid": BASE_TRACK,
            "time": "0;1;1;1;1;",
            "longitude_latitude": "4004663552,11629333504;;;;;",
            "altitude": "-2000000;1500;",
        }
    });
    let decoded = decode_workout_detail(&raw, None).unwrap();
    assert!(
        decoded
            .route
            .iter()
            .all(|point| !(point.latitude == 0.0 && point.longitude == 0.0)),
        "路由里不允许出现 (0,0) 占位点"
    );

    // 极端坐标跳变：哪怕解析出来，也不该把后续点整体拉到无效位置。
    let raw = broken_payload("extreme_deltas", 0);
    let decoded = decode_workout_detail(&raw, None);
    // 允许 Err 或 Ok，但 Ok 时绝不能有 (0,0) 点。
    if let Ok(decoded) = decoded {
        assert!(decoded
            .route
            .iter()
            .all(|point| !(point.latitude == 0.0 && point.longitude == 0.0)));
    }
}

#[test]
fn same_input_decodes_identically_every_time() {
    let raw = synthetic_track(BASE_TRACK, 120, true, true);
    let first = decode_workout_detail(&raw, None).unwrap();
    for round in 0..9 {
        let again = decode_workout_detail(&raw, None).unwrap();
        assert_eq!(first, again, "第 {round} 次重复解码结果漂移");
    }
}

#[test]
#[ignore = "压力级：3000 条混合详情流。cargo test -p zeppbridge-core --test normalizer_decoder_stress -- --ignored"]
fn stress_three_thousand_mixed_details_stay_stable() {
    let started = std::time::Instant::now();
    let kinds = [
        "missing_trackid",
        "invalid_trackid",
        "no_data",
        "empty",
        "extreme_deltas",
        "truncated",
        "wrong_types",
    ];
    let mut good = 0usize;
    let mut bad = 0usize;
    for index in 0..3000usize {
        let raw = match index % 3 {
            0 => synthetic_track(BASE_TRACK + index as i64, 120, true, index % 6 == 0),
            1 => broken_payload(kinds[index % kinds.len()], index),
            _ => synthetic_track(BASE_TRACK + index as i64, 30, false, false),
        };
        match decode_workout_detail(&raw, None) {
            Ok(_) => good += 1,
            Err(_) => bad += 1,
        }
    }
    println!("3000 条混合详情解码耗时 {:?}", started.elapsed());
    assert_eq!(good, 2000);
    assert_eq!(bad, 1000);
}

#[test]
#[ignore = "压力级：10k 点 GPS 轨迹。cargo test -p zeppbridge-core --test normalizer_decoder_stress -- --ignored"]
fn stress_long_gps_track_decodes_exactly() {
    let started = std::time::Instant::now();
    let raw = synthetic_track(BASE_TRACK, 10_000, true, true);
    let decoded = decode_workout_detail(&raw, None).unwrap();
    println!("10k 点轨迹解码耗时 {:?}", started.elapsed());
    assert_eq!(decoded.route.len(), 10_000, "每个 GPS 点都必须在");
    let first = &decoded.route[0];
    assert!((first.latitude - 40.04663552).abs() < 1e-8);
    assert!((first.longitude - 116.29333504).abs() < 1e-8);
    // 最后一个点 = 第一个点 + 9999 个最小步长。
    let last = &decoded.route[9_999];
    let expected = 40.04663552 + 9_999.0 / 100_000_000.0;
    assert!((last.latitude - expected).abs() < 1e-8, "累计 delta 不能漂移");
    // 时间单调且逐秒。
    for pair in decoded.route.windows(2) {
        assert_eq!(
            (pair[1].timestamp - pair[0].timestamp).num_seconds(),
            1,
            "GPS 轨迹必须逐秒推进"
        );
    }
    // HR 是 delta 编码：最后一点 = 70 + 9999。
    let last_sample = &decoded.samples[9_999];
    assert_eq!(last_sample.heart_rate, Some(70 + 9_999));
}

#[test]
fn medium_gps_track_decodes_exactly_in_ci() {
    // 日常级的中等规模轨迹（2k 点），保证 CI 也有长轨迹覆盖。
    let raw = synthetic_track(BASE_TRACK, 2_000, true, true);
    let decoded = decode_workout_detail(&raw, None).unwrap();
    assert_eq!(decoded.route.len(), 2_000);
    assert_eq!(decoded.samples.len(), 2_001);
}

// ---------------------------------------------------------------------------
// normalizer：批量 + 缺失语义
// ---------------------------------------------------------------------------

#[test]
fn normalizer_bad_items_are_skipped_and_good_ones_survive() {
    let mut items = Vec::new();
    for index in 0..300usize {
        match index % 5 {
            0 => items.push(json!({"timestamp": 1_700_000_000 + index as i64, "value": 72})),
            1 => items.push(json!({"timestamp": 1_700_000_000 + index as i64})), // 缺 value
            2 => items.push(json!({"value": 72})),                               // 缺 timestamp
            3 => items.push(json!({"timestamp": 1_700_000_000 + index as i64, "value": -5})), // 非法值
            _ => items.push(json!({"timestamp": 1_700_000_000 + index as i64, "value": 999})), // 超上限
        }
    }
    let raw = json!({ "items": items });
    let batch = Normalizer::normalize_heart_rate_with_diagnostics(&raw).unwrap();
    // 只有 index%5==0 的 60 条有效；坏条目进 diagnostics，绝不出现在结果里，
    // 也绝不变成 0。
    assert_eq!(batch.records.len(), 60);
    assert_eq!(batch.diagnostics.len(), 240);
    assert!(batch
        .records
        .iter()
        .all(|sample| sample.value > 0.0 && sample.value <= 300.0));
}

#[test]
fn normalizer_empty_payload_is_unavailable_not_zero() {
    // 空 items：普通入口报 DataUnavailable（不是 Ok 空结果），
    // 带诊断的入口返回 Ok 但 0 条 + 有诊断。
    let raw = json!({ "items": [] });
    assert!(Normalizer::normalize_heart_rate(&raw).is_err());
    let batch = Normalizer::normalize_heart_rate_with_diagnostics(&raw).unwrap();
    assert!(batch.records.is_empty());
    assert!(!batch.diagnostics.is_empty());

    // 完全没有 items 数组同样处理。
    let raw = json!({ "unrelated": 1 });
    assert!(Normalizer::normalize_heart_rate(&raw).is_err());
    assert!(Normalizer::normalize_hrv(&raw).is_err());

    // items 里是标量 / null 等垃圾：不 panic，逐条拒绝。
    let raw = json!({ "items": [null, 42, "text", [], {}] });
    let batch = Normalizer::normalize_heart_rate_with_diagnostics(&raw).unwrap();
    assert!(batch.records.is_empty());
    assert_eq!(batch.diagnostics.len(), 5);
}

#[test]
fn normalizer_repeated_parsing_is_deterministic() {
    let raw = json!({
        "items": [
            {"timestamp": 1_700_000_000, "value": 72},
            {"timestamp": 1_700_000_060, "value": 80},
            {"timestamp": 1_700_000_120, "value": 65},
        ]
    });
    let first = Normalizer::normalize_heart_rate(&raw).unwrap();
    let first_json = serde_json::to_string(&first).unwrap();
    for round in 0..49 {
        let again = Normalizer::normalize_heart_rate(&raw).unwrap();
        let again_json = serde_json::to_string(&again).unwrap();
        assert_eq!(first_json, again_json, "第 {round} 次重复解析结果漂移");
    }
    assert_eq!(first.len(), 3);
}

#[test]
fn normalizer_workout_type_missing_stays_missing() {
    // 数字码未知：类型必须是 unknown:{code}，绝不继承 endpoint sport 名，
    // 也绝不能凭空变成 0 或某个猜测值。
    let raw = json!({
        "data": {
            "trackid": BASE_TRACK,
            "type": 9999,
            "start_time": 1_700_000_000,
            "end_time": 1_700_000_600,
        }
    });
    let workouts = Normalizer::normalize_workouts_with_sport(&raw, Some("running")).unwrap();
    assert_eq!(workouts.len(), 1);
    let workout = &workouts[0];
    assert_eq!(workout.type_source, "unknown_code");
    assert!(workout.normalized_type.starts_with("unknown:"), "{}", workout.normalized_type);
    assert!(
        !workout.normalized_type.contains("running"),
        "未知码不得继承 endpoint sport 名"
    );
}
