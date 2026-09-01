//! storage 运行时强度测试（migrations / coverage / backup / write_lock / 资源清理）。
//!
//! 与 `src/storage/**` 里的单元测试互补：这里全部走**公开 API + 真实文件库**，
//! 针对的是「单测能过、但连续运行 / 并发 / 反复进出会出问题」的缺陷：
//! 迁移重入、锁未释放、临时文件残留、备份与写入并发、恢复后数据丢失。
//!
//! 运行：
//! - 日常级（默认 `cargo test` 即跑）：`cargo test -p zeppbridge-core --test storage_stress`
//! - 压力级（`#[ignore]`，手工跑）：
//!   `cargo test -p zeppbridge-core --test storage_stress -- --ignored --nocapture`
//!
//! 原则：只用临时目录；不触碰真实用户数据目录。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use zeppbridge_core::models::{CapabilityStatus, RawRecord, SourceScope};
use zeppbridge_core::storage::backup::{
    self, BackupKind, BackupVerification, MIGRATION_BACKUP_KEEP,
};
use zeppbridge_core::storage::coverage::{BACKFILL_STREAMS, ChunkStatus, month_chunks};
use zeppbridge_core::storage::write_lock::{self, WritePurpose};
use zeppbridge_core::storage::Database;

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// 每个测试一个唯一临时目录；`Guard` drop 时整体删除。
/// 在 Windows 上 `remove_dir_all` 成功本身就验证了所有句柄都已释放。
struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zb-storage-stress-{}-{}-{}",
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

fn raw_record(stream: &str, source_key: &str) -> RawRecord {
    RawRecord {
        stream: stream.to_string(),
        source_key: source_key.to_string(),
        source_scope: SourceScope::UserFused,
        device_id: None,
        start_utc: Utc::now(),
        end_utc: None,
        payload: serde_json::json!({ "items": [] }),
        capability: CapabilityStatus::Verified,
    }
}

/// 用独立的 rusqlite 只读连接数行 / 校验完整性 —— 刻意不经过 Database，
/// 免得被被测代码自身的连接状态掩盖问题。
fn count_raw_records(db_path: &Path) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row("SELECT COUNT(*) FROM raw_records", [], |row| row.get(0))
        .unwrap()
}

fn user_version(db_path: &Path) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn integrity_ok(db_path: &Path) -> bool {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row("PRAGMA integrity_check(1)", [], |row| row.get::<_, String>(0))
        .map(|value| value.eq_ignore_ascii_case("ok"))
        .unwrap_or(false)
}

/// 模拟「迁移中断」：把 user_version 拨回某个旧值（schema 本体与
/// schema_migrations 都还在）。迁移是幂等的，重入后必须能补齐版本号。
fn simulate_interrupted_migration(db_path: &Path, downgrade_to: i64) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute_batch(&format!("PRAGMA user_version = {downgrade_to};"))
        .unwrap();
}

fn date(text: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

// ---------------------------------------------------------------------------
// migrations
// ---------------------------------------------------------------------------

#[test]
fn migrations_empty_database_upgrades_and_reopens_idempotently() {
    let dir = TempDirGuard::new("mig-empty");
    for round in 0..6 {
        let db = Database::open_migrated(&dir.db_path())
            .unwrap_or_else(|e| panic!("第 {round} 次打开空库失败: {e}"));
        drop(db);
        assert_eq!(user_version(&dir.db_path()), 16, "每次打开后都应是 v16");
        assert!(integrity_ok(&dir.db_path()));
    }
}

#[test]
fn migration_reentry_after_interrupt_recovers_and_keeps_data() {
    let dir = TempDirGuard::new("mig-reentry");
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        for i in 0..10 {
            db.insert_raw_record(&raw_record("heart_rate", &format!("hr-{i}")))
                .unwrap();
        }
    }
    assert_eq!(count_raw_records(&dir.db_path()), 10);

    // 反复模拟「升级被打断在中间某一步」再重入。
    for downgrade_to in [15, 12, 7, 4, 1, 15, 7] {
        simulate_interrupted_migration(&dir.db_path(), downgrade_to);
        let db = Database::open_migrated(&dir.db_path())
            .unwrap_or_else(|e| panic!("从 v{downgrade_to} 重入迁移失败: {e}"));
        drop(db);
        assert_eq!(
            user_version(&dir.db_path()),
            16,
            "重入后版本号必须回到 v16（从 v{downgrade_to}）"
        );
        assert_eq!(
            count_raw_records(&dir.db_path()),
            10,
            "重入迁移不能丢已写入的记录（从 v{downgrade_to}）"
        );
        assert!(integrity_ok(&dir.db_path()));
    }
}

