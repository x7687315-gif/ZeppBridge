import assert from 'node:assert/strict';
import test from 'node:test';

import { onRequestPost, validateFeedbackReport, contentHashInput } from '../functions/api/feedback.js';
import { onRequestGet, projectLatestRelease } from '../functions/api/release.js';

// ---------------------------------------------------------------------------
// Feedback fixtures
// ---------------------------------------------------------------------------

const validReport = () => ({
  format: 'zeppbridge.feedback.v1',
  appVersion: '2.0.0',
  schemaVersion: 16,
  normalizerRevision: 'zepp-normalizer-2026-09-v17-road-cycling',
  operatingSystem: 'windows',
  deviceEvidence: {
    status: 'available',
    objectCount: 3,
    unknownDeviceCount: 1,
    idAliasObjects: 0,
    serialAliasObjects: 0,
    nameFieldObjects: 1,
    firmwareFieldObjects: 1,
    candidates: [],
    unmatchedProductHints: ['Amazfit Future Watch'],
    modelIdentifierHints: ['deviceSource:7930112'],
    shapes: [],
  },
  unknownWorkoutCodes: [{ code: 240, records: 2 }],
  workoutTypeConflicts: 0,
});

// v2.0.0 的 feedback.js 依赖三张语义：内容去重（feedback_reports.content_hash）、
// 按来源限流（feedback_intake_counters）、插入报告。这个 stub 按语句模式实现
// 同样的最小语义，让测试观察「产品代码以为自己在数据库上做的事」。
function makeD1() {
  const reports = new Map(); // content_hash -> { id, received_at }
  const counters = new Map(); // source_hash -> { window_started_at, count }
  let countersBroken = false;

  const exec = async (sql, bound) => {
    if (sql.includes('SELECT id, received_at FROM feedback_reports')) {
      return reports.get(bound[0]) ?? null; // .first() 语义
    }
    if (sql.includes('SELECT window_started_at')) {
      if (countersBroken) throw new Error('intake counters unavailable');
      return counters.get(bound[0]) ?? null;
    }
    if (sql.includes('INSERT INTO feedback_intake_counters')) {
      counters.set(bound[0], { window_started_at: bound[1], count: 1 });
      return { success: true };
    }
    if (sql.includes('UPDATE feedback_intake_counters')) {
      const row = counters.get(bound[0]);
      if (row) row.count += 1;
      return { success: true };
    }
    if (sql.includes('INSERT INTO feedback_reports')) {
      // 绑定参数第 16 个（下标 15）是 content_hash。
      reports.set(bound[15], { id: bound[0], received_at: bound[1] });
      return { success: true };
    }
    throw new Error(`stub 未实现的语句: ${sql.slice(0, 60)}`);
  };

  return {
    prepare(sql) {
      return {
        bind(...bound) {
          return {
            run: async () => exec(sql, bound),
            first: async () => exec(sql, bound),
            all: async () => ({ results: [] }),
          };
        },
        // 无 bind 直接调用（防御：真实代码总是 bind）。
        run: async () => exec(sql, []),
        first: async () => exec(sql, []),
      };
    },
    breakCounters() {
      countersBroken = true;
    },
    reportCount() {
      return reports.size;
    },
  };
}

function makeRequest(body, { contentType = 'application/json', contentLength, sourceIp } = {}) {
  const raw = typeof body === 'string' ? body : JSON.stringify(body);
  const bytes = new TextEncoder().encode(raw);
  const headers = new Map();
  headers.set('content-type', contentType);
  headers.set('content-length', String(contentLength ?? bytes.length));
  if (sourceIp) headers.set('cf-connecting-ip', sourceIp);
  return new Request('https://zeppbridge.pages.dev/api/feedback', {
    method: 'POST',
    headers,
    body: raw,
  });
}

const post = (db, body, options = {}) =>
  onRequestPost({ request: makeRequest(body, options), env: { FEEDBACK_DB: db } });

// ---------------------------------------------------------------------------
// Feedback boundary tests
// ---------------------------------------------------------------------------

