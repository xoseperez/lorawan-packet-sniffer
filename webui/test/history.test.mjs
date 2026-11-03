// Tests of the WebUI's pure history logic: src/lib/history.ts.
//
// A plain Node script: no test framework. It loads the TypeScript module
// through the `typescript` package (already a dev dependency), checks the
// shared vectors in history-vectors.json (the Rust router test reads the same
// file; its format is described at the top of that file), and runs the rest of
// the logic: timestamp order, merging, counts, the request state machine and
// the back-off series.
//
//   npm test        (or: node test/history.test.mjs)
//
// Prints one line per case and exits 1 if any failed. Node 20 or later.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import ts from 'typescript';

// ---------------------------------------------------------------------------
// Loading the module under test
// ---------------------------------------------------------------------------

const source = readFileSync(new URL('../src/lib/history.ts', import.meta.url), 'utf8');
const transpiled = ts.transpileModule(source, {
  fileName: 'history.ts',
  reportDiagnostics: true,
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
if (transpiled.diagnostics.length > 0) {
  for (const d of transpiled.diagnostics) {
    console.error(ts.flattenDiagnosticMessageText(d.messageText, '\n'));
  }
  process.exit(1);
}
// A `data:` URL cannot resolve a relative import, so the module must be
// self-contained. `import type` is erased by the transpiler.
if (/^\s*import[\s{*"']/m.test(transpiled.outputText)) {
  console.error('src/lib/history.ts must only use `import type`: the transpiled module still imports.');
  process.exit(1);
}
const H = await import(
  'data:text/javascript;base64,' + Buffer.from(transpiled.outputText).toString('base64')
);

const vectors = JSON.parse(readFileSync(new URL('./history-vectors.json', import.meta.url), 'utf8'));

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const results = [];

function test(name, fn) {
  try {
    fn();
    results.push({ name, ok: true });
    console.log(`ok   - ${name}`);
  } catch (e) {
    results.push({ name, ok: false });
    console.log(`FAIL - ${name}`);
    console.log(String(e && e.stack ? e.stack : e).replace(/^/gm, '       '));
  }
}

const ids = rows => rows.map(r => r.id);
const sign = Math.sign;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const NOW = Date.parse('2026-10-30T14:00:00.000Z');
const BASE = Date.parse('2026-10-30T13:00:00.000Z');

const DEFAULT_FILTERS = {
  device: '', mtype: [], status: 'all', gateway: '', timePreset: '24h', day: null, showMalformed: false,
};
const fstate = (o = {}) => ({ ...DEFAULT_FILTERS, ...o });

/** An ISO time `sec` seconds after BASE. */
const at = sec => new Date(BASE + sec * 1000).toISOString();

function rec(id, sec, o = {}) {
  return {
    id,
    timestamp: at(sec),
    mtype: 'UnconfirmedDataUp',
    status: 'Forwarded',
    gateway_eui: 'AA:BB:CC:DD:00:11:22:33',
    dev_addr: '0A1B2C3D',
    dev_eui: null,
    malformed: null,
    rssi: -50,
    ...o,
  };
}

/** A filter that selects everything. */
const ANY = {
  device: '', mtypes: [], status: 'all', gateway: '', fromMs: null, untilMs: null, hideMalformed: false,
};

// ===========================================================================
// Shared vectors
// ===========================================================================

test('vectors: the file is well formed', () => {
  const { records, cases, labels } = vectors;
  assert.equal(new Set(ids(records)).size, records.length, 'record ids are unique');
  assert.equal(new Set(cases.map(c => c.name)).size, cases.length, 'case names are unique');
  for (let i = 1; i < records.length; i++) {
    assert.ok(
      H.compareTimestamps(records[i - 1].timestamp, records[i].timestamp) < 0,
      `records are in strictly increasing time order at ${records[i].id}`,
    );
  }
  for (const c of cases) {
    const order = ids(records);
    const positions = c.expect.map(id => order.indexOf(id));
    assert.ok(positions.every(p => p >= 0), `${c.name}: expect names known records`);
    assert.deepEqual(positions, [...positions].sort((a, b) => a - b), `${c.name}: expect is in record order`);
  }
  for (const [k, v] of Object.entries(labels)) {
    assert.equal(typeof k, 'string');
    assert.equal(typeof v, 'string');
  }
  // The file has to exercise what it is for.
  assert.ok(records.some(r => r.dev_addr && r.dev_eui), 'a record with both identifiers');
  assert.ok(records.some(r => r.mtype === 'Unknown' && !r.malformed), 'an Unknown record');
  assert.ok(records.some(r => r.status === 'Captured'), 'a Captured record');
  assert.ok(['crc', 'mhdr', 'length'].every(k => records.some(r => r.malformed === k)), 'every malformed kind');
  assert.ok(cases.some(c => c.filter.before !== null && c.filter.from === null), 'a case with only an upper bound');
  assert.ok(cases.some(c => c.filter.before !== null && c.filter.from !== null), 'a case with a window');
  assert.ok(cases.some(c => c.filter.before !== null && c.expect.length === 0), 'a case with an upper bound and no match');
});

function vectorFilter(c) {
  return {
    device: c.filter.device,
    mtypes: c.filter.mtypes,
    status: c.filter.status,
    gateway: c.filter.gateway,
    fromMs: c.filter.from === null ? null : Date.parse(c.filter.from),
    untilMs: c.filter.before === null ? null : Date.parse(c.filter.before),
    hideMalformed: c.filter.hideMalformed,
  };
}

for (const c of vectors.cases) {
  test(`vector: ${c.name}`, () => {
    // Every case states both bounds, null when it has none.
    assert.ok(c.filter.from === null || typeof c.filter.from === 'string', 'from is null or text');
    assert.ok(c.filter.before === null || typeof c.filter.before === 'string', 'before is null or text');
    const f = vectorFilter(c);
    if (f.fromMs !== null && f.untilMs !== null) {
      assert.ok(f.untilMs > f.fromMs, 'a window ends after it starts (the server refuses the reverse)');
    }

    // The query string the browser builds is the one in the file.
    assert.equal(H.historyParams(f), c.query);
    assert.equal(
      H.loadQuery(f),
      (c.query === '' ? '' : c.query + '&') + 'newest=true&count=true&settle=true&limit=5000',
    );

    // ... and it decodes back to the filter: a plus never turns into a space.
    const p = new URLSearchParams(c.query);
    assert.equal(p.get('device') ?? '', f.device);
    assert.deepEqual((p.get('mtype') ?? '').split(',').filter(Boolean), f.mtypes);
    assert.equal(p.get('status') ?? 'all', f.status);
    assert.equal(p.get('gateway') ?? '', f.gateway);
    assert.equal(p.get('from'), c.filter.from);
    assert.equal(p.get('before'), c.filter.before);
    assert.equal(p.get('malformed'), f.hideMalformed ? 'exclude' : null);
    if (f.fromMs !== null) assert.equal(new Date(f.fromMs).toISOString(), c.filter.from);
    if (f.untilMs !== null) assert.equal(new Date(f.untilMs).toISOString(), c.filter.before);

    // The browser keeps exactly these records.
    const kept = vectors.records.filter(r => H.matchesFilter(r, f, vectors.labels));
    assert.deepEqual(ids(kept), c.expect);

    // The malformed figure counts them whether the box hides them or not.
    const malformed = vectors.records.filter(r => H.countsAsMalformed(r, f, vectors.labels));
    assert.equal(malformed.length, c.expectMalformed);
  });
}

// ===========================================================================
// matchesFilter
// ===========================================================================

test('matchesFilter: serverMatchedDevice keeps a row whose only match is a label not known yet', () => {
  const row = rec('x', 0, { dev_addr: '11112222' });
  const f = { ...ANY, device: 'sensor' };
  assert.equal(H.matchesFilter(row, f, {}), false, 'no label known: hidden');
  assert.equal(H.matchesFilter(row, f, {}, { serverMatchedDevice: true }), true, 'trusted: kept');
  assert.equal(H.matchesFilter(row, f, { '11112222': 'Cold Sensor' }), true, 'label known: kept');
  assert.equal(H.matchesFilter(row, f, {}, { serverMatchedDevice: false }), false, 'option off: hidden again');
  // The option only waives the device rule.
  assert.equal(
    H.matchesFilter(row, { ...f, status: 'Blocked' }, {}, { serverMatchedDevice: true }),
    false,
  );
});

test('matchesFilter: the time bound compares milliseconds, not nanoseconds', () => {
  const from = Date.parse('2026-10-30T12:00:00.000Z');
  const f = { ...ANY, fromMs: from };
  const inAt = ts => H.matchesFilter({ ...rec('x', 0), timestamp: ts }, f, {});
  assert.equal(inAt('2026-10-30T12:00:00Z'), true);
  assert.equal(inAt('2026-10-30T12:00:00.000000001Z'), true);
  assert.equal(inAt('2026-10-30T12:00:00.000999999Z'), true);
  assert.equal(inAt('2026-10-30T11:59:59.999999999Z'), false);
  assert.equal(inAt('2026-10-30T11:59:59.999Z'), false);
  assert.equal(inAt('not a time'), false);
});

test('matchesFilter: a record without identifiers or gateway', () => {
  const bare = rec('x', 0, { dev_addr: null, dev_eui: null, gateway_eui: null });
  assert.equal(H.matchesFilter(bare, { ...ANY, device: 'a' }, { null: 'a' }), false);
  assert.equal(H.matchesFilter(bare, { ...ANY, gateway: 'AA' }, {}), false);
  assert.equal(H.matchesFilter(bare, ANY, {}), true);
});

test('onlyMalformedFilter: the time bound is not a row filter', () => {
  assert.equal(H.onlyMalformedFilter({ ...ANY, hideMalformed: true, fromMs: 5 }), true);
  assert.equal(H.onlyMalformedFilter({ ...ANY, hideMalformed: false }), false);
  assert.equal(H.onlyMalformedFilter({ ...ANY, hideMalformed: true, device: 'a' }), false);
  assert.equal(H.onlyMalformedFilter({ ...ANY, hideMalformed: true, mtypes: ['JoinRequest'] }), false);
  assert.equal(H.onlyMalformedFilter({ ...ANY, hideMalformed: true, status: 'Blocked' }), false);
  assert.equal(H.onlyMalformedFilter({ ...ANY, hideMalformed: true, gateway: 'G' }), false);
});

// ===========================================================================
// Presets
// ===========================================================================

const PRESETS = { '5m': 5 * 60e3, '15m': 15 * 60e3, '1h': 60 * 60e3, '24h': 24 * 60 * 60e3 };

for (const [preset, span] of Object.entries(PRESETS)) {
  test(`presets: ${preset} is now minus ${span} ms, as the server parses it`, () => {
    const f = H.toHistoryFilter(fstate({ timePreset: preset, showMalformed: true }), NOW);
    assert.equal(f.fromMs, NOW - span);
    const iso = new Date(NOW - span).toISOString();
    assert.equal(H.historyParams(f), `from=${encodeURIComponent(iso)}`);
    assert.equal(new URLSearchParams(H.historyParams(f)).get('from'), iso);
    // The value is on a whole millisecond, with the Z the server accepts.
    assert.match(iso, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
    assert.equal(Date.parse(iso), f.fromMs);
  });
}

test('presets: a saved range the selector no longer offers loads like the default span', () => {
  // An older "all time", an unknown word, nothing, and a day that is not a day.
  const unusable = [
    { timePreset: 'all' },
    { timePreset: 'week' },
    { timePreset: undefined },
    { timePreset: 'day', day: null },
    { timePreset: 'day', day: 'tomorrow' },
    { timePreset: 'day', day: '2026-02-30' },
  ];
  for (const state of unusable) {
    const f = H.toHistoryFilter(fstate(state), NOW);
    assert.equal(f.fromMs, NOW - PRESETS['5m'], JSON.stringify(state));
    assert.equal(f.untilMs, null, JSON.stringify(state));
  }
});

test('toHistoryFilter: carries every field of the filter bar', () => {
  const f = H.toHistoryFilter(
    fstate({ device: 'Ab', mtype: ['JoinRequest'], status: 'Blocked', gateway: 'GW', showMalformed: true }),
    NOW,
  );
  assert.deepEqual(f, {
    device: 'Ab', mtypes: ['JoinRequest'], status: 'Blocked', gateway: 'GW',
    fromMs: NOW - PRESETS['24h'], untilMs: null, hideMalformed: false,
  });
  assert.equal(H.toHistoryFilter(fstate({ showMalformed: false }), NOW).hideMalformed, true);
});

// ===========================================================================
// Days
// ===========================================================================

/** Runs `fn` with the process time zone set to `tz`, then puts the old one back. */
function withTimeZone(tz, fn) {
  const saved = process.env.TZ;
  process.env.TZ = tz;
  try {
    return fn();
  } finally {
    if (saved === undefined) delete process.env.TZ;
    else process.env.TZ = saved;
  }
}

const HOUR = 3600e3;
const iso = ms => new Date(ms).toISOString();

test('withTimeZone: the zone really changes and is restored', () => {
  const before = new Date(2026, 6, 1).getTimezoneOffset();
  assert.equal(withTimeZone('Europe/Madrid', () => new Date(2026, 6, 1).getTimezoneOffset()), -120);
  assert.equal(withTimeZone('Europe/Madrid', () => new Date(2026, 0, 1).getTimezoneOffset()), -60);
  assert.equal(withTimeZone('UTC', () => new Date(2026, 6, 1).getTimezoneOffset()), 0);
  assert.equal(withTimeZone('Pacific/Auckland', () => new Date(2026, 6, 1).getTimezoneOffset()), -720);
  assert.equal(new Date(2026, 6, 1).getTimezoneOffset(), before);
});

test('dayBounds: Europe/Madrid, a 23-hour day, a 25-hour day and an ordinary one', () => {
  withTimeZone('Europe/Madrid', () => {
    // Clocks go forward on 2026-03-29: the day is 23 hours long.
    const spring = H.dayBounds('2026-03-29');
    assert.equal(iso(spring.fromMs), '2026-03-28T23:00:00.000Z');
    assert.equal(iso(spring.untilMs), '2026-03-29T22:00:00.000Z');
    assert.equal(spring.untilMs - spring.fromMs, 23 * HOUR);

    // Clocks go back on 2026-10-25: the day is 25 hours long.
    const autumn = H.dayBounds('2026-10-25');
    assert.equal(iso(autumn.fromMs), '2026-10-24T22:00:00.000Z');
    assert.equal(iso(autumn.untilMs), '2026-10-25T23:00:00.000Z');
    assert.equal(autumn.untilMs - autumn.fromMs, 25 * HOUR);

    // The days around them are ordinary, and each starts where the last ended.
    const before = H.dayBounds('2026-10-24');
    const after = H.dayBounds('2026-10-26');
    assert.equal(iso(before.fromMs), '2026-10-23T22:00:00.000Z');
    assert.equal(before.untilMs, autumn.fromMs);
    assert.equal(autumn.untilMs, after.fromMs);
    assert.equal(after.untilMs - after.fromMs, 24 * HOUR);

    const winter = H.dayBounds('2026-11-01');
    assert.equal(iso(winter.fromMs), '2026-10-31T23:00:00.000Z');
    assert.equal(iso(winter.untilMs), '2026-11-01T23:00:00.000Z');
    const summer = H.dayBounds('2026-07-15');
    assert.equal(iso(summer.fromMs), '2026-07-14T22:00:00.000Z');
    assert.equal(summer.untilMs - summer.fromMs, 24 * HOUR);
  });
});

test('dayBounds: UTC is midnight to midnight UTC, across month, year and leap day', () => {
  withTimeZone('UTC', () => {
    const day = H.dayBounds('2026-11-01');
    assert.equal(iso(day.fromMs), '2026-11-01T00:00:00.000Z');
    assert.equal(iso(day.untilMs), '2026-11-02T00:00:00.000Z');
    assert.equal(iso(H.dayBounds('2026-12-31').untilMs), '2027-01-01T00:00:00.000Z');
    assert.equal(iso(H.dayBounds('2026-01-31').untilMs), '2026-02-01T00:00:00.000Z');
    assert.equal(iso(H.dayBounds('2028-02-29').untilMs), '2028-03-01T00:00:00.000Z');
    assert.equal(iso(H.dayBounds('2028-02-29').fromMs), '2028-02-29T00:00:00.000Z');
  });
});

test('dayBounds: a zone west of Greenwich, where the offset is negative', () => {
  withTimeZone('America/Los_Angeles', () => {
    // Clocks go back on 2026-11-01 at 02:00 local: 25 hours, from PDT to PST.
    const day = H.dayBounds('2026-11-01');
    assert.equal(iso(day.fromMs), '2026-11-01T07:00:00.000Z');
    assert.equal(iso(day.untilMs), '2026-11-02T08:00:00.000Z');
  });
});

test('dayBounds: refuses what is not a day', () => {
  for (const bad of ['', 'tomorrow', '2026-02-30', '2026-13-01', '2026-1-5', '0002-11-02']) {
    assert.throws(() => H.dayBounds(bad), RangeError, bad);
  }
});

test('todayKey: the local date, just before and just after local midnight', () => {
  withTimeZone('Europe/Madrid', () => {
    // Winter: local midnight of 2 November is 23:00 UTC on the 1st.
    assert.equal(H.todayKey(Date.parse('2026-11-01T22:59:59.999Z')), '2026-11-01');
    assert.equal(H.todayKey(Date.parse('2026-11-01T23:00:00.000Z')), '2026-11-02');
    // Summer: local midnight of 25 October is 22:00 UTC on the 24th.
    assert.equal(H.todayKey(Date.parse('2026-10-24T21:59:59.999Z')), '2026-10-24');
    assert.equal(H.todayKey(Date.parse('2026-10-24T22:00:00.000Z')), '2026-10-25');
    // Late evening UTC is already the next day in Madrid, never the other way round at noon.
    assert.equal(H.todayKey(Date.parse('2026-06-30T23:30:00.000Z')), '2026-07-01');
    assert.equal(H.todayKey(Date.parse('2026-07-01T12:00:00.000Z')), '2026-07-01');
  });
  withTimeZone('UTC', () => {
    assert.equal(H.todayKey(Date.parse('2026-11-01T23:59:59.999Z')), '2026-11-01');
    assert.equal(H.todayKey(Date.parse('2026-11-02T00:00:00.000Z')), '2026-11-02');
    assert.equal(H.todayKey(Date.parse('2026-01-05T09:00:00.000Z')), '2026-01-05', 'month and day are zero-padded');
  });
  withTimeZone('America/Los_Angeles', () => {
    assert.equal(H.todayKey(Date.parse('2026-11-02T07:59:59.999Z')), '2026-11-01');
    assert.equal(H.todayKey(Date.parse('2026-11-02T08:00:00.000Z')), '2026-11-02');
  });
});

test('todayKey and dayBounds agree: now lies inside today', () => {
  for (const tz of ['Europe/Madrid', 'UTC', 'America/Los_Angeles', 'Pacific/Auckland']) {
    withTimeZone(tz, () => {
      for (const now of ['2026-03-29T00:30:00Z', '2026-03-29T01:30:00Z', '2026-10-25T00:30:00Z', '2026-10-25T01:30:00Z',
                         '2026-11-01T09:00:00Z', '2026-12-31T23:59:59Z']) {
        const ms = Date.parse(now);
        const { fromMs, untilMs } = H.dayBounds(H.todayKey(ms));
        assert.ok(fromMs <= ms && ms < untilMs, `${tz} ${now}`);
      }
    });
  }
});

test('pickDay: a day up to today is taken, a day to come and anything else is not', () => {
  const today = '2026-11-02';
  assert.equal(H.pickDay('2026-11-02', today), '2026-11-02', 'today');
  assert.equal(H.pickDay('2026-11-01', today), '2026-11-01', 'yesterday');
  assert.equal(H.pickDay('2026-10-25', today), '2026-10-25');
  assert.equal(H.pickDay('2025-12-31', today), '2025-12-31', 'across a year end');
  assert.equal(H.pickDay('2026-11-03', today), null, 'tomorrow');
  assert.equal(H.pickDay('2027-01-01', today), null, 'next year');
  assert.equal(H.pickDay('2026-12-01', '2026-11-30'), null, 'a later month');

  for (const bad of ['', 'abc', '2026-1-5', '2026-11-1', '26-11-01', '2026/11/01', '2026-11-01T00:00',
                     ' 2026-11-01', '2026-11-01 ', '20261101', '2026-11-01\n']) {
    assert.equal(H.pickDay(bad, today), null, JSON.stringify(bad));
  }
  // A date that does not exist is not rolled over into the next month.
  for (const impossible of ['2026-02-30', '2026-02-29', '2026-04-31', '2026-13-01', '2026-00-10', '2026-11-00', '2026-11-32']) {
    assert.equal(H.pickDay(impossible, today), null, impossible);
  }
  assert.equal(H.pickDay('2028-02-29', '2028-03-01'), '2028-02-29', 'a leap day');
  assert.equal(H.pickDay('2026-02-28', today), '2026-02-28');
  // The year a date input has while it is still being typed.
  assert.equal(H.pickDay('0002-11-02', today), null);
  assert.equal(H.pickDay('0202-11-02', today), null);
  assert.equal(H.pickDay('1969-12-31', today), null);
  assert.equal(H.pickDay('1970-01-01', today), '1970-01-01');
});

test('pickDay: needs a real "today" too', () => {
  assert.equal(H.pickDay('2026-11-01', ''), null);
  assert.equal(H.pickDay('2026-11-01', 'today'), null);
});

test('normaliseFilters: a range the selector no longer offers becomes the default span', () => {
  const kept = { device: 'ab', mtype: ['JoinRequest'], status: 'Blocked', gateway: 'GW', showMalformed: true };
  for (const [name, state] of [
    ['an older "all"', { timePreset: 'all', day: null }],
    ['an older "all" without a day at all', { timePreset: 'all', day: undefined }],
    ['an unknown word', { timePreset: 'week', day: null }],
    ['no range', { timePreset: undefined, day: null }],
    ['"all" with a day left over', { timePreset: 'all', day: '2026-11-01' }],
  ]) {
    const n = H.normaliseFilters(fstate({ ...kept, ...state }));
    assert.deepEqual(n, fstate({ ...kept, timePreset: '5m', day: null }), name);
  }
});

test('normaliseFilters: "day" needs a valid day, else it is the default span', () => {
  for (const day of [null, undefined, '', 'junk', '2026-02-30', '2026-11-1', '0002-11-02']) {
    const n = H.normaliseFilters(fstate({ timePreset: 'day', day }));
    assert.equal(n.timePreset, '5m', String(day));
    assert.equal(n.day, null, String(day));
  }
});

test('normaliseFilters: a valid day is kept, a span carries no day', () => {
  const day = fstate({ timePreset: 'day', day: '2026-10-25', device: 'ab' });
  assert.equal(H.normaliseFilters(day), day, 'the same object when nothing changes');

  for (const preset of ['5m', '15m', '1h', '24h']) {
    const n = H.normaliseFilters(fstate({ timePreset: preset, day: '2026-10-25' }));
    assert.equal(n.timePreset, preset);
    assert.equal(n.day, null, `${preset} keeps no day`);
  }

  const plain = fstate({ timePreset: '1h' });
  assert.equal(H.normaliseFilters(plain), plain, 'an ordinary state is returned as it is');
  // A state saved before days existed has no `day` at all.
  const old = { ...plain };
  delete old.day;
  assert.deepEqual(H.normaliseFilters(old), { ...plain, day: null });
});

test('normaliseFilters: the other fields are untouched', () => {
  const stored = fstate({
    device: 'Ab', mtype: ['JoinRequest'], status: 'Blocked', gateway: 'GW', showMalformed: true, timePreset: 'all',
  });
  const n = H.normaliseFilters(stored);
  assert.equal(n.device, 'Ab');
  assert.deepEqual(n.mtype, ['JoinRequest']);
  assert.equal(n.status, 'Blocked');
  assert.equal(n.gateway, 'GW');
  assert.equal(n.showMalformed, true);
  assert.equal(stored.timePreset, 'all', 'the stored object is not changed');
});

test('toHistoryFilter: a day is its two local midnights, whatever the time is now', () => {
  withTimeZone('Europe/Madrid', () => {
    const state = fstate({ timePreset: 'day', day: '2026-10-25', device: 'ab', showMalformed: true });
    const f = H.toHistoryFilter(state, NOW);
    assert.equal(iso(f.fromMs), '2026-10-24T22:00:00.000Z');
    assert.equal(iso(f.untilMs), '2026-10-25T23:00:00.000Z');
    assert.equal(f.device, 'ab');
    assert.equal(f.hideMalformed, false);
    // The bounds do not move with the clock, so a day loaded now and checked
    // against later live records agree.
    assert.deepEqual(H.toHistoryFilter(state, NOW + 7 * 24 * HOUR), f);
    assert.deepEqual(H.toHistoryFilter(state, 0), f);
  });
});

test('toHistoryFilter: a span has no end', () => {
  for (const preset of Object.keys(PRESETS)) {
    assert.equal(H.toHistoryFilter(fstate({ timePreset: preset, day: '2026-10-25' }), NOW).untilMs, null);
  }
});

test('historyParams: from, before, device, mtype, status, gateway, malformed, in that order', () => {
  const f = {
    device: 'a+b c', mtypes: ['JoinRequest', 'ConfirmedDataUp'], status: 'Blocked', gateway: 'AA:BB',
    fromMs: Date.parse('2026-10-24T22:00:00.000Z'), untilMs: Date.parse('2026-10-25T23:00:00.000Z'),
    hideMalformed: true,
  };
  assert.equal(
    H.historyParams(f),
    'from=2026-10-24T22%3A00%3A00.000Z&before=2026-10-25T23%3A00%3A00.000Z' +
    '&device=a%2Bb%20c&mtype=JoinRequest,ConfirmedDataUp&status=Blocked&gateway=AA%3ABB&malformed=exclude',
  );
  // It decodes back to the bounds, exactly.
  const p = new URLSearchParams(H.historyParams(f));
  assert.equal(p.get('from'), '2026-10-24T22:00:00.000Z');
  assert.equal(p.get('before'), '2026-10-25T23:00:00.000Z');
  assert.equal(Date.parse(p.get('before')), f.untilMs);
  assert.equal(p.get('device'), 'a+b c');
  assert.deepEqual([...p.keys()], ['from', 'before', 'device', 'mtype', 'status', 'gateway', 'malformed']);

  // `before` alone, and with only the lower bound.
  assert.equal(H.historyParams({ ...ANY, untilMs: f.untilMs }), 'before=2026-10-25T23%3A00%3A00.000Z');
  assert.equal(H.historyParams({ ...ANY, fromMs: f.fromMs }), 'from=2026-10-24T22%3A00%3A00.000Z');
  assert.equal(H.historyParams({ ...ANY, untilMs: f.untilMs, device: 'x' }), 'before=2026-10-25T23%3A00%3A00.000Z&device=x');
  assert.equal(H.historyParams(ANY), '');

  // The whole query keeps the filters in front of the paging parameters.
  assert.equal(
    H.loadQuery({ ...ANY, fromMs: f.fromMs, untilMs: f.untilMs }),
    'from=2026-10-24T22%3A00%3A00.000Z&before=2026-10-25T23%3A00%3A00.000Z' +
    '&newest=true&count=true&settle=true&limit=5000',
  );
});

test('historyParams: a day under a time zone sends the two midnights as UTC instants', () => {
  withTimeZone('Europe/Madrid', () => {
    const f = H.toHistoryFilter(fstate({ timePreset: 'day', day: '2026-10-25', showMalformed: true }), NOW);
    assert.equal(
      H.historyParams(f),
      'from=2026-10-24T22%3A00%3A00.000Z&before=2026-10-25T23%3A00%3A00.000Z',
    );
    // Whole milliseconds with a Z, as the server parses them.
    for (const v of new URLSearchParams(H.historyParams(f)).values()) {
      assert.match(v, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
    }
  });
});

test('rangeWords: every preset, a day, and a range that cannot be used', () => {
  assert.equal(H.rangeWords(fstate({ timePreset: '5m' })), 'the last 5 min');
  assert.equal(H.rangeWords(fstate({ timePreset: '15m' })), 'the last 15 min');
  assert.equal(H.rangeWords(fstate({ timePreset: '1h' })), 'the last hour');
  assert.equal(H.rangeWords(fstate({ timePreset: '24h' })), 'the last 24 h');
  assert.equal(H.rangeWords(fstate({ timePreset: 'day', day: '2026-11-01' })), 'on 2026-11-01');
  assert.equal(H.rangeWords(fstate({ timePreset: 'day', day: '2026-10-25' })), 'on 2026-10-25');
  // A span ignores a day left in the state.
  assert.equal(H.rangeWords(fstate({ timePreset: '1h', day: '2026-11-01' })), 'the last hour');
  // What cannot be used reads as the range that is loaded for it: the default span.
  assert.equal(H.rangeWords(fstate({ timePreset: 'all' })), 'the last 5 min');
  assert.equal(H.rangeWords(fstate({ timePreset: 'day', day: null })), 'the last 5 min');
  assert.equal(H.rangeWords(fstate({ timePreset: 'day', day: '2026-02-30' })), 'the last 5 min');
  assert.equal(H.rangeWords(fstate({ timePreset: 'toString' })), 'the last 5 min', 'not an inherited property');
});

test('matchesFilter: the end of a day is exclusive, the start inclusive, to the nanosecond', () => {
  withTimeZone('Europe/Madrid', () => {
    const f = H.toHistoryFilter(fstate({ timePreset: 'day', day: '2026-10-25' }), NOW);
    assert.equal(iso(f.untilMs), '2026-10-25T23:00:00.000Z');
    const inDay = ts => H.matchesFilter({ ...rec('x', 0), timestamp: ts }, f, {});

    // The end.
    assert.equal(inDay('2026-10-25T22:59:59.999Z'), true, '1 ms before the end');
    assert.equal(inDay('2026-10-25T22:59:59.999999999Z'), true, 'a nanosecond before the end');
    assert.equal(inDay('2026-10-25T23:00:00.000Z'), false, 'exactly at the end');
    assert.equal(inDay('2026-10-25T23:00:00Z'), false, 'exactly at the end, no fraction');
    assert.equal(inDay('2026-10-25T23:00:00.000000000Z'), false, 'exactly at the end, nine digits');
    assert.equal(inDay('2026-10-25T23:00:00.000000001Z'), false, 'a nanosecond after the end');
    assert.equal(inDay('2026-10-25T23:00:00.001Z'), false, '1 ms after the end');
    // The start.
    assert.equal(inDay('2026-10-24T21:59:59.999Z'), false, '1 ms before the start');
    assert.equal(inDay('2026-10-24T21:59:59.999999999Z'), false, 'a nanosecond before the start');
    assert.equal(inDay('2026-10-24T22:00:00.000Z'), true, 'exactly at the start');
    assert.equal(inDay('2026-10-24T22:00:00.000000001Z'), true, 'a nanosecond after the start');
    // The middle, and the neighbours.
    assert.equal(inDay('2026-10-25T12:00:00.000Z'), true);
    assert.equal(inDay('2026-10-25T00:30:00.000Z'), true, 'the repeated hour of the change is inside');
    assert.equal(inDay('2026-10-24T12:00:00.000Z'), false);
    assert.equal(inDay('2026-10-26T12:00:00.000Z'), false);
    assert.equal(inDay('not a time'), false);
  });
});

test('matchesFilter: a span has no end, so a record from the future still matches', () => {
  const f = H.toHistoryFilter(fstate({ timePreset: '5m' }), NOW);
  const future = { ...rec('x', 0), timestamp: iso(NOW + 365 * 24 * HOUR) };
  assert.equal(H.matchesFilter(future, f, {}), true);
});

test('matchesFilter: the end bound combines with the other filters', () => {
  const f = { ...ANY, untilMs: BASE + 10_000, status: 'Blocked' };
  assert.equal(H.matchesFilter(rec('a', 9, { status: 'Blocked' }), f, {}), true);
  assert.equal(H.matchesFilter(rec('b', 10, { status: 'Blocked' }), f, {}), false, 'at the end');
  assert.equal(H.matchesFilter(rec('c', 9), f, {}), false, 'inside, wrong status');
  // The malformed figure respects the window too.
  const bad = ts => ({ ...rec('m', 0, { malformed: 'crc', dev_addr: null }), timestamp: ts });
  assert.equal(H.countsAsMalformed(bad(at(9)), f, {}), false, 'status is still a filter');
  const win = { ...ANY, untilMs: BASE + 10_000 };
  assert.equal(H.countsAsMalformed(bad(at(9)), win, {}), true);
  assert.equal(H.countsAsMalformed(bad(at(10)), win, {}), false, 'at the end');
});

// ===========================================================================
// compareTimestamps
// ===========================================================================

test('compareTimestamps: 0, 3, 6 and 9 fraction digits are compared as times', () => {
  const c = H.compareTimestamps;
  assert.equal(c('2026-10-30T12:00:00Z', '2026-10-30T12:00:00.000Z'), 0);
  assert.equal(c('2026-10-30T12:00:00.000Z', '2026-10-30T12:00:00.000000Z'), 0);
  assert.equal(c('2026-10-30T12:00:00.000000Z', '2026-10-30T12:00:00.000000000Z'), 0);
  assert.equal(sign(c('2026-10-30T12:00:00Z', '2026-10-30T12:00:00.001Z')), -1);
  assert.equal(sign(c('2026-10-30T12:00:00.001Z', '2026-10-30T12:00:00Z')), 1);
  assert.equal(sign(c('2026-10-30T12:00:00.000Z', '2026-10-30T12:00:00.000001Z')), -1);
  assert.equal(sign(c('2026-10-30T12:00:00.000000Z', '2026-10-30T12:00:00.000000001Z')), -1);
  assert.equal(sign(c('2026-10-30T12:00:00.123456789Z', '2026-10-30T12:00:00.123456788Z')), 1);
});

test('compareTimestamps: .500Z is before .500001Z, whatever the text says', () => {
  const a = '2026-10-30T12:00:00.500Z';
  const b = '2026-10-30T12:00:00.500001Z';
  assert.equal(sign(H.compareTimestamps(a, b)), -1);
  assert.equal(sign(H.compareTimestamps(b, a)), 1);
  // As text the order is the other way round, which is what a plain string
  // sort would get wrong.
  assert.ok('2026-10-30T12:00:00.5Z' > b);
  assert.equal(sign(H.compareTimestamps('2026-10-30T12:00:00.5Z', b)), -1);
});

test('compareTimestamps: two rows inside one millisecond keep their true order', () => {
  const a = '2026-10-30T12:00:00.123456Z';
  const b = '2026-10-30T12:00:00.123789Z';
  assert.equal(Date.parse(a), Date.parse(b), 'Date.parse alone cannot tell them apart');
  assert.equal(sign(H.compareTimestamps(a, b)), -1);
  assert.equal(sign(H.compareTimestamps(b, a)), 1);
});

test('compareTimestamps: offsets, and a list of mixed forms sorts as times', () => {
  assert.equal(H.compareTimestamps('2026-10-30T14:00:00+02:00', '2026-10-30T12:00:00Z'), 0);
  const sorted = [
    '2026-10-30T12:00:00Z',
    '2026-10-30T12:00:00.5Z',
    '2026-10-30T12:00:00.500Z',
    '2026-10-30T12:00:00.500000001Z',
    '2026-10-30T12:00:00.500001Z',
    '2026-10-30T12:00:00.501Z',
    '2026-10-30T12:00:01Z',
  ];
  const shuffled = [sorted[4], sorted[0], sorted[6], sorted[2], sorted[5], sorted[3], sorted[1]];
  const result = shuffled.sort(H.compareTimestamps);
  // Equal times (.5 and .500) may come in either order; compare as times.
  assert.deepEqual(
    result.map(t => H.parseTimestampMs(t)),
    sorted.map(t => H.parseTimestampMs(t)),
  );
  for (let i = 1; i < result.length; i++) assert.ok(H.compareTimestamps(result[i - 1], result[i]) <= 0);
});

// ===========================================================================
// mergeRecords
// ===========================================================================

test('mergeRecords: duplicates by id appear once', () => {
  const m = H.mergeRecords([rec('a', 1), rec('b', 2)], [rec('b', 2), rec('c', 3)], 100);
  assert.deepEqual(ids(m.records), ['a', 'b', 'c']);
  assert.equal(m.dropped, 0);
  const d = H.mergeRecords([rec('a', 1), rec('a', 1)], [rec('b', 2), rec('b', 2)], 100);
  assert.deepEqual(ids(d.records), ['a', 'b'], 'duplicates inside one side too');
});

test('mergeRecords: oldest first', () => {
  const m = H.mergeRecords([rec('b', 20), rec('c', 30)], [rec('a', 10), rec('d', 40)], 100);
  assert.deepEqual(ids(m.records), ['a', 'b', 'c', 'd']);
});

test('mergeRecords: rows with equal times keep the order they were added in', () => {
  const m = H.mergeRecords([rec('a', 5), rec('b', 5)], [rec('c', 5)], 100);
  assert.deepEqual(ids(m.records), ['a', 'b', 'c']);
});

test('mergeRecords: sub-millisecond order', () => {
  const x = { ...rec('x', 0), timestamp: '2026-10-30T12:00:00.123789Z' };
  const y = { ...rec('y', 0), timestamp: '2026-10-30T12:00:00.123456Z' };
  assert.deepEqual(ids(H.mergeRecords([x], [y], 10).records), ['y', 'x']);
});

test('mergeRecords: max keeps the newest and counts what it dropped', () => {
  const rows = [1, 2, 3, 4, 5].map(n => rec('r' + n, n));
  const m = H.mergeRecords(rows.slice(0, 3), rows.slice(3), 3);
  assert.deepEqual(ids(m.records), ['r3', 'r4', 'r5']);
  assert.equal(m.dropped, 2);
  const exact = H.mergeRecords(rows, [], 5);
  assert.equal(exact.dropped, 0);
  assert.equal(exact.records.length, 5);
  const roomy = H.mergeRecords(rows, [], 50);
  assert.equal(roomy.dropped, 0);
  const none = H.mergeRecords(rows, [], 0);
  assert.deepEqual(none, { records: [], dropped: 5 });
});

test('mergeRecords: out-of-order timestamps land in place', () => {
  // Another gateway's task stamped this record first but it arrived last.
  const held = [rec('a', 10), rec('b', 30), rec('c', 40)];
  const m = H.mergeRecords(held, [rec('late', 20)], 100);
  assert.deepEqual(ids(m.records), ['a', 'late', 'b', 'c']);
  // And one older than everything held can be cut by max, counted as dropped.
  const old = H.mergeRecords(held, [rec('ancient', 1)], 3);
  assert.deepEqual(ids(old.records), ['a', 'b', 'c']);
  assert.equal(old.dropped, 1);
});

test('mergeRecords: a differing copy of one id: the base copy wins', () => {
  const fetched = rec('a', 10, { rssi: -50 });
  const live = rec('a', 10, { rssi: -99 });
  const m = H.mergeRecords([fetched], [live], 10);
  assert.equal(m.records.length, 1);
  assert.equal(m.records[0].rssi, -50);
  assert.equal(m.records[0], fetched);
});

test('mergeRecords: does not change its inputs', () => {
  const base = [rec('b', 2), rec('a', 1)];
  const extra = [rec('c', 0)];
  const copyBase = structuredClone(base);
  const copyExtra = structuredClone(extra);
  H.mergeRecords(base, extra, 2);
  assert.deepEqual(base, copyBase);
  assert.deepEqual(extra, copyExtra);
});

// ===========================================================================
// mergeAfterLoad
// ===========================================================================

const live = (sinceSent = [], held = []) => ({ sinceSent, held });

test('mergeAfterLoad: a live row received after the load was sent and missing from it is carried, whatever its time', () => {
  const result = [rec('r1', 100), rec('r2', 110), rec('r3', 120)];
  const newer = rec('newer', 200);
  const older = rec('older', 5); // older than the result's oldest row, as in a capped result
  const middle = rec('middle', 105);
  const m = H.mergeAfterLoad(result, true, live([newer, older, middle]), ANY, {}, 100);
  assert.deepEqual(ids(m.records), ['older', 'r1', 'middle', 'r2', 'r3', 'newer']);
  assert.equal(m.dropped, 0);
});

test('mergeAfterLoad: a live row also in the result appears once, as the fetched copy', () => {
  const fetched = rec('a', 100, { rssi: -50 });
  const copy = rec('a', 100, { rssi: -99 });
  const m = H.mergeAfterLoad([fetched], true, live([copy], [copy]), ANY, {}, 100);
  assert.equal(m.records.length, 1);
  assert.equal(m.records[0], fetched);
});

test('mergeAfterLoad: a row held from before the load and missing from its result is dropped', () => {
  // Rows from earlier loads are not passed at all: a load replaces them.
  const m = H.mergeAfterLoad([rec('r1', 100)], true, live([], []), ANY, {}, 100);
  assert.deepEqual(ids(m.records), ['r1']);
  // A live row received before the load was sent is in its snapshot; a settled
  // result that lacks it has it beyond the cap or out of the range.
  const before = rec('before', 90);
  const settled = H.mergeAfterLoad([rec('r1', 100)], true, live([], [before]), ANY, {}, 100);
  assert.deepEqual(ids(settled.records), ['r1']);
});

test('mergeAfterLoad: an unsettled result keeps every live row it lacks, a settled one does not', () => {
  const before = rec('before', 90);
  const after = rec('after', 105);
  const result = [rec('r1', 100)];
  const unsettled = H.mergeAfterLoad(result, false, live([after], [before, after]), ANY, {}, 100);
  assert.deepEqual(ids(unsettled.records), ['before', 'r1', 'after']);
  const settled = H.mergeAfterLoad(result, true, live([after], [before, after]), ANY, {}, 100);
  assert.deepEqual(ids(settled.records), ['r1', 'after']);
});

test('mergeAfterLoad: a carried row that does not match the filter is dropped', () => {
  const f = { ...ANY, status: 'Forwarded', hideMalformed: true, fromMs: BASE + 50_000 };
  const good = rec('good', 100);
  const blocked = rec('blocked', 101, { status: 'Blocked' });
  const malformed = rec('malformed', 102, { malformed: 'crc', dev_addr: null });
  const tooOld = rec('tooOld', 10);
  const m = H.mergeAfterLoad([], true, live([good, blocked, malformed, tooOld]), f, {}, 100);
  assert.deepEqual(ids(m.records), ['good']);
});

test('mergeAfterLoad: a carried row is matched with the labels as they are now', () => {
  const f = { ...ANY, device: 'cold' };
  const row = rec('labelled', 100, { dev_addr: '11112222' });
  assert.deepEqual(ids(H.mergeAfterLoad([], true, live([row]), f, {}, 10).records), []);
  assert.deepEqual(
    ids(H.mergeAfterLoad([], true, live([row]), f, { '11112222': 'Cold room' }, 10).records),
    ['labelled'],
  );
});

test('mergeAfterLoad: membership decides, not time', () => {
  // A live row stamped before the newest result row and one stamped after it:
  // both were received after the send and are missing, so both are carried.
  const result = [rec('r1', 100), rec('r2', 200)];
  const m = H.mergeAfterLoad(result, true, live([rec('early', 50), rec('late', 300)]), ANY, {}, 100);
  assert.deepEqual(ids(m.records), ['early', 'r1', 'r2', 'late']);
});

test('mergeAfterLoad: max cuts the old end and counts it', () => {
  const result = [rec('r1', 100), rec('r2', 110)];
  const m = H.mergeAfterLoad(result, true, live([rec('x', 120), rec('y', 130)]), ANY, {}, 3);
  assert.deepEqual(ids(m.records), ['r2', 'x', 'y']);
  assert.equal(m.dropped, 1);
});

// ===========================================================================
// Count helpers
// ===========================================================================

test('parseCountHeader and parseWriteFailures', () => {
  assert.equal(H.parseCountHeader('12345'), 12345);
  assert.equal(H.parseCountHeader('0'), 0);
  assert.equal(H.parseCountHeader(' 7 '), 7);
  assert.equal(H.parseCountHeader(null), null);
  assert.equal(H.parseCountHeader(''), null);
  assert.equal(H.parseCountHeader('-3'), null);
  assert.equal(H.parseCountHeader('1.5'), null);
  assert.equal(H.parseCountHeader('abc'), null);
  assert.equal(H.parseCountHeader('99999999999999999999'), null);
  assert.equal(H.parseWriteFailures('4'), 4);
  assert.equal(H.parseWriteFailures(null), 0);
  assert.equal(H.parseWriteFailures('x'), 0);
});

test('olderAfterLoad: the total less the rows returned, plus the rows the cut dropped', () => {
  assert.equal(H.olderAfterLoad(12345, 5000, 0), 7345);
  assert.equal(H.olderAfterLoad(12345, 5000, 40), 7385);
  assert.equal(H.olderAfterLoad(10, 10, 0), 0);
  assert.equal(H.olderAfterLoad(3, 5, 0), 0, 'never negative');
  assert.equal(H.olderAfterLoad(null, 5, 2), 2, 'no total: nothing is known to lie beyond the rows');
});

test('matchingCount: older records count only while the oldest held row is in range', () => {
  const oldest = rec('o', 0);
  const inRange = BASE - 1000;
  const aged = BASE + 1000;
  assert.equal(H.matchingCount(10, 90, oldest, null), 100);
  assert.equal(H.matchingCount(10, 90, oldest, inRange), 100);
  assert.equal(H.matchingCount(10, 90, oldest, aged), 10);
  assert.equal(H.matchingCount(10, 90, oldest, BASE), 100, 'exactly at the bound is still in');
  assert.equal(H.matchingCount(0, 90, undefined, null), 0);
});

test('malformedAfterLoad: the header plus the live records since the send, null without a header', () => {
  assert.equal(H.malformedAfterLoad(678, 2), 680);
  assert.equal(H.malformedAfterLoad(0, 0), 0);
  assert.equal(H.malformedAfterLoad(null, 2), null);
});

// ===========================================================================
// The reference wiring
//
// A model of what the history hook does with the state machine's actions: the
// rows, the live records, the pause buffer, the counts. It runs the real pure
// functions, and checks after every step the rules the hook relies on.
// ===========================================================================

const CAP = 5000;

class Hook {
  constructor({ filter = fstate(), labels = {}, nowMs = NOW, max = H.MAX_MESSAGES } = {}) {
    this.nowMs = nowMs;
    this.labels = labels;
    this.max = max;
    this.state = H.initialRequestState(filter);

    this.rows = [];
    this.liveIds = new Set(); // rows that arrived live and were never replaced by a fetched copy
    this.older = 0;
    this.lastLoad = null; // { filter, sentSeq, header } of the load last applied
    this.writeFailures = 0;
    this.buffer = []; // the pause buffer
    this.overflowed = false;
    this.log = []; // every raw live record, with its arrival number
    this.seq = 0;
    this.sentSeq = new Map(); // generation -> arrival number when the load was sent
    this.held = null; // { load, settled, payload }
    this.payload = null;
    this.timers = new Set();
    this.sends = []; // every send action
    this.steps = []; // { event, actions }
  }

  // --- driving the machine ------------------------------------------------

  dispatch(event) {
    const hadInFlight = this.state.inFlight !== null;
    const { state, actions } = H.nextRequests(this.state, event, this.nowMs);
    this.state = state;
    this.steps.push({ event, actions });
    this.lastActions = actions;
    let aborted = false;
    for (const a of actions) {
      if (a.kind === 'abort') aborted = true;
      if (a.kind === 'send' && !a.resend && hadInFlight) {
        assert.ok(aborted, 'a new load while one is in flight must abort it first');
      }
      this.run(a);
    }
    this.check();
    return actions;
  }

  run(a) {
    switch (a.kind) {
      case 'send':
        for (const part of ['newest=true', 'count=true', 'settle=true', 'limit=5000']) {
          assert.ok(a.load.query.split('&').includes(part), `every load carries ${part}`);
        }
        if (!a.resend) this.sentSeq.set(a.load.generation, this.seq);
        this.sends.push(a);
        break;
      case 'startTimer':
        this.timers.add(a.timer);
        break;
      case 'cancelTimer':
        assert.ok(this.timers.delete(a.timer), `cancelling ${a.timer}, which is not pending`);
        break;
      case 'apply':
        this.applyLoad(a.load, a.settled, this.payload);
        break;
      case 'hold':
        this.held = { load: a.load, settled: a.settled, payload: this.payload };
        break;
      case 'applyHeld':
        assert.ok(this.held && this.held.load.generation === a.load.generation, 'applyHeld names the held load');
        this.applyLoad(a.load, a.settled, this.held.payload);
        this.held = null;
        this.buffer = [];
        this.overflowed = false;
        break;
      case 'mergeBuffer':
        this.mergeBuffer();
        break;
      case 'abort':
      case 'ignore':
        break;
      default:
        assert.fail(`unknown action ${a.kind}`);
    }
    if (a.kind === 'send' && !a.resend) {
      // A load discards a held answer, and an overflowed buffer is replaced by it.
      this.held = null;
      if (a.load.cause === 'user' && this.overflowed && !this.state.paused) {
        this.buffer = [];
        this.overflowed = false;
      }
    }
  }

  check() {
    const s = this.state;
    assert.equal(this.timers.has('typing'), s.typingTimer, 'typing timer flag matches the actions');
    assert.equal(this.timers.has('retry'), s.retryTimer, 'retry timer flag matches the actions');
    assert.equal(this.timers.has('mount'), s.mountTimer, 'mount timer flag matches the actions');
    assert.ok(!(s.retryTimer && s.inFlight !== null), 'no retry timer is pending while a load is in flight');
    assert.equal((s.held !== null), (this.held !== null), 'a held answer exists on both sides');
    if (s.held !== null) assert.ok(s.paused || false, 'an answer is held only while paused');
    if (s.retryStep === null) assert.ok(!s.retryTimer, 'no retry timer without a series');
  }

  fire(timer) {
    assert.ok(this.timers.delete(timer), `${timer} timer fired while not pending`);
    const kind = { typing: 'timer', retry: 'retryTimer', mount: 'mountTimer' }[timer];
    return this.dispatch({ kind });
  }

  // --- answers --------------------------------------------------------------

  respond(generation, { records = [], total = records.length, malformed = null, settled = true, failures = null } = {}) {
    this.writeFailures = H.parseWriteFailures(failures); // every response, current or not
    this.payload = { records, total, malformed };
    return this.dispatch({ kind: 'response', generation, settled });
  }

  fail(generation, transient, message = 'failed') {
    return this.dispatch({ kind: 'failed', generation, transient, message });
  }

  noWal(generation) {
    return this.dispatch({ kind: 'noWal', generation });
  }

  unauthorized(generation) {
    return this.dispatch({ kind: 'unauthorized', generation });
  }

  // --- the table --------------------------------------------------------------

  currentFilter() {
    return H.toHistoryFilter(this.state.filter, this.nowMs);
  }

  onRecord(r) {
    const seq = this.seq++;
    this.log.push({ seq, record: r });
    if (this.state.paused) {
      this.buffer.push(r);
      if (this.buffer.length > this.max) this.overflowed = true;
      return;
    }
    if (!H.matchesFilter(r, this.currentFilter(), this.labels)) return;
    const had = this.rows.some(x => x.id === r.id);
    const m = H.mergeRecords(this.rows, [r], this.max);
    this.rows = m.records;
    this.older += m.dropped;
    if (!had) this.liveIds.add(r.id);
  }

  applyLoad(load, settled, payload) {
    const sentSeq = this.sentSeq.get(load.generation);
    const sinceSent = this.log.filter(e => e.seq >= sentSeq).map(e => e.record);
    const heldLive = [...this.rows.filter(r => this.liveIds.has(r.id)), ...this.buffer];
    const m = H.mergeAfterLoad(
      payload.records, settled, { sinceSent, held: heldLive }, load.filter, this.labels, this.max,
    );
    this.rows = m.records;
    this.older = H.olderAfterLoad(payload.total, payload.records.length, m.dropped);
    const fetched = new Set(ids(payload.records));
    this.liveIds = new Set(ids(m.records).filter(id => !fetched.has(id)));
    this.lastLoad = { filter: load.filter, sentSeq, header: payload.malformed };
  }

  mergeBuffer() {
    const f = this.currentFilter();
    const keep = this.buffer.filter(r => H.matchesFilter(r, f, this.labels));
    const before = new Set(ids(this.rows));
    const m = H.mergeRecords(this.rows, keep, this.max);
    this.rows = m.records;
    this.older += m.dropped;
    for (const r of keep) if (!before.has(r.id)) this.liveIds.add(r.id);
    this.buffer = [];
    this.overflowed = false;
  }

  /** Rows that pass the render-time filter. */
  shown(opts) {
    const f = this.currentFilter();
    return this.rows.filter(r => H.matchesFilter(r, f, this.labels, opts));
  }

  matching() {
    const f = this.currentFilter();
    return H.matchingCount(this.shown().length, this.older, this.rows[0], f.fromMs);
  }

  malformedFigure() {
    if (this.lastLoad === null) return null;
    const { filter, sentSeq, header } = this.lastLoad;
    const live = this.log.filter(e => e.seq >= sentSeq && H.countsAsMalformed(e.record, filter, this.labels)).length;
    return H.malformedAfterLoad(header, live);
  }

  lastSend() {
    return this.sends[this.sends.length - 1];
  }
}

const kinds = actions =>
  actions.map(a => {
    if (a.kind === 'startTimer' || a.kind === 'cancelTimer') return `${a.kind}:${a.timer}`;
    if (a.kind === 'send') return a.resend ? 'resend' : 'send';
    return a.kind;
  });

const open = { kind: 'socketOpen' };

test('presets: one nowMs gives both the request and the carry-over check', () => {
  const h = new Hook({ filter: fstate({ timePreset: '5m' }), nowMs: NOW });
  h.dispatch({ kind: 'socketOpen' });
  const load = h.lastSend().load;
  const from = NOW - 5 * 60e3;
  assert.equal(load.filter.fromMs, from);
  assert.equal(new URLSearchParams(load.query).get('from'), new Date(from).toISOString());
  // A live record just inside the bound is carried; one just outside is not.
  const inside = { ...rec('in', 0), timestamp: new Date(from).toISOString() };
  const outside = { ...rec('out', 0), timestamp: new Date(from - 1).toISOString() };
  const merged = H.mergeAfterLoad([], true, { sinceSent: [inside, outside], held: [] }, load.filter, {}, 100);
  assert.deepEqual(ids(merged.records), ['in']);
});

// The rows of a day, with live records. In the hook, a live record is checked
// against `toHistoryFilter(filter, now)` before it is added, and a load carries
// over only the live records that pass the same check.

const MADRID_NOW = Date.parse('2026-10-30T14:00:00.000Z'); // a Friday afternoon in Madrid, after the change of the 25th

/** A record stamped `timestamp`. */
const recAt = (id, timestamp, o = {}) => ({ ...rec(id, 0, o), timestamp });

test('days: a past day takes no live record', () => {
  withTimeZone('Europe/Madrid', () => {
    const h = new Hook({ filter: fstate({ timePreset: 'day', day: '2026-10-25' }), nowMs: MADRID_NOW });
    h.dispatch(open);
    const query = h.lastSend().load.query;
    assert.ok(
      query.startsWith('from=2026-10-24T22%3A00%3A00.000Z&before=2026-10-25T23%3A00%3A00.000Z&'),
      query,
    );
    h.respond(1, { records: [recAt('stored', '2026-10-25T10:00:00.000Z')], total: 1 });
    assert.deepEqual(ids(h.rows), ['stored']);

    // Live traffic now, and a stamp just after the day's end.
    h.onRecord(recAt('now', iso(MADRID_NOW)));
    h.onRecord(recAt('next-day', '2026-10-25T23:00:00.001Z'));
    assert.deepEqual(ids(h.rows), ['stored'], 'nothing was added');
    assert.deepEqual(ids(h.shown()), ['stored']);
    assert.equal(h.matching(), 1);
  });
});

test('days: today takes live records up to its end, not past it', () => {
  withTimeZone('Europe/Madrid', () => {
    // 2026-10-30 is a 24-hour day in CET: it ends at 23:00 UTC.
    const h = new Hook({ filter: fstate({ timePreset: 'day', day: '2026-10-30' }), nowMs: MADRID_NOW });
    h.dispatch(open);
    h.respond(1, { records: [recAt('stored', '2026-10-30T08:00:00.000Z')], total: 1 });

    h.onRecord(recAt('now', iso(MADRID_NOW)));
    h.onRecord(recAt('last', '2026-10-30T22:59:59.999Z'));
    h.onRecord(recAt('midnight', '2026-10-30T23:00:00.000Z'));
    h.onRecord(recAt('yesterday', '2026-10-29T22:59:59.999Z'));
    assert.deepEqual(ids(h.rows), ['stored', 'now', 'last']);
    assert.deepEqual(ids(h.shown()), ['stored', 'now', 'last']);
    assert.equal(h.matching(), 3, 'one stored row and two live ones; nothing older');
  });
});

test('days: a load carries over only the live records that are inside the day', () => {
  withTimeZone('Europe/Madrid', () => {
    const h = new Hook({ filter: fstate({ timePreset: 'day', day: '2026-10-30' }), nowMs: MADRID_NOW });
    h.dispatch(open);
    // Received after the load was sent, before it answers.
    h.onRecord(recAt('in', iso(MADRID_NOW)));
    h.onRecord(recAt('past', '2026-10-29T12:00:00.000Z'));
    h.onRecord(recAt('end', '2026-10-30T23:00:00.000Z'));
    h.respond(1, { records: [], total: 0 });
    assert.deepEqual(ids(h.rows), ['in']);

    // Going to another day replaces the rows and drops the live record held for today.
    h.dispatch({ kind: 'filter', filter: fstate({ timePreset: 'day', day: '2026-10-29' }) });
    h.respond(2, { records: [recAt('old', '2026-10-29T09:00:00.000Z')], total: 1 });
    assert.deepEqual(ids(h.rows), ['old']);
    assert.equal(h.lastSend().load.filter.untilMs, Date.parse('2026-10-29T23:00:00.000Z'));
  });
});

test('days: the clock moving on does not move a day, so a record on its last millisecond stays', () => {
  withTimeZone('Europe/Madrid', () => {
    const filter = fstate({ timePreset: 'day', day: '2026-10-30' });
    const early = new Hook({ filter, nowMs: Date.parse('2026-10-30T09:00:00.000Z') });
    const late = new Hook({ filter, nowMs: Date.parse('2026-10-30T22:59:59.000Z') });
    const record = recAt('r', '2026-10-30T22:59:59.999Z');
    assert.equal(H.matchesFilter(record, early.currentFilter(), {}), true);
    assert.equal(H.matchesFilter(record, late.currentFilter(), {}), true);
    assert.deepEqual(early.currentFilter(), late.currentFilter());
  });
});

// ===========================================================================
// Counts against an oracle
// ===========================================================================

/**
 * One load, with records on every side of it. Each record has a timeline:
 *   live  - when this browser received it: never, before the load was sent,
 *           while it was in flight, or after it answered;
 *   saved - when the database got it: before the snapshot the load read,
 *           after it, or never (the insert failed).
 * A record received live before the load was sent was queued before the
 * load's marker, so it is in the snapshot unless its insert failed.
 */
const VARIANTS = [
  { live: 'none',   saved: 'pre'    },
  { live: 'before', saved: 'pre'    },
  { live: 'before', saved: 'failed' },
  { live: 'during', saved: 'pre'    },
  { live: 'during', saved: 'post'   },
  { live: 'during', saved: 'failed' },
  { live: 'after',  saved: 'pre'    },
  { live: 'after',  saved: 'post'   },
  { live: 'after',  saved: 'failed' },
];

function serverAnswer(snapshot, f, labels, cap) {
  const matching = snapshot.filter(r => H.matchesFilter(r, f, labels));
  const oldestFirst = [...matching].sort((a, b) => H.compareTimestamps(a.timestamp, b.timestamp));
  return {
    records: cap === Infinity ? oldestFirst : oldestFirst.slice(-cap),
    total: matching.length,
    malformed: snapshot.filter(r => H.countsAsMalformed(r, f, labels)).length,
  };
}

function runCountScenario({ recs, filter, cap, settled }) {
  // recs: [{ record, live, saved }]
  const h = new Hook({ filter });
  const f = h.currentFilter();
  const arrive = when => recs.filter(x => x.live === when).forEach(x => h.onRecord(x.record));

  arrive('before');
  h.dispatch(open);
  arrive('during');
  const snapshot = recs.filter(x => x.saved === 'pre').map(x => x.record);
  h.respond(1, { ...serverAnswer(snapshot, f, h.labels, cap), settled });
  arrive('after');

  const result = serverAnswer(snapshot, f, h.labels, cap);
  const inResult = new Set(ids(result.records));
  const dbFinal = recs.filter(x => x.saved !== 'failed').map(x => x.record);
  const oracle = dbFinal.filter(r => H.matchesFilter(r, f, h.labels));
  const oracleMalformed = dbFinal.filter(r => H.countsAsMalformed(r, f, h.labels)).length;

  // What the table must hold: the result, and the live records it carries.
  const carriedWhen = settled ? ['during', 'after'] : ['before', 'during', 'after'];
  const carried = recs.filter(
    x => carriedWhen.includes(x.live) && !inResult.has(x.record.id) && H.matchesFilter(x.record, f, h.labels),
  );
  const tag = JSON.stringify(recs.map(x => `${x.record.id}:${x.live}/${x.saved}${x.record.malformed ? '/m' : ''}`)) +
    ` cap=${cap} settled=${settled} hide=${f.hideMalformed}`;

  // The rows are the result plus the carried records, oldest first.
  const expectedRows = [...result.records, ...carried.map(x => x.record)]
    .sort((a, b) => H.compareTimestamps(a.timestamp, b.timestamp));
  assert.deepEqual(ids(h.shown()), ids(expectedRows), `rows: ${tag}`);

  // Matching never undercounts. It is exact unless a carried record is also
  // counted beyond the cap, or a carried record was never saved.
  const failedShown = carried.filter(x => x.saved === 'failed').length;
  const doubled = carried.filter(x => x.saved === 'pre').length; // saved, cut by the cap, and carried
  const matching = h.matching();
  assert.ok(matching >= oracle.length, `Matching never low: ${matching} < ${oracle.length}: ${tag}`);
  assert.equal(matching, oracle.length + failedShown + doubled, `Matching: ${tag}`);
  if (cap === Infinity && failedShown === 0) {
    assert.equal(matching, oracle.length, `Matching exact when uncapped: ${tag}`);
  }
  if (cap === Infinity) {
    for (const r of oracle) assert.ok(ids(h.shown()).includes(r.id), `every record of an uncapped range is shown: ${tag}`);
  }

  // The malformed figure: the header plus the matching malformed live records
  // since the send. Never low; high by one per record counted twice (in the
  // header and live) or never saved.
  const figure = h.malformedFigure();
  const liveSince = recs.filter(
    x => (x.live === 'during' || x.live === 'after') && H.countsAsMalformed(x.record, f, h.labels),
  );
  const malformedDoubled = liveSince.filter(x => x.saved === 'pre').length;
  const malformedFailed = liveSince.filter(x => x.saved === 'failed').length;
  assert.ok(figure >= oracleMalformed, `malformed never low: ${figure} < ${oracleMalformed}: ${tag}`);
  assert.equal(figure, oracleMalformed + malformedDoubled + malformedFailed, `malformed: ${tag}`);
}

test('counts: every timeline of 3 records around a load, capped and uncapped, checkbox off and on, settled or not', () => {
  const perms = [[0, 1, 2], [2, 1, 0], [1, 2, 0]];
  const masks = [[], [0], [1, 2], [0, 1, 2]]; // which records are malformed
  let scenarios = 0;
  for (let a = 0; a < VARIANTS.length; a++) {
    for (let b = 0; b < VARIANTS.length; b++) {
      for (let c = 0; c < VARIANTS.length; c++) {
        for (const perm of perms) {
          for (const mask of masks) {
            for (const cap of [Infinity, 2]) {
              for (const showMalformed of [false, true]) {
                for (const settled of [true, false]) {
                  const recs = [a, b, c].map((v, i) => ({
                    record: rec('n' + i, 100 + perm[i] * 10, { malformed: mask.includes(i) ? 'crc' : null }),
                    ...VARIANTS[v],
                  }));
                  // A record the filter does not match, arriving during the load.
                  recs.push({ record: rec('other', 135, { status: 'Blocked' }), live: 'during', saved: 'post' });
                  runCountScenario({ recs, filter: fstate({ status: 'Forwarded', showMalformed }), cap, settled });
                  scenarios++;
                }
              }
            }
          }
        }
      }
    }
  }
  assert.equal(scenarios, 729 * 3 * 4 * 2 * 2 * 2);
});

test('counts: live records on both sides of the snapshot, uncapped: Matching is exact', () => {
  const h = new Hook();
  const a = rec('a', 10), b = rec('b', 20), c = rec('c', 30), d = rec('d', 40);
  h.onRecord(a); // received before the load, saved before the snapshot
  h.dispatch(open);
  h.onRecord(b); // received during the load, saved after the snapshot
  h.respond(1, { records: [a, c], total: 2, malformed: 0 }); // c: in the snapshot, never received live
  assert.deepEqual(ids(h.rows), ['a', 'b', 'c']);
  h.onRecord(d); // after the load
  assert.deepEqual(ids(h.rows), ['a', 'b', 'c', 'd']);
  assert.equal(h.matching(), 4); // the database now holds a, b, c, d
});

test('counts: a record in the snapshot but cut by the cap, and received during the load, is counted twice (high, not low)', () => {
  const h = new Hook();
  const cut = rec('cut', 5), n1 = rec('n1', 10), n2 = rec('n2', 20);
  h.dispatch(open);
  h.onRecord(cut); // during the load; stamped older than the cap's edge
  h.respond(1, { records: [n1, n2], total: 3, malformed: 0 }); // cap 2 of 3
  assert.deepEqual(ids(h.rows), ['cut', 'n1', 'n2'], 'carried by id, not dropped on a timestamp guess');
  assert.equal(h.older, 1);
  assert.equal(h.matching(), 4, 'the database holds 3; the figure is one high');
});

test('counts: rows cut at MAX_MESSAGES are added to older', () => {
  const h = new Hook({ max: 3 });
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 1), rec('b', 2), rec('c', 3)], total: 3 });
  h.onRecord(rec('d', 4));
  assert.deepEqual(ids(h.rows), ['b', 'c', 'd']);
  assert.equal(h.older, 1);
  assert.equal(h.matching(), 4);
});

test('counts: the malformed figure with a malformed live record on each side of the snapshot, box off and on', () => {
  for (const showMalformed of [false, true]) {
    const h = new Hook({ filter: fstate({ showMalformed }) });
    const inSnapshot = rec('m1', 10, { malformed: 'crc', dev_addr: null });
    const duringSaved = rec('m2', 20, { malformed: 'mhdr', dev_addr: null }); // in the snapshot and live
    const duringLater = rec('m3', 30, { malformed: 'length', dev_addr: null }); // saved after the snapshot
    h.dispatch(open);
    h.onRecord(duringSaved);
    h.onRecord(duringLater);
    // Database at the end: m1, m2, m3 = 3 malformed. The header saw m1 and m2.
    h.respond(1, { records: showMalformed ? [inSnapshot, duringSaved] : [], total: showMalformed ? 2 : 0, malformed: 2 });
    assert.equal(h.malformedFigure(), 4, 'one record counted twice, none missed');
    assert.ok(h.malformedFigure() >= 3);
    const after = rec('m4', 40, { malformed: 'crc', dev_addr: null });
    h.onRecord(after);
    assert.equal(h.malformedFigure(), 5);
    assert.deepEqual(ids(h.shown()), showMalformed ? ['m1', 'm2', 'm3', 'm4'] : []);
  }
});

test('counts: the malformed figure is null without the header, and counts only records that pass the other filters', () => {
  const h = new Hook({ filter: fstate({ gateway: 'G1' }) });
  h.dispatch(open);
  h.onRecord(rec('here', 10, { malformed: 'crc', dev_addr: null, gateway_eui: 'G1' }));
  h.onRecord(rec('elsewhere', 11, { malformed: 'crc', dev_addr: null, gateway_eui: 'G2' }));
  h.respond(1, { records: [], total: 0, malformed: 7 });
  assert.equal(h.malformedFigure(), 8);
  const old = new Hook();
  old.dispatch(open);
  old.respond(1, { records: [], total: 0, malformed: null });
  assert.equal(old.malformedFigure(), null);
});

test('counts: a live record whose insert failed is carried and stays until the next replacing load', () => {
  const h = new Hook();
  const stored = rec('stored', 10), failed = rec('failed', 20);
  h.dispatch(open);
  h.onRecord(failed); // received after the load was sent, never saved
  h.respond(1, { records: [stored], total: 1, failures: '1' });
  assert.deepEqual(ids(h.rows), ['stored', 'failed'], 'carried');
  assert.equal(h.matching(), 2, 'high by the unsaved row, never low');
  // Later answers that do not carry it again drop it. A load sent after the
  // row was received finds it in neither the result nor its own live records.
  h.dispatch({ kind: 'filter', filter: fstate() });
  h.respond(2, { records: [stored], total: 1, failures: '1' });
  assert.deepEqual(ids(h.rows), ['stored']);
  assert.equal(h.matching(), 1);
});

test('counts: writeFailures follows the latest response, settled or not, stale or not', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [], failures: '0' });
  assert.equal(h.writeFailures, 0);
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) }); // generation 2
  h.respond(2, { records: [], settled: false, failures: '3' });
  assert.equal(h.writeFailures, 3, 'an unsettled answer reports it too');
  h.respond(1, { records: [], failures: '5' }); // a stale answer
  assert.equal(h.writeFailures, 5, 'a stale answer still carries the server-wide counter');
  h.respond(2, { records: [], failures: null }); // an older server: no header
  assert.equal(h.writeFailures, 0);
});