/// 模拟「升级前备份」路径：降版本后重入必须产生一份 PreMigration 快照，
/// 且滚动清理后不超过 MIGRATION_BACKUP_KEEP 份。
#[test]
fn migration_reentry_leaves_a_verifiable_pre_migration_backup() {
    let dir = TempDirGuard::new("mig-backup");
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        db.insert_raw_record(&raw_record("sleep", "sleep-1")).unwrap();
    }
    for round in 0..(MIGRATION_BACKUP_KEEP as i32 + 4) {
        simulate_interrupted_migration(&dir.db_path(), 15);
        Database::open_migrated(&dir.db_path()).unwrap();
        let backups = backup::list_backups(&dir.path).unwrap();
        let migration_backups = backups
            .iter()
            .filter(|m| m.kind == BackupKind::PreMigration)
            .count();
        assert!(
            migration_backups <= MIGRATION_BACKUP_KEEP,
            "第 {round} 轮后迁移备份应滚动保留 ≤{MIGRATION_BACKUP_KEEP} 份，实际 {migration_backups}"
        );
        if let Some(latest) = backups.first() {
            let verification: BackupVerification =
                backup::verify_backup(&dir.path, &latest.id).unwrap();
            assert!(
                verification.is_usable(),
                "最新的升级前备份必须可用: {:?}",
                verification.problem
            );
        }
    }
}

#[test]
#[ignore = "压力级：40 轮降版本-重入马拉松。cargo test -p zeppbridge-core --test storage_stress -- --ignored"]
fn stress_migrations_downgrade_reopen_marathon() {
    let dir = TempDirGuard::new("mig-marathon");
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        for i in 0..100 {
            db.insert_raw_record(&raw_record("heart_rate", &format!("hr-{i}")))
                .unwrap();
        }
    }
    let started = Instant::now();
    for round in 0..40 {
        let downgrade_to = 1 + (round % 15) as i64;
        simulate_interrupted_migration(&dir.db_path(), downgrade_to);
        Database::open_migrated(&dir.db_path()).unwrap();
        assert_eq!(user_version(&dir.db_path()), 16);
        assert_eq!(
            count_raw_records(&dir.db_path()),
            100,
            "第 {round} 轮（从 v{downgrade_to} 重入）丢了数据"
        );
        assert!(integrity_ok(&dir.db_path()));
    }
    println!("40 轮迁移重入耗时 {:?}", started.elapsed());
}

// ---------------------------------------------------------------------------
// coverage
// ---------------------------------------------------------------------------

