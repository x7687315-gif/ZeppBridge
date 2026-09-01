import assert from 'node:assert/strict';
import test from 'node:test';

import { onRequestPost, validateFeedbackReport } from '../functions/api/feedback.js';
import { onRequestGet, projectLatestRelease } from '../functions/api/release.js';

// ---------------------------------------------------------------------------
// Feedback fixtures
// ---------------------------------------------------------------------------

const validReport = () => ({
  format: 'zeppbridge.feedback.v1',
  appVersion: '1.1.5',
  schemaVersion: 16,
  normalizerRevision: 'zepp-normalizer-2026-08-v16-workout-catalog',
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

function makeDb() {
  let boundValues = null;
  return {
    prepare() {
      return {
        bind(...values) {
          boundValues = values;
          return { run: async () => ({ success: true }) };
        },
      };
    },
    lastBound() {
      return boundValues;
    },
  };
}

function makeRequest(body, { contentType = 'application/json', contentLength } = {}) {
  const raw = typeof body === 'string' ? body : JSON.stringify(body);
  const bytes = new TextEncoder().encode(raw);
  const headers = new Map();
  headers.set('content-type', contentType);
  headers.set('content-length', String(contentLength ?? bytes.length));
  return new Request('https://zeppbridge.pages.dev/api/feedback', {
    method: 'POST',
    headers,
    body: raw,
  });
}

// ---------------------------------------------------------------------------
// Feedback boundary tests
// ---------------------------------------------------------------------------

test('rejects non-JSON content-type', async () => {
  const request = makeRequest(validReport(), { contentType: 'text/plain' });
  const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
  assert.equal(response.status, 415);
  const body = await response.json();
  assert.equal(body.error, 'unsupported_media_type');
});

test('rejects malformed JSON with stable error code', async () => {
  const request = makeRequest('{ this is not json', { contentType: 'application/json' });
  const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
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
  const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
  assert.equal(response.status, 400);
  assert.equal((await response.json()).error, 'invalid_request');
});

test('rejects missing required fields', async () => {
  for (const key of Object.keys(validReport())) {
    const bad = { ...validReport() };
    delete bad[key];
    const request = makeRequest(bad);
    const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
    assert.equal(response.status, 422, `缺少 ${key} 应被拒绝`);
    assert.equal((await response.json()).error, 'invalid_report');
  }
});

test('rejects oversized body beyond MAX_BODY_BYTES', async () => {
  const bigNote = 'x'.repeat(40 * 1024);
  const report = { ...validReport(), userNote: bigNote };
  const request = makeRequest(report);
  const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
  assert.equal(response.status, 413);
  assert.equal((await response.json()).error, 'payload_too_large');
});

test('rejects userNote that is too long or wrong type', async () => {
  for (const userNote of ['x'.repeat(501), 42, {}]) {
    const request = makeRequest({ ...validReport(), userNote });
    const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
    assert.equal(response.status, 422, `userNote=${typeof userNote} 应被拒绝`);
  }
});

test('rejects invalid category values', async () => {
  for (const category of ['hacked', 'x'.repeat(200), 123]) {
    const request = makeRequest({ ...validReport(), category });
    const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
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

test('accepts repeated identical submissions', async () => {
  const db = makeDb();
  const request = makeRequest(validReport());
  for (let i = 0; i < 50; i += 1) {
    const response = await onRequestPost({ request, env: { FEEDBACK_DB: db } });
    assert.equal(response.status, 201);
    const body = await response.json();
    assert.match(body.reportId, /^[0-9a-f-]{36}$/);
  }
});

test('error responses do not leak internal details', async () => {
  const badCases = [
    makeRequest({ ...validReport(), token: 'must-not-appear' }),
    makeRequest({ ...validReport(), category: 'hacked' }),
    makeRequest('{ bad json', { contentType: 'application/json' }),
  ];
  for (const request of badCases) {
    const response = await onRequestPost({ request, env: { FEEDBACK_DB: makeDb() } });
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

// ---------------------------------------------------------------------------
// Release boundary tests
// ---------------------------------------------------------------------------

const releaseFixture = () => ({
  tag_name: 'v1.1.2',
  published_at: '2026-08-31T06:50:40Z',
  html_url: 'https://github.com/lingcang728/ZeppBridge/releases/tag/v1.1.2',
  draft: false,
  prerelease: false,
  assets: [
    { name: 'ZeppBridge_1.1.2_x64-setup.exe', browser_download_url: 'https://example.test/windows.exe', size: 1, digest: 'sha256:a' },
    { name: 'ZeppBridge_1.1.2_x64_en-US.msi', browser_download_url: 'https://example.test/windows.msi', size: 1, digest: 'sha256:b' },
    { name: 'ZeppBridge_1.1.2_aarch64.dmg', browser_download_url: 'https://example.test/macos.dmg', size: 1, digest: 'sha256:c' },
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