test('counts: no WAL, malformed-heavy and capped: 1,000 valid rows shown, Matching an upper bound of 6,000', () => {
  // The newest 5,000 records of a 10,000-record range: 4,000 malformed and
  // 1,000 valid. The older 5,000 hold 1,000 malformed and 4,000 valid, so the
  // database has 5,000 valid records in all.
  const records = [];
  for (let i = 0; i < 5000; i++) {
    const malformed = i >= 1000; // 4,000 of the newest 5,000 are malformed
    records.push(rec('n' + i, 5000 + i, malformed ? { malformed: 'crc', dev_addr: null } : {}));
  }
  const h = new Hook(); // the default view hides malformed frames
  h.dispatch(open);
  assert.ok(h.lastSend().load.query.includes('malformed=exclude'));
  h.noWal(1);
  assert.ok(!h.lastSend().load.query.includes('malformed='), 'the range is loaded with malformed frames');
  h.respond(1, { records, total: 10000, malformed: null });
  assert.equal(h.rows.length, 5000);
  const shown = h.shown();
  assert.equal(shown.length, 1000);
  assert.ok(shown.every(r => r.malformed === null), 'no malformed row is shown');
  assert.equal(h.older, 5000);
  assert.equal(h.matching(), 6000);
  assert.ok(h.matching() >= 5000, 'not below the 5,000 valid records the database holds');
  assert.equal(h.malformedFigure(), null, 'the malformed count is unknown');
});

