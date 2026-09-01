# 运行时强度测试

本项目的测试分成三层：

1. **单元测试**：与源码同文件或在 `src/**/tests.rs` 里，跑得快、覆盖分支。
2. **集成测试**：验证多个模块组合后的行为。
3. **运行时强度测试**：也就是本文档描述的测试，专门抓
   「单测/集成测试能过，但连续跑、并发、大数据量、重复进出、异常输入下会出问题」的缺陷。

所有运行时强度测试都**只用临时目录/临时库/合成 fixture**，不碰真实用户数据；
也不使用真实 token、user id、HAR 原文或 cookie。

## 执行环境约定

- 本地旧机器/开发机只负责**编写与静态审查**。
- 完整的 `cargo test --workspace` 与 `npm run test:functions` 等重计算任务，
  默认在**云端沙盒**执行：
  - GitHub Actions：`.github/workflows/stress-tests.yml`（日常级随 push/PR 跑，
    压力级需 `workflow_dispatch` 手动触发）。
  - Google Colab：仓库根 `scripts/stress/ZeppBridge_Runtime_Stress_Tests.ipynb`，
    适合不方便触发 Actions 时快速跑一轮。
- 不要在本地旧机器或内存紧张的机器上强跑压力级用例。

## 测试清单

### A. `storage_stress.rs`（`src-tauri/crates/core/tests/`）

覆盖模块：`storage`（migrations / coverage / backup / write_lock）

| 运行时风险 | 日常级用例 | 压力级用例 |
| --- | --- | --- |
| 迁移中断后重入 | 空库升级、user_version 回拨后 `open_migrated` 仍能补齐 | 连续升降 user_version 100 次 |
| coverage 状态混淆 | empty/pending/failed 混合 50 个 chunk 流转 | 2000 个 chunk 随机状态流转 |
| 备份与恢复 | backup→sha256 verify→restore→再写入，3 轮 | 30 轮，写入与 backup 并发 |
| 写锁争用 | 4 线程同时写，断言不丢数据 | 16 线程 × N 轮，断言无死锁 |
| 句柄/临时文件残留 | 测试 drop 后 `remove_dir_all` 成功 | 同上 |

运行：

```bash
# 日常级
cargo test -p zeppbridge-core --test storage_stress -- --nocapture
# 压力级
cargo test -p zeppbridge-core --test storage_stress -- --ignored --nocapture
```

### B. `normalizer_decoder_stress.rs`（`src-tauri/crates/core/tests/`）

覆盖模块：`decoder` / `normalizer`

| 运行时风险 | 日常级用例 | 压力级用例 |
| --- | --- | --- |
| 单条坏数据毒化整批 | 500 条里混入截断/缺 trackid/负时长/空数组 | 5000 条，混入异常大 delta、非法 GPS |
| 缺失变 0 | 无 HR/GPS 时断言输出不含 0 | 同上 |
| 长 GPS 轨迹 | 10k 点轨迹解码正确 | 100k 点，验证可完成 |

运行：

```bash
cargo test -p zeppbridge-core --test normalizer_decoder_stress
```

### C. `export_contract_stress.rs`（`src-tauri/crates/core/tests/`）

覆盖模块：`export_formats` / `contract`

| 运行时风险 | 日常级用例 | 压力级用例 |
| --- | --- | --- |
| 多次导出语义漂移 | 同一数据集反复导出 JSON/CSV/GPX 5 轮 | 50 轮 |
| 缺失值写成 0 | CSV/GPX 中缺失行被跳过 | 大范围导出 |
| 无数据时输出空文件 | 空库 export 返回 Err | 同上 |
| contract 稳定 | `metric_names`/`unit_for`/`MISSING_VALUE_CONVENTION` | 同上 |

运行：

```bash
cargo test -p zeppbridge-core --test export_contract_stress
```

### D. `auth_stress.rs`（`src-tauri/crates/core/tests/`）

覆盖模块：`auth` / HAR 提取