test('rejects non-JSON content-type', async () => {
  const response = await post(makeD1(), validReport(), { contentType: 'text/plain' });
  assert.equal(response.status, 415);
  const body = await response.json();
  assert.equal(body.error, 'unsupported_media_type');
});

test('rejects malformed JSON with stable error code', async () => {
  const response = await post(makeD1(), '{ this is not json');
  assert.equal(response.status, 400);
  const body = await response.json();
  assert.equal(body.error, 'invalid_json');
  assert.equal(body.ok, undefined, '错误响应不应包含 ok 字段');
});

test('rejects truncated request body', async () => {
  // 模拟 Request.text() 抛错（网络层截断）。
  const request = new Request('https://zeppbridge.pages.dev/api/feedback', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'content-length': '1000' },
    body: '{"format":"zeppbridge.feedback.v1"',
  });
  Object.defineProperty(request, 'text', {
    value: async () => { throw new Error('network truncated'); },
  });
  const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeD1() } });
  assert.equal(response.status, 400);
  assert.equal((await response.json()).error, 'invalid_request');
});

test('rejects missing required fields', async () => {
  for (const key of Object.keys(validReport())) {
    const bad = { ...validReport() };
    delete bad[key];
    const response = await post(makeD1(), bad);
    assert.equal(response.status, 422, `缺少 ${key} 应被拒绝`);
    assert.equal((await response.json()).error, 'invalid_report');
  }
});

test('rejects oversized body beyond MAX_BODY_BYTES', async () => {
  const bigNote = 'x'.repeat(40 * 1024);
  const response = await post(makeD1(), { ...validReport(), userNote: bigNote });
  assert.equal(response.status, 413);
  assert.equal((await response.json()).error, 'payload_too_large');
});

test('rejects userNote that is too long or wrong type', async () => {
  for (const userNote of ['x'.repeat(501), 42, {}]) {
    const response = await post(makeD1(), { ...validReport(), userNote });
    assert.equal(response.status, 422, `userNote=${typeof userNote} 应被拒绝`);
  }
});

test('rejects invalid category values', async () => {
  for (const category of ['hacked', 'x'.repeat(200), 123]) {
    const response = await post(makeD1(), { ...validReport(), category });
    assert.equal(response.status, 422, `category=${category} 应被拒绝`);
  }
});

test('rejects model identifier hints that contain free text', async () => {
  for (const hints of [
    ['sn:ABC123'],
    ['macAddress:001122334455'],
    ['deviceSource:123456789 extra'],
    ['deviceSource:not-a-number'],
  ]) {
    const report = { ...validReport() };
    report.deviceEvidence = { ...report.deviceEvidence, modelIdentifierHints: hints };
    assert.equal(validateFeedbackReport(report), false, `${hints} 应在校验层被拒`);
  }
});

// ---------------------------------------------------------------------------
// v2.0.0 新增：去重与限流
// ---------------------------------------------------------------------------

test('duplicate submissions return the first report id and store nothing twice', async () => {
  const db = makeD1();
  const first = await post(db, validReport());
  assert.equal(first.status, 201);
  const firstBody = await first.json();

  for (let i = 0; i < 5; i += 1) {
    const response = await post(db, validReport());
    assert.equal(response.status, 200, '重复提交是成功，但没有新建');
    const body = await response.json();
    assert.equal(body.reportId, firstBody.reportId, '必须返回第一份的 id');
    assert.equal(body.duplicate, true);
  }
  assert.equal(db.reportCount(), 1, '五次重复后库里仍然只有一份');
});

test('content hash ignores userNote but nothing else', async () => {
  const db = makeD1();
  const first = await post(db, validReport());
  const firstBody = await first.json();
  assert.equal(first.status, 201);

  // 只改备注：同一份证据，应判定为重复。
  const sameEvidence = await post(db, { ...validReport(), userNote: '换了一句话' });
  assert.equal(sameEvidence.status, 200);
  assert.equal((await sameEvidence.json()).reportId, firstBody.reportId);

  // 改了证据本身（unknownWorkoutCodes）：是新的报告。
  const newEvidence = await post(db, {
    ...validReport(),
    unknownWorkoutCodes: [{ code: 241, records: 1 }],
  });
  assert.equal(newEvidence.status, 201);
  assert.equal(db.reportCount(), 2);
});