// ===========================================================================
// nextRequests: typing, filters, sockets
// ===========================================================================

test('nextRequests: a type change inside the typing pause cancels the timer and sends once, with the typed text', () => {
  const h = new Hook({ filter: fstate() });
  h.dispatch(open);
  h.respond(1);
  const typed = fstate({ device: 'abc' });
  assert.deepEqual(kinds(h.dispatch(H.filterChangeEvent(fstate(), typed))), ['startTimer:typing']);
  assert.equal(h.lastActions[0].delayMs, 400);
  const withType = fstate({ device: 'abc', mtype: ['JoinRequest'] });
  const actions = h.dispatch(H.filterChangeEvent(typed, withType));
  assert.deepEqual(kinds(actions), ['cancelTimer:typing', 'send']);
  const load = h.lastSend().load;
  assert.equal(load.cause, 'user');
  assert.equal(load.generation, 2);
  assert.ok(load.query.includes('device=abc') && load.query.includes('mtype=JoinRequest'));
  // The timer is gone: nothing else is sent.
  assert.deepEqual(h.dispatch({ kind: 'timer' }), []);
  assert.equal(h.sends.length, 2);
});

test('nextRequests: typing restarts the timer, and only its expiry sends', () => {
  const h = new Hook({ filter: fstate() });
  h.dispatch(open);
  h.respond(1);
  let prev = fstate();
  for (const text of ['0', '0a', '0a1', '0a1b']) {
    const next = fstate({ device: text });
    assert.deepEqual(kinds(h.dispatch(H.filterChangeEvent(prev, next))), ['startTimer:typing']);
    prev = next;
  }
  assert.equal(h.sends.length, 1, 'no load while typing');
  assert.ok(H.isBusy(h.state), 'a load is scheduled');
  assert.deepEqual(kinds(h.fire('typing')), ['send']);
  assert.ok(h.lastSend().load.query.includes('device=0a1b'));
  assert.equal(h.sends.length, 2);
});