| 运行时风险 | 日常级用例 | 压力级用例 |
| --- | --- | --- |
| region_host 非法形态漏过 | 合法/非法 host、端口、userinfo、query/fragment、大小写 | 10000 次重复 normalize 稳定 |
| 凭据错误信息泄漏 token | 坏 `credentials.json`、后端失败时断言错误不含 token | 同上 |
| AuthInfo Debug 泄漏完整 token | `format!("{:?}", auth)` 包含完整 token（钉死 bug） | 同上 |
| 元数据写失败未回滚凭据 | 父目录为文件时触发回滚 | 1000 次 save/load/clear 幂等 |
| 旧版 auth.json token 残留 | 加载后重写磁盘，断言 token 消失 | 同上 |

运行：

```bash
cargo test -p zeppbridge-core --test auth_stress
```

### E. `runtime.rs`（CLI / MCP，分别位于 `crates/cli/tests/`、`crates/mcp/tests/`）

覆盖模块：`zeppbridge-cli` / `zeppbridge-mcp` 真实二进制

| 运行时风险 | CLI 用例 | MCP 用例 |
| --- | --- | --- |
| 库不存在/空库/有数据三种状态 | status/export/contract 退出码契约 | 无库返回 -32001，有库返回结果 |
| 连续调用稳定 | 同一命令连续 20 次输出一致 | tools/list 连续两次一致 |
| JSON 输出可解析 | `--json` 输出为合法 JSON | JSON-RPC 响应可解析 |
| 不 panic | stderr 不含 panic/thread/RUST_BACKTRACE | stderr 不含 panic/thread |
| 只读边界 | — | 工具调用前后 SQLite 主库字节一致 |

运行：

```bash
cargo test -p zeppbridge-cli --test runtime -- --nocapture
cargo test -p zeppbridge-mcp --test runtime -- --nocapture
```

### F. `functions-stress.test.mjs`（`tests/`）

覆盖模块：`functions/api/feedback.js` / `functions/api/release.js`

| 运行时风险 | 用例 |
| --- | --- |
| 非法 body / 缺字段 | 坏 JSON、缺 content-type、缺必填字段 |
| 超大输入 | body > 32KB 返回 413 |
| 重复提交 | 同一报告连发 50 次不崩溃 |
| 错误响应泄漏内部信息 | 错误体不含 token/SQL/stack/路径 |
| release fail-closed | GitHub 503/畸形 JSON/缺 asset 均返回 502 |
| release 缓存头稳定 | 200 响应带 `s-maxage=300` |

运行：

```bash
npm run test:functions
```

## CI 入口

`.github/workflows/stress-tests.yml` 定义：

- `daily-linux` / `daily-windows`：push/PR 到 `stress-tests` 分支时自动跑日常级。
- `stress-linux`：`workflow_dispatch` 勾选 `run-stress` 时跑全部 `#[ignore]` 用例。

触发方式：

1. 推送代码到 `x7687315-gif/ZeppBridge` 的 `stress-tests` 分支。
2. 打开 GitHub Actions → `runtime-stress-sandbox` → `Run workflow`。
3. 或在 Colab 中打开 `scripts/stress/ZeppBridge_Runtime_Stress_Tests.ipynb`，按单元格运行。

## 发现 bug 时的处理流程

1. 先补一个能复现的测试（如本文件所列）。
2. **不修改产品代码**，把 bug 写入 `BUGS_FOUND.md`：
   - 位置（file:line）
   - 现象与复现用例名
   - 违反的原则
   - 建议的最小修复方向
3. 在云端沙盒重新跑测试，确认测试能钉死问题。
4. 后续再由修复 PR 统一处理。

## 本地只跑 smoke 的情况

如果需要在本地快速验证新增测试是否「基本能编译/有戏」，可以只跑一个最小子集，
并观察资源占用：

```bash
cargo test -p zeppbridge-core --test auth_stress region_host_is_idempotent
cargo test -p zeppbridge-cli --test runtime version_and_contract_always_succeed
npm run test:functions -- --test-name-pattern='rejects non-JSON'
```

但完整运行仍应交到云端沙盒。