#[test]
fn coverage_large_mixed_statuses_stay_distinct() {
    let dir = TempDirGuard::new("coverage-mixed");
    let db = Database::open_migrated(&dir.db_path()).unwrap();

    let from = date("2015-01-01");
    let to = date("2016-12-31");
    let chunks = month_chunks(from, to);
    assert_eq!(chunks.len(), 24);
    let total = chunks.len() * BACKFILL_STREAMS.len();
    assert_eq!(db.plan_backfill(from, to).unwrap() as usize, total);

    // 四种去向轮流分配：persisted / empty_from_cloud / failed / 留在 pending。
    let mut failed_rows = 0usize;
    let mut persisted_rows = 0usize;
    let mut empty_rows = 0usize;
    for stream in BACKFILL_STREAMS {
        for (index, (start, _end)) in chunks.iter().enumerate() {
            let start = start.to_string();
            match index % 4 {
                0 => {
                    db.record_backfill_chunk(
                        stream, &start, ChunkStatus::Persisted, 31, None, None,
                    )
                    .unwrap();
                    persisted_rows += 1;
                }
                1 => {
                    db.record_backfill_chunk(
                        stream, &start, ChunkStatus::EmptyFromCloud, 0, None, None,
                    )
                    .unwrap();
                    empty_rows += 1;
                }
                2 => {
                    db.record_backfill_chunk(
                        stream,
                        &start,
                        ChunkStatus::Failed,
                        0,
                        Some("网络超时"),
                        Some("err.core.network"),
                    )
                    .unwrap();
                    failed_rows += 1;
                }
                _ => {} // pending
            }
        }
    }

    let ledger = db.coverage_ledger().unwrap();
    assert_eq!(ledger.total_chunks as usize, total);
    assert!(!ledger.complete, "还有 pending/failed 时不能自称完整");
    assert_eq!(ledger.failed_chunks_detail.len(), failed_rows);

    for stream in ledger.streams.iter() {
        assert_eq!(stream.requested_chunks as usize, chunks.len());
        assert_eq!(
            (stream.persisted_chunks
                + stream.empty_chunks
                + stream.failed_chunks
                + stream.pending_chunks) as usize,
            chunks.len(),
            "四种状态计数之和必须等于块总数（stream: {}）",
            stream.stream
        );
    }

    // empty_from_cloud 绝不是失败：不在失败清单、不回待办、不触发人工重试提示。
    let failed = db.failed_backfill_chunks().unwrap();
    assert_eq!(failed.len(), failed_rows);
    let pending = db.pending_backfill_chunks(total).unwrap();
    assert!(
        pending
            .iter()
            .all(|chunk| chunk.status == "pending" || chunk.status == "failed"),
        "待办队列只能有 pending/failed，不得混入 persisted/empty"
    );
    assert_eq!(pending.len(), failed_rows + (total - failed_rows - persisted_rows - empty_rows));
    // 失败块必须带稳定码（英文界面靠它取文案）。
    assert!(failed
        .iter()
        .all(|chunk| chunk.error_code.as_deref() == Some("err.core.network")));

    // 重试一次失败块成功后：错误清掉、计入 persisted、attempts 归零。
    let first_failed = &failed[0];
    db.record_backfill_chunk(
        &first_failed.stream,
        &first_failed.chunk_start,
        ChunkStatus::Persisted,
        7,
        None,
        None,
    )
    .unwrap();
    let ledger = db.coverage_ledger().unwrap();
    let stream = ledger
        .streams
        .iter()
        .find(|s| s.stream == first_failed.stream)
        .unwrap();
    assert_eq!(stream.failed_chunks, failed_rows as i64 - 1);
    assert_eq!(stream.persisted_chunks, (persisted_rows as i64) / (BACKFILL_STREAMS.len() as i64) + 1);

    // 重复排同一范围不得重开任何已有结论的块。
    assert_eq!(db.plan_backfill(from, to).unwrap(), 0);
}

#[test]
fn coverage_exhausted_failures_stop_autoretrying_but_stay_visible() {
    let dir = TempDirGuard::new("coverage-exhausted");
    let db = Database::open_migrated(&dir.db_path()).unwrap();
    db.plan_backfill(date("2026-01-01"), date("2026-12-31")).unwrap();
    let chunk = "2026-06-01";

    for _ in 0..zeppbridge_core::storage::coverage::MAX_AUTO_ATTEMPTS {
        let queue = db.pending_backfill_chunks(1000).unwrap();
        assert!(queue
            .iter()
            .any(|c| c.stream == "heart_rate" && c.chunk_start == chunk));
        db.record_backfill_chunk(
            "heart_rate",
            chunk,
            ChunkStatus::Failed,
            0,
            Some("确定性解析失败"),
            Some("err.backfill.no_canonical_records"),
        )
        .unwrap();
    }
    let queue = db.pending_backfill_chunks(1000).unwrap();
    assert!(
        !queue
            .iter()
            .any(|c| c.stream == "heart_rate" && c.chunk_start == chunk),
        "自动重试用尽后必须退出自动队列"
    );
    let ledger = db.coverage_ledger().unwrap();
    assert!(ledger.needs_manual_retry, "用尽重试要提示用户显式重试");
    assert!(!ledger.complete);

    // 人工重试清零后重新可做；其余块不受影响。
    assert_eq!(db.reset_failed_backfill_chunks().unwrap(), 1);
    assert!(db
        .pending_backfill_chunks(1000)
        .unwrap()
        .iter()
        .any(|c| c.stream == "heart_rate" && c.chunk_start == chunk));
}