test('nextRequests: a filter change during a catch-up load aborts it; its late answer is ignored, rows and counts', () => {
  const h = new Hook();
  const stale = rec('stale', 10);
  h.dispatch(open); // generation 1, catch-up
  const actions = h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) });
  assert.deepEqual(kinds(actions), ['abort', 'send']);
  assert.equal(h.lastSend().load.generation, 2);
  const failuresBefore = h.writeFailures;
  assert.deepEqual(kinds(h.respond(1, { records: [stale], total: 99, malformed: 9, failures: '4' })), ['ignore']);
  assert.deepEqual(h.rows, []);
  assert.equal(h.older, 0);
  assert.equal(h.lastLoad, null);
  assert.equal(h.state.inFlight.generation, 2, 'the newer load is still awaited');
  assert.equal(failuresBefore, 0);
  assert.deepEqual(kinds(h.fail(1, false, 'late')), ['ignore']);
  assert.equal(h.state.error, null, 'an old generation shows no error');
  h.respond(2, { records: [rec('fresh', 20)], total: 1, malformed: 0 });
  assert.deepEqual(ids(h.rows), ['fresh']);
});

test('nextRequests: a socket open during a user load aborts it and sends one catch-up load with the typed text', () => {
  const h = new Hook({ filter: fstate() });
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) }); // user load, generation 1
  assert.equal(h.lastSend().load.cause, 'user');
  h.dispatch(H.filterChangeEvent(fstate({ gateway: 'G1' }), fstate({ gateway: 'G1', device: 'typed' })));
  assert.ok(h.state.typingTimer);
  const actions = h.dispatch(open);
  assert.deepEqual(kinds(actions), ['abort', 'cancelTimer:typing', 'send']);
  const load = h.lastSend().load;
  assert.equal(load.cause, 'catchUp');
  assert.equal(load.generation, 2);
  assert.ok(load.query.includes('device=typed') && load.query.includes('gateway=G1'));
  assert.equal(h.sends.length, 2);
  assert.deepEqual(kinds(h.respond(1, { records: [rec('x', 1)] })), ['ignore'], 'the aborted load answers late: ignored');
  assert.deepEqual(h.rows, []);
});