test('contentHashInput canonicalises key order', () => {
  const a = contentHashInput({ ...validReport(), unknownWorkoutCodes: [{ code: 240, records: 2 }] });
  const b = contentHashInput({ unknownWorkoutCodes: [{ records: 2, code: 240 }], ...validReport() });
  assert.equal(a, b, '键序不同、内容相同的报告必须同哈希');
});

test('more than RATE_LIMIT_MAX_REPORTS distinct reports from one source gets 429', async () => {
  const db = makeD1();
  // 前 12 份不同内容的报告都成功（固定窗口上限 12）。
  for (let i = 0; i < 12; i += 1) {
    const response = await post(db, {
      ...validReport(),
      unknownWorkoutCodes: [{ code: 100 + i, records: 1 }],
    });
    assert.equal(response.status, 201, `第 ${i + 1} 份应成功`);
  }
  // 第 13 份被限流。
  const rejected = await post(db, { ...validReport(), unknownWorkoutCodes: [{ code: 999, records: 1 }] });
  assert.equal(rejected.status, 429);
  assert.equal((await rejected.json()).error, 'rate_limited');
  assert.equal(db.reportCount(), 12);
});

test('duplicates do not consume the rate limit quota', async () => {
  const db = makeD1();
  await post(db, validReport());
  // 11 份去重重发 + 11 份新内容后，仍然允许第 12 份新内容。
  for (let i = 0; i < 11; i += 1) {
    await post(db, validReport()); // duplicate → 不消耗额度
  }
  for (let i = 0; i < 11; i += 1) {
    const response = await post(db, {
      ...validReport(),
      unknownWorkoutCodes: [{ code: 200 + i, records: 1 }],
    });
    assert.equal(response.status, 201, `新内容第 ${i + 1} 份应成功`);
  }
});

test('a different source ip has its own quota', async () => {
  const db = makeD1();
  for (let i = 0; i < 12; i += 1) {
    const response = await post(db, {
      ...validReport(),
      unknownWorkoutCodes: [{ code: 100 + i, records: 1 }],
    }, { sourceIp: '203.0.113.7' });
    assert.equal(response.status, 201);
  }
  const otherSource = await post(db, {
    ...validReport(),
    unknownWorkoutCodes: [{ code: 500, records: 1 }],
  }, { sourceIp: '198.51.100.9' });
  assert.equal(otherSource.status, 201, '另一个来源不受第一个来源的窗口影响');
});

test('rate limiter failure allows the report instead of dropping it', async () => {
  const db = makeD1();
  db.breakCounters();
  const response = await post(db, validReport());
  assert.equal(response.status, 201, '限流闸坏掉时应放行（防滥用闸 ≠ 数据完整性）');
  assert.equal(db.reportCount(), 1);
});

test('error responses do not leak internal details', async () => {
  const badCases = [
    makeRequest({ ...validReport(), token: 'must-not-appear' }),
    makeRequest({ ...validReport(), category: 'hacked' }),
    makeRequest('{ bad json', { contentType: 'application/json' }),
  ];
  for (const request of badCases) {
    const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeD1() } });
    const text = await response.text();
    const lower = text.toLowerCase();
    assert.doesNotMatch(lower, /token/);
    assert.doesNotMatch(lower, /prepare/);
    assert.doesNotMatch(lower, /sql/);
    assert.doesNotMatch(lower, /stack/);
    assert.doesNotMatch(lower, /trace/);
    assert.doesNotMatch(lower, /at \S+\.js/);
  }
});