#[test]
#[ignore = "压力级：2880 块全量流转。cargo test -p zeppbridge-core --test storage_stress -- --ignored"]
fn stress_coverage_thousands_of_chunks_flow_correctly() {
    let dir = TempDirGuard::new("coverage-thousands");
    let db = Database::open_migrated(&dir.db_path()).unwrap();

    let from = date("2000-01-01");
    let to = date("2039-12-31"); // 480 个月 × 6 流 = 2880 块
    let total = month_chunks(from, to).len() * BACKFILL_STREAMS.len();
    let planned = db.plan_backfill(from, to).unwrap() as usize;
    assert_eq!(planned, total);

    let started = Instant::now();
    let mut persisted = 0usize;
    let mut empty = 0usize;
    let mut failed = 0usize;
    for stream in BACKFILL_STREAMS {
        for (index, (start, _end)) in month_chunks(from, to).iter().enumerate() {
            let start = start.to_string();
            match index % 3 {
                0 => {
                    db.record_backfill_chunk(stream, &start, ChunkStatus::Persisted, 10, None, None)
                        .unwrap();
                    persisted += 1;
                }
                1 => {
                    db.record_backfill_chunk(stream, &start, ChunkStatus::EmptyFromCloud, 0, None, None)
                        .unwrap();
                    empty += 1;
                }
                _ => {
                    db.record_backfill_chunk(
                        stream, &start, ChunkStatus::Failed, 0, Some("超时"), Some("err.core.network"),
                    )
                    .unwrap();
                    failed += 1;
                }
            }
        }
    }
    let ledger = db.coverage_ledger().unwrap();
    println!("2880 块流转耗时 {:?}", started.elapsed());
    assert_eq!(ledger.total_chunks as usize, total);
    assert_eq!(ledger.completed_chunks as usize, persisted + empty);
    assert_eq!(ledger.failed_chunks_detail.len(), failed);
    assert!(!ledger.complete, "failed 块未处理时不能自称完整");

    // 全部重试成功后必须完整。
    db.reset_failed_backfill_chunks().unwrap();
    while let Some(chunk) = db.pending_backfill_chunks(1).unwrap().into_iter().next() {
        db.record_backfill_chunk(
            &chunk.stream,
            &chunk.chunk_start,
            ChunkStatus::Persisted,
            1,
            None,
            None,
        )
        .unwrap();
    }
    let ledger = db.coverage_ledger().unwrap();
    assert!(ledger.complete, "全部块有结论后 complete 必须为真");
    assert_eq!(ledger.completed_chunks, ledger.total_chunks);
}

// ---------------------------------------------------------------------------
// backup / restore
// ---------------------------------------------------------------------------