test('nextRequests: stale answers are ignored whatever the cause', () => {
  const h = new Hook();
  h.dispatch(open); // 1, catch-up
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'A' }) }); // 2, user
  h.dispatch(open); // 3, catch-up
  for (const g of [1, 2]) {
    assert.deepEqual(kinds(h.respond(g, { records: [rec('s' + g, g)] })), ['ignore']);
  }
  assert.deepEqual(h.rows, []);
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'B' }) }); // 4, user
  assert.deepEqual(kinds(h.respond(3, { records: [rec('s3', 3)] })), ['ignore']);
  assert.deepEqual(kinds(h.respond(4, { records: [rec('s4', 4)] })), ['apply']);
  assert.deepEqual(ids(h.rows), ['s4']);
});

test('nextRequests: the first label snapshot schedules one load when the device box has text, none when empty', () => {
  const empty = new Hook({ filter: fstate() });
  empty.dispatch(open);
  empty.respond(1);
  assert.deepEqual(empty.dispatch({ kind: 'labels', snapshot: true }), []);
  assert.equal(empty.sends.length, 1);

  const typed = new Hook({ filter: fstate({ device: 'havana' }) });
  typed.dispatch(open);
  typed.respond(1);
  assert.deepEqual(kinds(typed.dispatch({ kind: 'labels', snapshot: true })), ['startTimer:typing']);
  assert.equal(typed.lastActions[0].delayMs, 400);
  assert.deepEqual(kinds(typed.fire('typing')), ['send']);
  assert.equal(typed.sends.length, 2, 'one load follows the snapshot');
  assert.equal(typed.lastSend().load.cause, 'user');
  // A later label change does the same, once per quiet 400 ms.
  typed.respond(2);
  typed.dispatch({ kind: 'labels', snapshot: false });
  typed.dispatch({ kind: 'labels', snapshot: false });
  typed.fire('typing');
  assert.equal(typed.sends.length, 3);
});