test('429 response does not leak source ip or hash', async () => {
  const db = makeD1();
  for (let i = 0; i < 13; i += 1) {
    await post(db, {
      ...validReport(),
      unknownWorkoutCodes: [{ code: 600 + i, records: 1 }],
    }, { sourceIp: '203.0.113.99' });
  }
  const rejected = await post(db, {
    ...validReport(),
    unknownWorkoutCodes: [{ code: 777, records: 1 }],
  }, { sourceIp: '203.0.113.99' });
  assert.equal(rejected.status, 429);
  const text = await rejected.text();
  assert.doesNotMatch(text, /203\.0\.113\.99/);
  assert.doesNotMatch(text, /source_hash|salt/i);
});

// ---------------------------------------------------------------------------
// Release boundary tests
// ---------------------------------------------------------------------------

const releaseFixture = () => ({
  tag_name: 'v2.0.0',
  published_at: '2026-09-01T00:00:00Z',
  html_url: 'https://github.com/lingcang728/ZeppBridge/releases/tag/v2.0.0',
  draft: false,
  prerelease: false,
  assets: [
    { name: 'ZeppBridge_2.0.0_x64-setup.exe', browser_download_url: 'https://example.test/windows.exe', size: 1, digest: 'sha256:a' },
    { name: 'ZeppBridge_2.0.0_x64_en-US.msi', browser_download_url: 'https://example.test/windows.msi', size: 1, digest: 'sha256:b' },
    { name: 'ZeppBridge_2.0.0_aarch64.dmg', browser_download_url: 'https://example.test/macos.dmg', size: 1, digest: 'sha256:c' },
  ],
});

test('rejects malformed JSON from GitHub as incomplete release', async (context) => {
  context.mock.method(globalThis, 'fetch', async () => Response.json({ notARelease: true }));
  const response = await onRequestGet({ request: new Request('https://zeppbridge.pages.dev/api/release'), waitUntil() {} });
  assert.equal(response.status, 502);
  assert.deepEqual(await response.json(), { error: 'latest_release_incomplete' });
});

test('rejects release missing required assets', async (context) => {
  const fixture = releaseFixture();
  fixture.assets = fixture.assets.filter((a) => !a.name.endsWith('.dmg'));
  context.mock.method(globalThis, 'fetch', async () => Response.json(fixture));
  const response = await onRequestGet({ request: new Request('https://zeppbridge.pages.dev/api/release'), waitUntil() {} });
  assert.equal(response.status, 502);
  assert.deepEqual(await response.json(), { error: 'latest_release_incomplete' });
});

test('rejects draft and prerelease releases', () => {
  for (const flag of ['draft', 'prerelease']) {
    const fixture = releaseFixture();
    fixture[flag] = true;
    assert.throws(() => projectLatestRelease(fixture), /published stable release/);
  }
});

test('fails closed when GitHub returns server error', async (context) => {
  context.mock.method(globalThis, 'fetch', async () => new Response('upstream error', { status: 503 }));
  const response = await onRequestGet({ request: new Request('https://zeppbridge.pages.dev/api/release'), waitUntil() {} });
  assert.equal(response.status, 502);
  assert.deepEqual(await response.json(), { error: 'latest_release_unavailable' });
});

test('error responses do not leak upstream paths or diagnostics', async (context) => {
  context.mock.method(globalThis, 'fetch', async () => new Response('upstream error', { status: 503 }));
  const response = await onRequestGet({ request: new Request('https://zeppbridge.pages.dev/api/release'), waitUntil() {} });
  const text = await response.text();
  const lower = text.toLowerCase();
  assert.doesNotMatch(lower, /github\.com/);
  assert.doesNotMatch(lower, /upstream error/);
  assert.doesNotMatch(lower, /api\.github/);
});

test('successful release response has stable cache headers', async (context) => {
  context.mock.method(globalThis, 'fetch', async () => Response.json(releaseFixture()));
  const response = await onRequestGet({ request: new Request('https://zeppbridge.pages.dev/api/release'), waitUntil() {} });
  assert.equal(response.status, 200);
  const cc = response.headers.get('cache-control');
  assert.match(cc, /s-maxage=300/);
  assert.match(cc, /max-age=60/);
});