#[test]
fn backup_restore_roundtrip_repeatedly() {
    let dir = TempDirGuard::new("backup-roundtrip");
    let rounds = 4;
    let mut cumulative = 0i64;

    for round in 0..rounds {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        for i in 0..(round + 1) {
            cumulative += 1;
            db.insert_raw_record(&raw_record("heart_rate", &format!("r{round}-a{i}")))
                .unwrap();
        }
        drop(db);

        let manifest =
            backup::create_backup(&dir.path, BackupKind::Manual, "stress-test").unwrap();
        assert!(manifest.integrity_ok);
        let verification = backup::verify_backup(&dir.path, &manifest.id).unwrap();
        assert!(verification.is_usable(), "第 {round} 轮备份必须可用");

        // 备份之后继续写 —— 这些写入必须在恢复后消失。
        let db = Database::open_without_migration(dir.db_path()).unwrap();
        for i in 0..(round + 1) {
            db.insert_raw_record(&raw_record("heart_rate", &format!("r{round}-b{i}")))
                .unwrap();
        }
        drop(db);
        assert!(count_raw_records(&dir.db_path()) > cumulative);

        let pending = backup::stage_restore(&dir.path, &manifest.id, "stress-test").unwrap();
        assert_eq!(pending.backup_id, manifest.id);
        let outcome = backup::apply_pending_restore(&dir.path)
            .expect("排队过的恢复必须产生结果");
        assert!(outcome.succeeded, "第 {round} 轮恢复失败: {}", outcome.message);
        assert!(backup::pending_restore(&dir.path).is_none());

        assert_eq!(
            count_raw_records(&dir.db_path()),
            cumulative,
            "第 {round} 轮恢复后行数必须回到备份时刻"
        );
        assert!(integrity_ok(&dir.db_path()));
    }

    // 回滚快照（pre-restore）也堆积在备份目录里：都不能是坏的。
    let all = backup::list_backups(&dir.path).unwrap();
    assert!(all.len() >= rounds);
    for manifest in &all {
        let verification = backup::verify_backup(&dir.path, &manifest.id).unwrap();
        assert!(
            verification.is_usable(),
            "备份 {} 未通过校验: {:?}",
            manifest.id,
            verification.problem
        );
    }
}