test('nextRequests: a failure of an old generation shows no error; a current one does, and the next answer clears it', () => {
  const h = new Hook();
  h.dispatch(open); // 1
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) }); // 2
  h.fail(1, false, 'old');
  assert.equal(h.state.error, null);
  h.fail(2, false, 'HTTP 500');
  assert.equal(h.state.error, 'HTTP 500');
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G2' }) }); // 3, the error stays while it loads
  assert.equal(h.state.error, 'HTTP 500');
  h.respond(3);
  assert.equal(h.state.error, null);
});

test('nextRequests: a failed load keeps the rows', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 1)], total: 1 });
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) });
  h.fail(2, false, 'boom');
  assert.deepEqual(ids(h.rows), ['a']);
});

test('nextRequests: isBusy covers a scheduled and an in-flight load', () => {
  const h = new Hook({ filter: fstate() });
  assert.equal(H.isBusy(h.state), false);
  h.dispatch(open);
  assert.equal(H.isBusy(h.state), true);
  h.respond(1);
  assert.equal(H.isBusy(h.state), false);
  h.dispatch(H.filterChangeEvent(fstate(), fstate({ device: 'a' })));
  assert.equal(H.isBusy(h.state), true);
});

test('filterChangeEvent: only the device text is debounced', () => {
  const base = fstate();
  assert.equal(H.filterChangeEvent(base, { ...base }), null);
  assert.equal(H.filterChangeEvent(base, { ...base, device: 'a' }).kind, 'typed');
  for (const change of [
    { mtype: ['JoinRequest'] }, { status: 'Blocked' }, { gateway: 'G' }, { timePreset: '5m' }, { showMalformed: true },
  ]) {
    assert.equal(H.filterChangeEvent(base, { ...base, ...change }).kind, 'filter', JSON.stringify(change));
  }
  // The device text changing together with another filter loads at once.
  assert.equal(H.filterChangeEvent(base, { ...base, device: 'a', status: 'Blocked' }).kind, 'filter');
});

test('filterChangeEvent: a change of day loads at once', () => {
  const a = fstate({ timePreset: 'day', day: '2026-10-24' });
  const b = fstate({ timePreset: 'day', day: '2026-10-25' });
  assert.equal(H.filterChangeEvent(a, { ...a }), null, 'the same day is no change');
  assert.deepEqual(H.filterChangeEvent(a, b), { kind: 'filter', filter: b });
  // From a span to a day and back: the preset changes too.
  assert.equal(H.filterChangeEvent(fstate(), a).kind, 'filter');
  assert.equal(H.filterChangeEvent(a, fstate()).kind, 'filter');
  // The day alone is enough, whatever the preset says.
  assert.equal(H.filterChangeEvent(fstate({ day: null }), fstate({ day: '2026-10-24' })).kind, 'filter');
  // A day change together with typing is not debounced.
  assert.equal(H.filterChangeEvent(a, { ...b, device: 'x' }).kind, 'filter');

  const h = new Hook({ filter: a });
  h.dispatch(open);
  h.respond(1);
  const actions = h.dispatch(H.filterChangeEvent(a, b));
  assert.ok(kinds(actions).includes('send'), kinds(actions).join());
  assert.ok(!kinds(actions).includes('startTimer:typing'));
  assert.ok(h.lastSend().load.query.startsWith('from='), h.lastSend().load.query);
  assert.ok(h.lastSend().load.query.includes('&before='), h.lastSend().load.query);
});

// ===========================================================================
// nextRequests: reload on reconnect
// ===========================================================================

test('reload on reconnect: every socket open sends one catch-up load with the current filters, the first included', () => {
  const h = new Hook({ filter: fstate({ gateway: 'G1', status: 'Forwarded' }) });
  for (let n = 1; n <= 3; n++) {
    const actions = h.dispatch(open);
    assert.deepEqual(kinds(actions).filter(k => k === 'send'), ['send']);
    const load = h.lastSend().load;
    assert.equal(load.cause, 'catchUp');
    assert.equal(load.generation, n);
    assert.ok(load.query.includes('settle=true') && load.query.includes('gateway=G1') && load.query.includes('status=Forwarded'));
    h.respond(n);
  }
  assert.equal(h.sends.length, 3);
});

test('reload on reconnect: no open within 3 s of mount sends one best-effort user load and nothing more', () => {
  const h = new Hook({ filter: fstate() });
  const mount = h.dispatch({ kind: 'mount' });
  assert.deepEqual(kinds(mount), ['startTimer:mount']);
  assert.equal(h.lastActions[0].delayMs, 3000);
  assert.deepEqual(kinds(h.fire('mount')), ['send']);
  assert.equal(h.lastSend().load.cause, 'user');
  h.respond(1, { records: [rec('seen', 10)], total: 1 });
  // The socket stays down: no timer, no request, however long it takes.
  assert.equal(h.timers.size, 0);
  assert.equal(h.sends.length, 1);
  assert.equal(H.isBusy(h.state), false);
  assert.equal(H.isBehind(h.state), false);
});

test('reload on reconnect: a record nobody received meanwhile is missing until the open, whose catch-up load adds it', () => {
  const h = new Hook({ filter: fstate() });
  const seen = rec('seen', 10), missed = rec('missed', 20);
  h.dispatch({ kind: 'mount' });
  h.fire('mount');
  h.respond(1, { records: [seen], total: 1 });
  // `missed` was saved after the best-effort load and the socket was down.
  assert.deepEqual(ids(h.rows), ['seen']);
  h.dispatch(open);
  assert.equal(h.lastSend().load.cause, 'catchUp');
  h.respond(2, { records: [seen, missed], total: 2 });
  assert.deepEqual(ids(h.rows), ['seen', 'missed']);
});

test('reload on reconnect: an open before the 3 s timer cancels it', () => {
  const h = new Hook({ filter: fstate() });
  h.dispatch({ kind: 'mount' });
  assert.deepEqual(kinds(h.dispatch(open)), ['cancelTimer:mount', 'send']);
  assert.deepEqual(h.dispatch({ kind: 'mountTimer' }), [], 'a late fire does nothing');
  assert.equal(h.sends.length, 1);
});

test('reload on reconnect: a load sent before the open cancels the fallback too', () => {
  const h = new Hook({ filter: fstate() });
  h.dispatch({ kind: 'mount' });
  assert.deepEqual(kinds(h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) })), ['cancelTimer:mount', 'send']);
  assert.equal(h.timers.size, 0);
});

test('reload on reconnect: a result holding a row stamped 15 s before the newest held row adds it; older held rows leave and older counts them', () => {
  const h = new Hook({ filter: fstate() });
  const held = [60, 120, 180, 240, 300].map(s => rec('h' + s, s));
  h.dispatch(open);
  h.respond(1, { records: held, total: 5 });
  assert.deepEqual(ids(h.rows), held.map(r => r.id));

  const withheld = rec('withheld', 285); // 15 s before the newest held row, never received
  const newer = rec('newer', 330);
  h.dispatch(open);
  // The range holds seven records; the cap here is 3.
  h.respond(2, { records: [withheld, held[4], newer], total: 7 });
  assert.deepEqual(ids(h.rows), ['withheld', 'h300', 'newer']);
  assert.equal(h.older, 4, 'the four held rows older than the result left the table, and are counted');
  assert.equal(h.matching(), 7);
});

// ===========================================================================
// nextRequests: unsettled answers and the retry series
// ===========================================================================

const stepOf = actions => actions.find(a => a.kind === 'startTimer' && a.timer === 'retry')?.step;

test('retry series: an unsettled answer schedules a catch-up load about 5 s later, and the notice shows', () => {
  const h = new Hook();
  h.dispatch(open);
  const actions = h.respond(1, { records: [rec('a', 1)], settled: false });
  assert.deepEqual(kinds(actions), ['apply', 'startTimer:retry']);
  assert.equal(H.retryNominalMs(stepOf(actions)), 5000);
  assert.equal(H.isBehind(h.state), true);
  assert.deepEqual(kinds(h.fire('retry')), ['send']);
  const load = h.lastSend().load;
  assert.equal(load.cause, 'catchUp');
  assert.equal(load.retry, true);
  assert.equal(load.generation, 2);
  assert.ok(load.query.includes('settle=true'));
  assert.equal(H.isBehind(h.state), true, 'the notice stays while the retry is in flight');
});