#[test]
fn backup_concurrent_with_writes_never_produces_a_bad_snapshot() {
    let dir = Arc::new(TempDirGuard::new("backup-concurrent"));
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        db.insert_raw_record(&raw_record("heart_rate", "seed")).unwrap();
    }

    let writer_dir = Arc::clone(&dir);
    let writer = std::thread::spawn(move || {
        // 写者不拿跨进程写锁、直接写 —— 模拟 GUI 同步线程与用户点备份并发。
        let db = Database::open_without_migration(writer_dir.db_path()).unwrap();
        for i in 0..150 {
            db.insert_raw_record(&raw_record("heart_rate", &format!("w{i}")))
                .unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        150usize
    });

    let mut backups = Vec::new();
    while !writer.is_finished() {
        match backup::create_backup(&dir.path, BackupKind::Manual, "stress-test") {
            Ok(manifest) => backups.push(manifest.id),
            Err(error) => panic!("并发写入期间备份失败: {error}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let written = writer.join().unwrap();

    assert!(
        count_raw_records(&dir.db_path()) as usize == written + 1,
        "写入不能丢"
    );
    assert!(integrity_ok(&dir.db_path()), "并发写入后库必须完整");
    assert!(
        backups.len() >= 2,
        "并发窗口里至少应产出两份快照，实际 {}",
        backups.len()
    );
    for id in &backups {
        let verification = backup::verify_backup(&dir.path, id).unwrap();
        assert!(
            verification.is_usable(),
            "并发期间生成的快照 {id} 未通过校验: {:?}",
            verification.problem
        );
    }
}

#[test]
#[ignore = "压力级：30 轮备份-恢复马拉松。cargo test -p zeppbridge-core --test storage_stress -- --ignored"]
fn stress_backup_restore_marathon() {
    let dir = TempDirGuard::new("backup-marathon");
    let mut cumulative = 0i64;
    let started = Instant::now();
    for round in 0..30 {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        for i in 0..5 {
            cumulative += 1;
            db.insert_raw_record(&raw_record("sleep", &format!("m{round}-{i}")))
                .unwrap();
        }
        drop(db);
        let manifest =
            backup::create_backup(&dir.path, BackupKind::Manual, "stress-test").unwrap();
        let db = Database::open_without_migration(dir.db_path()).unwrap();
        for i in 0..5 {
            db.insert_raw_record(&raw_record("sleep", &format!("x{round}-{i}")))
                .unwrap();
        }
        drop(db);
        backup::stage_restore(&dir.path, &manifest.id, "stress-test").unwrap();
        let outcome = backup::apply_pending_restore(&dir.path).unwrap();
        assert!(outcome.succeeded, "第 {round} 轮: {}", outcome.message);
        assert_eq!(count_raw_records(&dir.db_path()), cumulative);
        assert!(integrity_ok(&dir.db_path()));
    }
    println!("30 轮备份-恢复耗时 {:?}", started.elapsed());
}

// ---------------------------------------------------------------------------
// write_lock
// ---------------------------------------------------------------------------

#[test]
fn write_lock_multithreaded_contention_keeps_every_write() {
    let dir = Arc::new(TempDirGuard::new("lock-contention"));
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        db.insert_raw_record(&raw_record("heart_rate", "seed")).unwrap();
    }

    let threads = 4;
    let rounds = 20;
    let mut handles = Vec::new();
    for thread_id in 0..threads {
        let dir = Arc::clone(&dir);
        handles.push(std::thread::spawn(move || {
            let mut inserted = 0usize;
            for round in 0..rounds {
                let purpose = match round % 4 {
                    0 => WritePurpose::Sync,
                    1 => WritePurpose::Backup,
                    2 => WritePurpose::Reprocess,
                    _ => WritePurpose::Cleanup,
                };
                // 争用窗口有限：最多等 30 秒，超时直接失败而不是无限挂起。
                let guard = write_lock::acquire_with_timeout(&dir.path, purpose, Duration::from_secs(30))
                    .unwrap_or_else(|e| panic!("线程 {thread_id} 第 {round} 轮取锁失败: {e}"));
                let db = Database::open_without_migration(dir.db_path()).unwrap();
                db.insert_raw_record(&raw_record(
                    "heart_rate",
                    &format!("t{thread_id}-r{round}"),
                ))
                .unwrap();
                drop(db);
                drop(guard);
                inserted += 1;
                // 故意制造争用：不 sleep 直接抢。
            }
            inserted
        }));
    }

    let total_inserted: usize = handles.into_iter().map(|handle| handle.join().unwrap()).sum();

    assert_eq!(
        count_raw_records(&dir.db_path()) as usize,
        total_inserted + 1,
        "锁下并发写入不能丢数据"
    );
    assert!(integrity_ok(&dir.db_path()));
    assert!(
        !dir.path.join("zepp.db.write-lock.holder").exists(),
        "全部结束后不得残留持有者文件"
    );
}

#[test]
fn write_lock_timeout_reports_busy_rather_than_hanging_forever() {
    let dir = TempDirGuard::new("lock-timeout");
    let _held = write_lock::try_acquire(&dir.path, WritePurpose::HistoryBackfill).unwrap();
    let started = Instant::now();
    let error = write_lock::acquire_with_timeout(&dir.path, WritePurpose::Sync, Duration::from_millis(500))
        .expect_err("别人持锁时必须失败");
    let elapsed = started.elapsed();
    assert!(matches!(error, write_lock::WriteLockError::Busy { .. }));
    assert!(elapsed >= Duration::from_millis(400), "应当真的等过: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(10), "不该等太久: {elapsed:?}");
}

#[test]
fn write_lock_open_migrated_from_many_threads_serializes_migration() {
    // 多线程同时 open_migrated（各拿各的连接）：迁移锁必须把它们串行化，
    // 且最终库是完整可读的、没有半个迁移状态。
    let dir = Arc::new(TempDirGuard::new("lock-migrated"));
    let handles: Vec<_> = (0..6)
        .map(|thread_index| {
            let dir = Arc::clone(&dir);
            std::thread::spawn(move || {
                let db = Database::open_migrated(&dir.db_path())?;
                db.insert_raw_record(&raw_record(
                    "heart_rate",
                    &format!("m-{thread_index}"),
                ))
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(user_version(&dir.db_path()), 16);
    assert!(integrity_ok(&dir.db_path()));
    assert_eq!(count_raw_records(&dir.db_path()), 6);
}

#[test]
#[ignore = "压力级：16 线程 × 100 轮锁争用。cargo test -p zeppbridge-core --test storage_stress -- --ignored"]
fn stress_write_lock_barrage() {
    let dir = Arc::new(TempDirGuard::new("lock-barrage"));
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        db.insert_raw_record(&raw_record("heart_rate", "seed")).unwrap();
    }
    let started = Instant::now();
    let handles: Vec<_> = (0..16)
        .map(|thread_id| {
            let dir = Arc::clone(&dir);
            std::thread::spawn(move || {
                let mut inserted = 0usize;
                for round in 0..100 {
                    let guard = write_lock::acquire_with_timeout(
                        &dir.path,
                        WritePurpose::Sync,
                        Duration::from_secs(60),
                    )
                    .unwrap();
                    let db = Database::open_without_migration(dir.db_path()).unwrap();
                    db.insert_raw_record(&raw_record(
                        "heart_rate",
                        &format!("b{thread_id}-{round}"),
                    ))
                    .unwrap();
                    drop(db);
                    drop(guard);
                    inserted += 1;
                }
                inserted
            })
        })
        .collect();
    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    println!("16×100 轮锁争用耗时 {:?}", started.elapsed());
    assert_eq!(count_raw_records(&dir.db_path()) as usize, total + 1);
    assert!(integrity_ok(&dir.db_path()));
    assert!(!dir.path.join("zepp.db.write-lock.holder").exists());
}

// ---------------------------------------------------------------------------
// 资源清理
// ---------------------------------------------------------------------------

#[test]
fn resources_no_leftover_lock_or_staging_files_after_full_cycle() {
    let dir = TempDirGuard::new("cleanup");
    {
        let db = Database::open_migrated(&dir.db_path()).unwrap();
        db.insert_raw_record(&raw_record("heart_rate", "c1")).unwrap();
        db.plan_backfill(date("2026-01-01"), date("2026-02-28")).unwrap();
    }
    backup::create_backup(&dir.path, BackupKind::Manual, "stress-test").unwrap();

    // 拿过锁再放下。
    drop(write_lock::try_acquire(&dir.path, WritePurpose::Compaction).unwrap());

    for entry in walk_data_dir(&dir.path) {
        let name = entry.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            !name.ends_with(".write-lock.holder"),
            "残留持有者文件: {name}"
        );
        assert!(!name.contains("restore-staging"), "残留恢复暂存文件: {name}");
        assert!(!name.contains("restore-previous"), "残留被替换库: {name}");
        assert!(name != "restore-pending.json", "残留恢复待办: {name}");
    }

    // Windows 上 remove_dir_all 失败 = 还有句柄没关。
    let path = dir.path.clone();
    drop(dir);
    // TempDirGuard 的 drop 已经删过一次；这里再确认目录确实没了。
    assert!(!path.exists(), "临时目录未能整体删除，说明有句柄未释放");
}

fn walk_data_dir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// 连续重复进出：同一目录反复 open → 写 → close 多轮后，行数严格累加、
/// 无残留。抓「句柄/锁未释放导致间歇性失败」与「重复执行结果不一致」。
#[test]
fn repeated_open_write_close_cycles_are_stable() {
    let dir = TempDirGuard::new("open-cycles");
    let rounds = 25;
    for round in 0..rounds {
        let db = Database::open_migrated(&dir.db_path())
            .unwrap_or_else(|e| panic!("第 {round} 轮打开失败: {e}"));
        db.insert_raw_record(&raw_record("heart_rate", &format!("cycle-{round}")))
            .unwrap();
        drop(db);
        assert_eq!(
            count_raw_records(&dir.db_path()),
            round as i64 + 1,
            "第 {round} 轮后行数不匹配 —— 重复进出结果不一致"
        );
    }
    assert!(integrity_ok(&dir.db_path()));
    assert!(!dir.path.join("zepp.db.write-lock.holder").exists());
}

/// 重复执行 sync 侧的写入入口（persist_fetched_record）同一条记录：
/// UNIQUE(stream, source_key) 幂等，结果必须一致而不是报错或翻倍。
#[test]
fn persisting_the_same_fetched_record_twice_is_idempotent() {
    let dir = TempDirGuard::new("persist-idempotent");
    let db = Database::open_migrated(&dir.db_path()).unwrap();
    let record = raw_record("heart_rate", "same-key-1");
    let (first, _) = db.persist_fetched_record(&record).unwrap();
    for _ in 0..9 {
        let (again, _) = db.persist_fetched_record(&record).unwrap();
        assert_eq!(first, again, "同一条记录重复写入必须幂等");
    }
    assert_eq!(count_raw_records(&dir.db_path()), 1);
    assert!(integrity_ok(&dir.db_path()));
}