test('retry series: nominal 5, 10, 20, 40, 60, 60 ... s, and still scheduled after 100 unsettled answers', () => {
  const h = new Hook();
  h.dispatch(open);
  let generation = 1;
  const nominals = [];
  for (let n = 0; n < 100; n++) {
    const actions = h.respond(generation, { settled: false });
    nominals.push(H.retryNominalMs(stepOf(actions)));
    assert.equal(H.isBehind(h.state), true);
    assert.equal(h.state.error, null);
    h.fire('retry');
    generation++;
  }
  assert.deepEqual(nominals.slice(0, 8), [5000, 10000, 20000, 40000, 60000, 60000, 60000, 60000]);
  assert.ok(nominals.slice(4).every(ms => ms === 60000));
  assert.equal(nominals.length, 100);
  // The 101st answer is still unsettled and still schedules a retry.
  const last = h.respond(generation, { settled: false });
  assert.equal(H.retryNominalMs(stepOf(last)), 60000);
  assert.ok(H.nextRetryDelay(stepOf(last), 0.5) <= 60000);
});

test('retry series: a settled answer to any load stops it and clears the notice', () => {
  // Answer to the retry itself.
  const a = new Hook();
  a.dispatch(open);
  a.respond(1, { settled: false });
  a.fire('retry');
  assert.deepEqual(kinds(a.respond(2, { settled: true })), ['apply']);
  assert.equal(H.isBehind(a.state), false);
  assert.equal(a.timers.size, 0);

  // Answer to a socket open's catch-up load, with a retry pending.
  const b = new Hook();
  b.dispatch(open);
  b.respond(1, { settled: false });
  assert.deepEqual(kinds(b.dispatch(open)), ['cancelTimer:retry', 'send']);
  assert.equal(H.isBehind(b.state), true, 'the notice stays until an answer says otherwise');
  b.respond(2, { settled: true });
  assert.equal(H.isBehind(b.state), false);

  // Answer to a user load.
  const c = new Hook();
  c.dispatch(open);
  c.respond(1, { settled: false });
  c.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) });
  c.respond(2, { settled: true });
  assert.equal(H.isBehind(c.state), false);
  assert.equal(c.timers.size, 0);
});

test('retry series: a user load meanwhile cancels the pending retry; the series restarts from 5 s only if that load is unsettled too', () => {
  const run = settled => {
    const h = new Hook();
    h.dispatch(open);
    let g = 1;
    for (let n = 0; n < 3; n++) { h.respond(g, { settled: false }); h.fire('retry'); g++; }
    h.respond(g, { settled: false }); // retry number 3 is now pending (40 s)
    assert.equal(h.state.retryStep, 3);
    assert.deepEqual(
      kinds(h.dispatch({ kind: 'filter', filter: fstate({ status: 'Blocked' }) })),
      ['cancelTimer:retry', 'send'],
    );
    assert.equal(H.isBehind(h.state), true);
    return h.respond(g + 1, { settled });
  };
  assert.equal(H.retryNominalMs(stepOf(run(false))), 5000, 'restarts from 5 s');
  const settledActions = run(true);
  assert.equal(stepOf(settledActions), undefined, 'no retry after a settled answer');
});

test('retry series: a filter change or a socket open cancels a pending retry', () => {
  for (const event of [{ kind: 'filter', filter: fstate({ status: 'Blocked' }) }, open]) {
    const h = new Hook();
    h.dispatch(open);
    h.respond(1, { settled: false });
    assert.equal(h.state.retryTimer, true);
    const actions = h.dispatch(event);
    assert.deepEqual(kinds(actions), ['cancelTimer:retry', 'send']);
    assert.equal(h.state.retryTimer, false);
  }
});

test('retry series: an unsettled result keeps live rows it lacks, a settled one drops them', () => {
  const lacked = rec('lacked', 50);
  const stored = rec('stored', 60);
  for (const settled of [false, true]) {
    const h = new Hook();
    h.dispatch(open);
    h.respond(1, { records: [stored], total: 1 });
    h.onRecord(lacked); // received after the first load, before the next one is sent
    h.dispatch(open); // a reconnect's load
    h.respond(2, { records: [stored], total: 1, settled });
    assert.deepEqual(ids(h.rows), settled ? ['stored'] : ['lacked', 'stored'], `settled=${settled}`);
    if (!settled) {
      // The retry's settled answer ends it: the row is dropped if it still lacks it.
      h.fire('retry');
      h.respond(3, { records: [stored], total: 1, settled: true });
      assert.deepEqual(ids(h.rows), ['stored']);
    }
  }
});

test('retry series: rows from earlier loads are not kept by an unsettled result', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('fetchedBefore', 10), rec('b', 20)], total: 2 });
  h.dispatch(open);
  h.respond(2, { records: [rec('b', 20)], total: 1, settled: false });
  assert.deepEqual(ids(h.rows), ['b']);
});

test('retry series: a transient failure of a catch-up load joins it, with the error shown', () => {
  const h = new Hook();
  h.dispatch(open);
  const actions = h.fail(1, true, 'network');
  assert.deepEqual(kinds(actions), ['startTimer:retry']);
  assert.equal(H.retryNominalMs(stepOf(actions)), 5000);
  assert.equal(h.state.error, 'network');
  assert.equal(H.isBehind(h.state), true);
  h.fire('retry');
  const next = h.fail(2, true, 'timeout');
  assert.equal(H.retryNominalMs(stepOf(next)), 10000);
  assert.equal(h.state.error, 'timeout');
  h.fire('retry');
  h.respond(3, { settled: true });
  assert.equal(h.state.error, null, 'an answer clears the error');
  assert.equal(H.isBehind(h.state), false);
});

test('retry series: a failed user load is not retried', () => {
  const h = new Hook();
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) });
  assert.deepEqual(kinds(h.fail(1, true, 'network')), []);
  assert.equal(h.state.error, 'network');
  assert.equal(h.timers.size, 0);
  assert.equal(H.isBehind(h.state), false);
});

test('retry series: a no_wal answer for a device filter is an error and is not retried', () => {
  const h = new Hook({ filter: fstate({ device: 'abc' }) });
  h.dispatch(open);
  assert.deepEqual(kinds(h.noWal(1)), []);
  assert.match(h.state.error, /WAL/);
  assert.equal(h.timers.size, 0);
  assert.equal(H.isBehind(h.state), false);
  assert.equal(h.state.noWalInclude, false);
});

test('retry series: a permanent 500 on a catch-up load keeps the error, schedules nothing, and the next open tries again', () => {
  const h = new Hook();
  h.dispatch(open);
  assert.deepEqual(kinds(h.fail(1, false, 'HTTP 500')), []);
  assert.equal(h.state.error, 'HTTP 500');
  assert.equal(h.timers.size, 0);
  assert.equal(H.isBehind(h.state), false);
  assert.deepEqual(kinds(h.dispatch(open)), ['send']);
  assert.equal(h.lastSend().load.cause, 'catchUp');
  assert.equal(h.state.error, 'HTTP 500', 'the error stays visible until an answer replaces it');
});

test('retry series: a permanent 500 on a retry stops the running series', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { settled: false });
  h.fire('retry');
  assert.deepEqual(kinds(h.fail(2, false, 'HTTP 500')), []);
  assert.equal(h.state.error, 'HTTP 500');
  assert.equal(h.timers.size, 0);
  assert.equal(H.isBehind(h.state), false);
  assert.deepEqual(kinds(h.dispatch(open)), ['send']);
});

test('retry series: a 500 with a retryable code (a fixture; the server sends none) is retried', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { settled: false });
  h.fire('retry');
  // The hook classifies the answer as transient; the machine only sees that.
  const actions = h.fail(2, true, 'HTTP 500: retryable');
  assert.equal(H.retryNominalMs(stepOf(actions)), 10000);
  assert.equal(H.isBehind(h.state), true);
});

test('retry series: a stale timer fire does nothing', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { settled: false });
  h.dispatch(open); // cancels the retry timer
  assert.deepEqual(h.dispatch({ kind: 'retryTimer' }), []);
  assert.equal(h.sends.length, 2);
});

// ===========================================================================
// nextRequests: pause
// ===========================================================================

test('pause: a catch-up load completing while paused is held; resume replaces the rows and carries the buffered live rows received after it was sent', () => {
  const h = new Hook();
  const a1 = rec('a1', 10), a2 = rec('a2', 20), l1 = rec('l1', 30);
  h.dispatch(open);
  h.respond(1, { records: [a1, a2], total: 2 });
  h.onRecord(l1);
  assert.deepEqual(ids(h.rows), ['a1', 'a2', 'l1']);

  h.dispatch({ kind: 'pause' });
  const b1 = rec('b1', 40); // received before the next load is sent, never stored
  h.onRecord(b1);
  h.dispatch(open); // reconnect while paused
  assert.equal(h.lastSend().load.cause, 'catchUp');
  const b2 = rec('b2', 50); // received after the send, and in its result
  const b3 = rec('b3', 60); // received after the send, saved after the snapshot
  h.onRecord(b2);
  const rows = ids(h.rows);

  const b2Fetched = rec('b2', 50, { rssi: -1 });
  const actions = h.respond(2, { records: [a1, a2, l1, b2Fetched], total: 4 });
  assert.deepEqual(kinds(actions), ['hold']);
  assert.deepEqual(ids(h.rows), rows, 'the table does not change while paused');
  h.onRecord(b3);
  assert.deepEqual(ids(h.rows), rows);

  const resumed = h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(kinds(resumed), ['applyHeld']);
  assert.deepEqual(ids(h.rows), ['a1', 'a2', 'l1', 'b2', 'b3'], 'b1 was received before the send and is missing: dropped');
  assert.equal(h.rows.find(r => r.id === 'b2').rssi, -1, 'one copy of b2, the fetched one');
  assert.equal(h.buffer.length, 0);
  assert.equal(h.state.held, null);
});

test('pause: a catch-up answer that arrives after resume is applied as any load', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 10)], total: 1 });
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // generation 2
  assert.deepEqual(kinds(h.dispatch({ kind: 'resume', overflowed: false })), ['mergeBuffer']);
  assert.deepEqual(kinds(h.respond(2, { records: [rec('a', 10), rec('c', 30)], total: 2 })), ['apply']);
  assert.deepEqual(ids(h.rows), ['a', 'c']);
});

test('pause: two reconnects while paused hold only the second', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 10)], total: 1 });
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // generation 2
  assert.deepEqual(kinds(h.dispatch(open)), ['abort', 'send']); // generation 3
  assert.deepEqual(kinds(h.respond(2, { records: [rec('first', 20)] })), ['ignore']);
  assert.deepEqual(kinds(h.respond(3, { records: [rec('a', 10), rec('second', 30)], total: 2 })), ['hold']);
  assert.equal(h.state.held.load.generation, 3);
  assert.deepEqual(ids(h.rows), ['a'], 'unchanged');
  h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(ids(h.rows), ['a', 'second']);
});

test('pause: a held answer is replaced by a newer catch-up load', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [], total: 0 });
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // 2
  h.respond(2, { records: [rec('held', 20)], total: 1 });
  assert.notEqual(h.state.held, null);
  h.dispatch(open); // 3, discards the held answer
  assert.equal(h.state.held, null);
  h.respond(3, { records: [rec('newer', 30)], total: 1 });
  h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(ids(h.rows), ['newer']);
});

test('pause: a user load while paused replaces the rows and discards a held catch-up load; the buffer merges on resume, keeping rows that match', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 10)], total: 1 });
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // 2, catch-up
  h.respond(2, { records: [rec('a', 10), rec('heldRow', 20)], total: 2 });
  assert.notEqual(h.state.held, null);

  const early = rec('early', 40, { gateway_eui: 'G1' }); // buffered before the user load is sent
  const wrong = rec('wrong', 41, { gateway_eui: 'G2' });
  h.onRecord(early);
  h.onRecord(wrong);

  const actions = h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) }); // 3, user
  assert.deepEqual(kinds(actions), ['send']);
  assert.equal(h.state.held, null, 'the held catch-up load is discarded');

  const late = rec('late', 50, { gateway_eui: 'G1' }); // buffered after it is sent, saved after its snapshot
  const stored = rec('stored', 45, { gateway_eui: 'G1' });
  h.onRecord(late);
  h.onRecord(stored);

  // The user load's answer replaces the table while still paused. It holds
  // `stored` and lacks `early` (received before the send, so in its snapshot
  // unless beyond the cap) and `late` (saved after the snapshot).
  assert.deepEqual(kinds(h.respond(3, { records: [rec('g1', 15, { gateway_eui: 'G1' }), stored], total: 2 })), ['apply']);
  assert.equal(h.state.paused, true);
  assert.deepEqual(ids(h.rows), ['g1', 'stored', 'late'], 'late is carried; early and the wrong gateway are not');

  // Resume merges what the buffer holds that matches the new filter and the
  // table lacks: early comes back, the wrong gateway does not.
  assert.deepEqual(kinds(h.dispatch({ kind: 'resume', overflowed: false })), ['mergeBuffer']);
  assert.deepEqual(ids(h.rows), ['g1', 'early', 'stored', 'late']);
  assert.equal(h.buffer.length, 0);
});

test('pause: without a held load, resume merges the buffer without duplicates, the table copy winning', () => {
  const h = new Hook();
  h.dispatch(open);
  const fetched = rec('a', 10, { rssi: -50 });
  h.respond(1, { records: [fetched], total: 1 });
  h.dispatch({ kind: 'pause' });
  h.onRecord(rec('a', 10, { rssi: -99 })); // the same record again, live
  h.onRecord(rec('b', 20));
  h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(ids(h.rows), ['a', 'b']);
  assert.equal(h.rows[0], fetched);
});

test('pause: an overflowed buffer sends one user load on resume and drops the buffer and any held load', () => {
  const h = new Hook({ max: 3 });
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 1)], total: 1 });
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // 2
  h.respond(2, { records: [rec('held', 2)], total: 1 });
  assert.notEqual(h.state.held, null);
  for (let i = 0; i < 4; i++) h.onRecord(rec('buf' + i, 10 + i));
  assert.equal(h.overflowed, true, 'more rows than the buffer may hold');
  const actions = h.dispatch({ kind: 'resume', overflowed: true });
  assert.deepEqual(kinds(actions), ['send']);
  assert.equal(h.lastSend().load.cause, 'user');
  assert.equal(h.state.held, null);
  assert.equal(h.buffer.length, 0);
  assert.equal(h.state.paused, false);
  assert.deepEqual(ids(h.rows), ['a'], 'the table is replaced when the load answers');
  h.respond(3, { records: [rec('a', 1), rec('z', 99)], total: 2 });
  assert.deepEqual(ids(h.rows), ['a', 'z']);
});

test('pause: resume with a catch-up load in flight and an overflowed buffer aborts it for the user load', () => {
  const h = new Hook({ max: 2 });
  h.dispatch(open);
  h.respond(1);
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // 2, in flight
  for (let i = 0; i < 3; i++) h.onRecord(rec('b' + i, i));
  assert.deepEqual(kinds(h.dispatch({ kind: 'resume', overflowed: true })), ['abort', 'send']);
  assert.deepEqual(kinds(h.respond(2, { records: [rec('late', 1)] })), ['ignore']);
});

test('pause: resume when not paused does nothing', () => {
  const h = new Hook();
  assert.deepEqual(h.dispatch({ kind: 'resume', overflowed: false }), []);
});

test('pause: a retry load completing while paused is held too, and the series goes on', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 10)], total: 1, settled: false });
  h.dispatch({ kind: 'pause' });
  h.fire('retry');
  const actions = h.respond(2, { records: [rec('a', 10), rec('b', 20)], total: 2, settled: false });
  assert.deepEqual(kinds(actions), ['hold', 'startTimer:retry']);
  assert.equal(H.retryNominalMs(stepOf(actions)), 10000);
  assert.deepEqual(ids(h.rows), ['a']);
  h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(ids(h.rows), ['a', 'b']);
});

test('pause: an unsettled held load keeps all buffered live rows it lacks on resume', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [], total: 0 });
  h.dispatch({ kind: 'pause' });
  const early = rec('early', 10);
  h.onRecord(early); // buffered before the next load is sent
  h.dispatch(open);
  h.respond(2, { records: [rec('stored', 20)], total: 1, settled: false });
  h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(ids(h.rows), ['early', 'stored']);
});

// ===========================================================================
// nextRequests: no WAL
// ===========================================================================

test('no WAL: malformed=exclude alone answered 503 no_wal is resent with include; later loads go straight to include until the next open', () => {
  const h = new Hook({ filter: fstate({ timePreset: '5m' }), nowMs: BASE + 60_000 });
  h.dispatch(open);
  const first = h.lastSend().load;
  assert.ok(first.query.includes('malformed=exclude'));
  const actions = h.noWal(1);
  assert.deepEqual(kinds(actions), ['resend']);
  const resent = h.lastSend().load;
  assert.equal(resent.generation, first.generation);
  assert.equal(resent.cause, first.cause);
  assert.equal(resent.includeMalformed, true);
  assert.ok(!resent.query.includes('malformed='), 'the resend asks for malformed frames too');
  assert.ok(resent.query.includes('from='), 'the time range is kept');
  assert.equal(resent.filter.hideMalformed, true, 'the browser still hides them');
  assert.equal(h.state.noWalInclude, true);

  // The answer holds a malformed row; the browser hides it.
  const valid = rec('v', 10), bad = rec('m', 11, { malformed: 'crc', dev_addr: null });
  h.respond(1, { records: [valid, bad], total: 2, malformed: null });
  assert.deepEqual(ids(h.rows), ['v', 'm']);
  assert.deepEqual(ids(h.shown()), ['v']);

  // A later load asks for include at once.
  h.dispatch({ kind: 'filter', filter: fstate({ timePreset: '15m' }) });
  assert.ok(!h.lastSend().load.query.includes('malformed='));
  assert.equal(h.lastSend().load.includeMalformed, true);
  h.respond(2, { records: [valid], total: 1 });

  // The next socket open goes back to the normal request.
  h.dispatch(open);
  assert.ok(h.lastSend().load.query.includes('malformed=exclude'));
  assert.equal(h.state.noWalInclude, false);
});

test('no WAL: a device filter answered 503 shows the error and is not retried, not even later', () => {
  const h = new Hook({ filter: fstate({ device: 'abc' }) });
  h.dispatch(open);
  h.noWal(1);
  assert.equal(h.sends.length, 1, 'nothing is resent');
  assert.match(h.state.error, /WAL/);
  assert.equal(h.timers.size, 0);
  assert.equal(h.state.noWalInclude, false);
  // A resent request cannot be refused a second time as a fallback.
  const again = new Hook({ filter: fstate() });
  again.dispatch(open);
  again.noWal(1);
  again.noWal(1); // the resend refused as well
  assert.match(again.state.error, /WAL/);
  assert.equal(again.sends.length, 2);
});

test('no WAL: a no_wal answer for an old generation is ignored', () => {
  const h = new Hook();
  h.dispatch(open);
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) });
  assert.deepEqual(kinds(h.noWal(1)), ['ignore']);
  assert.equal(h.sends.length, 2);
  assert.equal(h.state.noWalInclude, false);
});

test('no WAL while paused: a catch-up load answered no_wal is resent, its answer held, and the table waits for resume', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 10)], total: 1 });
  h.dispatch({ kind: 'pause' });
  h.dispatch(open); // 2, catch-up
  assert.deepEqual(kinds(h.noWal(2)), ['resend']);
  assert.equal(h.lastSend().load.cause, 'catchUp');
  assert.equal(h.lastSend().load.generation, 2);
  assert.deepEqual(kinds(h.respond(2, { records: [rec('a', 10), rec('b', 20)], total: 2 })), ['hold']);
  assert.deepEqual(ids(h.rows), ['a']);
  h.dispatch({ kind: 'resume', overflowed: false });
  assert.deepEqual(ids(h.rows), ['a', 'b']);
});

test('no WAL, overtaken: a resend overtaken by a newer load is ignored, rows and counts, paused or not', () => {
  for (const paused of [false, true]) {
    const h = new Hook();
    h.dispatch(open);
    h.respond(1, { records: [rec('a', 10)], total: 1, malformed: 0 });
    if (paused) h.dispatch({ kind: 'pause' });
    h.dispatch({ kind: 'filter', filter: fstate({ timePreset: '15m' }) }); // 2, user
    h.noWal(2); // resent
    const overtakers = [
      { kind: 'filter', filter: fstate({ timePreset: '1h' }) },
      open,
    ];
    for (const event of overtakers) {
      const before = { rows: ids(h.rows), older: h.older, malformed: h.malformedFigure() };
      h.dispatch(event); // a newer load
      const stale = h.state.generation - 1;
      assert.deepEqual(kinds(h.respond(stale, { records: [rec('stale', 99)], total: 50, malformed: 7 })), ['ignore']);
      assert.deepEqual({ rows: ids(h.rows), older: h.older, malformed: h.malformedFigure() }, before, `paused=${paused}`);
    }
  }
});

// ===========================================================================
// Back-off
// ===========================================================================

test('nextReconnectDelay: nominal 3, 6, 12, 24, 48, 60, 60 s for connections under a minute', () => {
  let previous = null;
  const got = [];
  for (let i = 0; i < 7; i++) {
    const next = H.nextReconnect(previous, 1000, 0.999999999);
    got.push(next.nominalMs);
    previous = next.nominalMs;
  }
  assert.deepEqual(got, [3000, 6000, 12000, 24000, 48000, 60000, 60000]);
  // random just under 1 gives just under the nominal; 1 gives the nominal itself.
  assert.equal(H.nextReconnectDelay(null, 0, 1), 3000);
});

test('nextReconnectDelay: 3 s again after a connection of 60 s', () => {
  assert.equal(H.nextReconnect(48000, 60_000, 1).nominalMs, 3000);
  assert.equal(H.nextReconnect(60000, 600_000, 1).nominalMs, 3000);
  assert.equal(H.nextReconnect(48000, 59_999, 1).nominalMs, 60000, 'just under a minute does not reset');
  assert.equal(H.nextReconnect(6000, 59_999, 1).nominalMs, 12000);
});

test('nextReconnectDelay: random 0 gives half of each nominal wait, just under 1 gives just under all of it', () => {
  const nominals = [3000, 6000, 12000, 24000, 48000, 60000, 60000];
  let previous = null;
  for (const nominal of nominals) {
    const low = H.nextReconnectDelay(previous, 1000, 0);
    const high = H.nextReconnectDelay(previous, 1000, 0.9999999);
    assert.equal(low, nominal / 2);
    assert.ok(high < nominal && high > nominal * 0.999, `${high} just under ${nominal}`);
    previous = nominal;
  }
  // The doubling follows the nominal waits, never the jittered ones.
  let jittered = null;
  let nominal = null;
  const seen = [];
  for (let i = 0; i < 5; i++) {
    const next = H.nextReconnect(nominal, 1000, 0);
    seen.push(next.delayMs);
    nominal = next.nominalMs;
    jittered = next.delayMs;
  }
  assert.deepEqual(seen, [1500, 3000, 6000, 12000, 24000]);
  assert.ok(jittered < nominal);
});

test('nextRetryDelay: the same jitter bounds for every step', () => {
  const nominals = [5000, 10000, 20000, 40000, 60000, 60000, 60000];
  nominals.forEach((nominal, step) => {
    assert.equal(H.retryNominalMs(step), nominal);
    assert.equal(H.nextRetryDelay(step, 0), nominal / 2);
    const high = H.nextRetryDelay(step, 0.9999999);
    assert.ok(high < nominal && high > nominal * 0.999);
    assert.equal(H.nextRetryDelay(step, 1), nominal);
  });
  assert.equal(H.nextRetryDelay(1000, 0), 30000, 'any later step is the last one');
  assert.equal(H.nextRetryDelay(-3, 1), 5000);
});

// ===========================================================================
// A 401: the session ended
// ===========================================================================

test('classifyFailedLoad: a 401 is unauthorized, not an error and not a retry', () => {
  assert.equal(H.classifyFailedLoad(401, null), 'unauthorized');
  // The status decides alone: no body code can turn a 401 into something retried.
  assert.equal(H.classifyFailedLoad(401, 'busy'), 'unauthorized');
  assert.equal(H.classifyFailedLoad(401, 'no_wal'), 'unauthorized');
  assert.equal(H.classifyFailedLoad(401, 'anything'), 'unauthorized');
});

test('classifyFailedLoad: the other statuses keep their meaning', () => {
  assert.equal(H.classifyFailedLoad(503, 'no_wal'), 'noWal');
  assert.equal(H.classifyFailedLoad(503, 'busy'), 'busy');
  assert.equal(H.classifyFailedLoad(503, null), 'failed');
  assert.equal(H.classifyFailedLoad(503, 'other'), 'failed');
  // The codes belong to a 503 only.
  assert.equal(H.classifyFailedLoad(500, 'busy'), 'failed');
  assert.equal(H.classifyFailedLoad(500, 'no_wal'), 'failed');
  for (const status of [400, 403, 404, 429, 500, 502, 504]) {
    assert.equal(H.classifyFailedLoad(status, null), 'failed', `status ${status}`);
  }
});

test('unauthorized: a 401 ends the load without an error, a timer or another request', () => {
  const h = new Hook();
  h.dispatch({ kind: 'mount' });
  h.fire('mount');
  assert.notEqual(h.state.inFlight, null);
  const sends = h.sends.length;

  const actions = h.unauthorized(h.state.inFlight.generation);

  assert.deepEqual(actions, []);
  assert.equal(h.state.inFlight, null);
  assert.equal(h.state.error, null, 'no error strip');
  assert.equal(h.state.retryStep, null);
  assert.equal(H.isBehind(h.state), false);
  assert.equal(H.isBusy(h.state), false);
  assert.equal(h.timers.size, 0, 'nothing is scheduled');
  assert.equal(h.sends.length, sends, 'nothing is sent again');
});

test('unauthorized: the reload after a reconnect that gets a 401 is not retried either', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 1)] });
  // The socket drops and comes back: a catch-up load, which a transient
  // failure would put in the retry series.
  h.dispatch(open);
  const load = h.lastSend().load;
  assert.equal(load.cause, 'catchUp');

  const actions = h.unauthorized(load.generation);

  assert.deepEqual(kinds(actions), []);
  assert.equal(h.state.error, null);
  assert.equal(h.state.retryStep, null);
  assert.equal(h.state.retryTimer, false);
  assert.equal(H.isBehind(h.state), false);
  assert.equal(h.timers.size, 0);
  assert.deepEqual(ids(h.rows), ['a'], 'the rows already held are left alone');
});

test('unauthorized: contrast, a transient failure of the same load does start the retry series and set an error', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { records: [rec('a', 1)] });
  h.dispatch(open);
  const actions = h.fail(h.lastSend().load.generation, true, 'could not reach the sniffer');
  assert.deepEqual(kinds(actions), ['startTimer:retry']);
  assert.equal(h.state.error, 'could not reach the sniffer');
  assert.equal(H.isBehind(h.state), true);
});

test('unauthorized: a 401 to a retry ends a running series', () => {
  const h = new Hook();
  h.dispatch(open);
  h.respond(1, { settled: false });
  assert.equal(H.isBehind(h.state), true);
  h.fire('retry');
  const retry = h.lastSend().load;
  assert.equal(retry.retry, true);

  h.unauthorized(retry.generation);

  assert.equal(H.isBehind(h.state), false);
  assert.equal(h.state.retryStep, null);
  assert.equal(h.state.error, null);
  assert.equal(h.timers.size, 0);
});

test('unauthorized: a 401 to a load that was overtaken changes nothing', () => {
  const h = new Hook();
  h.dispatch(open);
  const first = h.lastSend().load;
  h.dispatch({ kind: 'filter', filter: fstate({ gateway: 'G1' }) });
  const second = h.lastSend().load;
  assert.notEqual(first.generation, second.generation);

  const before = h.state;
  const actions = h.unauthorized(first.generation);

  assert.deepEqual(kinds(actions), ['ignore']);
  assert.equal(h.state, before);
  assert.equal(h.state.inFlight.generation, second.generation, 'the newer load is still awaited');
});

// ===========================================================================
// Summary
// ===========================================================================

const failed = results.filter(r => !r.ok);
console.log(`\n${results.length - failed.length} passed, ${failed.length} failed, ${results.length} cases`);
if (failed.length > 0) {
  console.log('Failed:');
  for (const r of failed) console.log(`  - ${r.name}`);
  process.exit(1);
}
