/**
 * The WebUI's history logic, as pure functions: no React, no timers, no
 * `fetch`. The hooks only run what these functions decide, and `npm test`
 * runs them on their own.
 *
 * This file must keep only `import type` imports. The Node test transpiles it
 * alone and loads the result from a `data:` URL, where a relative runtime
 * import cannot be resolved.
 *
 * What it holds:
 * - the matching rule, shared by the server (stored records) and the browser
 *   (live records): `matchesFilter`, with the query string that asks the
 *   server for the same thing, `historyParams`;
 * - merging: `mergeRecords`, `mergeAfterLoad`, and the count arithmetic;
 * - the request state machine, `nextRequests`, and `classifyFailedLoad`, which
 *   says what the status of a load that did not return 200 means;
 * - the back-off series for retries and reconnects.
 */
import type { MessageRecord, MType, Status } from '../types/messages';
import type { FilterState } from '../components/FilterBar';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Most records one load returns (the server never returns more). */
export const HISTORY_LIMIT = 5_000;

/** Most records the table holds; the oldest are dropped first. */
export const MAX_MESSAGES = 10_000;

/** Pause after the last keystroke in the device box before a load is sent. */
export const TYPING_DELAY_MS = 400;

/** Wait for the live connection to open before loading without it. */
export const MOUNT_FALLBACK_MS = 3_000;

/** A load that has not answered after this long is a transient failure. */
export const LOAD_TIMEOUT_MS = 30_000;

/** Time presets of the filter bar, as a span counted back from now. */
export const TIME_PRESET_MS: Record<string, number> = {
  '5m':  5  * 60 * 1000,
  '15m': 15 * 60 * 1000,
  '1h':  60 * 60 * 1000,
  '24h': 24 * 60 * 60 * 1000,
};

// ---------------------------------------------------------------------------
// Records and the matching rule
// ---------------------------------------------------------------------------

/**
 * The identifier a row can be labelled by: the DevAddr of a data frame, or the
 * DevEUI of a join request. A record never carries both. Join accepts, rejoin
 * requests, proprietary frames, truncated frames and malformed frames carry
 * neither, so they have no identifier and cannot be labelled.
 */
export function deviceId(m: Pick<MessageRecord, 'dev_addr' | 'dev_eui'>): string | null {
  return m.dev_addr ?? m.dev_eui ?? null;
}

/** The fields the matching rule reads. */
export type FilterableRecord = Pick<
  MessageRecord,
  'timestamp' | 'mtype' | 'status' | 'gateway_eui' | 'dev_addr' | 'dev_eui' | 'malformed'
>;

/** What merging needs besides the matching fields: an identity. */
export type HistoryRow = FilterableRecord & Pick<MessageRecord, 'id'>;

/**
 * The filters of one load, in the form that is sent to the server and used to
 * decide which live records belong in the table.
 */
export interface HistoryFilter {
  /** Device text as typed: not trimmed, matched as a substring. '' = any. */
  device: string;
  /** Selected types; empty = any. */
  mtypes: MType[];
  /** 'all' or a status. */
  status: 'all' | Status;
  /** Gateway EUI, compared exactly; '' = any. */
  gateway: string;
  /** Inclusive lower bound of the time range, in epoch milliseconds; null = no bound. */
  fromMs: number | null;
  /** Leave out malformed frames. */
  hideMalformed: boolean;
}

/**
 * The filter state of the filter bar as a history filter. `nowMs` is read once
 * per load and used both for the request and for the check of the live records
 * that load carries over, so the two cannot disagree about where the range
 * starts.
 */
export function toHistoryFilter(f: FilterState, nowMs: number): HistoryFilter {
  const span = TIME_PRESET_MS[f.timePreset];
  return {
    device: f.device,
    mtypes: [...f.mtype],
    status: f.status,
    gateway: f.gateway,
    fromMs: span === undefined ? null : nowMs - span,
    hideMalformed: !f.showMalformed,
  };
}

/**
 * Whether a record belongs in a view with filter `f`. The server applies the
 * same rules to stored records, so a record is shown or hidden the same way
 * whichever side decided.
 *
 * - Device: a case-insensitive substring of the DevAddr, the DevEUI, or the
 *   label of the record's identifier (see `deviceId`).
 * - Type: one of the selected types. An `Unknown` record is therefore left out
 *   whenever any type is selected, since no type filter can select it.
 * - Time: the record's time, truncated to the millisecond, is not before the
 *   bound. Truncating is what a browser's `Date` does with the sniffer's
 *   nanosecond timestamps, and the server's inclusive bound gives the same
 *   answer only for a bound on a whole millisecond.
 *
 * `serverMatchedDevice`: the row came from a load made for this device text
 * and the browser has no label snapshot yet, so the server already applied the
 * device rule with labels the browser cannot see. The rule counts as met.
 */
export function matchesFilter(
  m: FilterableRecord,
  f: HistoryFilter,
  labels: Record<string, string>,
  opts?: { serverMatchedDevice?: boolean },
): boolean {
  if (f.device !== '' && opts?.serverMatchedDevice !== true) {
    const q = f.device.toLowerCase();
    let hit =
      (!!m.dev_addr && m.dev_addr.toLowerCase().includes(q)) ||
      (!!m.dev_eui && m.dev_eui.toLowerCase().includes(q));
    if (!hit) {
      const id = deviceId(m);
      const label = id !== null ? labels[id] : undefined;
      hit = typeof label === 'string' && label.toLowerCase().includes(q);
    }
    if (!hit) return false;
  }

  if (f.mtypes.length > 0 && !f.mtypes.includes(m.mtype)) return false;
  if (f.status !== 'all' && m.status !== f.status) return false;
  if (f.gateway !== '' && m.gateway_eui !== f.gateway) return false;
  if (f.fromMs !== null && !(parseTimestampMs(m.timestamp) >= f.fromMs)) return false;
  if (f.hideMalformed && m.malformed != null) return false;
  return true;
}

/**
 * Whether a record counts towards the malformed figure of a view: it is
 * malformed and passes every filter except the one that hides malformed
 * frames. The figure is the same with the box checked or not, so it can be
 * checked against the table either way.
 */
export function countsAsMalformed(
  m: FilterableRecord,
  f: HistoryFilter,
  labels: Record<string, string>,
): boolean {
  return m.malformed != null && matchesFilter(m, { ...f, hideMalformed: false }, labels);
}

/**
 * Whether hiding malformed frames is the only row filter of `f`. A database
 * that cannot serve filtered scans still serves a plain time range, so this is
 * the one case where the browser can load the range with malformed frames
 * included and hide them itself.
 */
export function onlyMalformedFilter(f: HistoryFilter): boolean {
  return f.hideMalformed && f.device === '' && f.mtypes.length === 0 &&
    f.status === 'all' && f.gateway === '';
}

// ---------------------------------------------------------------------------
// Query string
// ---------------------------------------------------------------------------

/**
 * The filter part of the `/api/messages` query, in a fixed order: from,
 * device, mtype, status, gateway, malformed. Filters that select everything
 * are left out, and `malformed` is sent only to exclude. Every value is
 * percent-encoded, so a `+` in the device text can never arrive as a space,
 * and `from` is `toISOString()` (milliseconds and `Z`), which the server
 * parses exactly.
 */
export function historyParams(f: HistoryFilter): string {
  const parts: string[] = [];
  if (f.fromMs !== null) parts.push(`from=${encodeURIComponent(new Date(f.fromMs).toISOString())}`);
  if (f.device !== '') parts.push(`device=${encodeURIComponent(f.device)}`);
  if (f.mtypes.length > 0) parts.push(`mtype=${f.mtypes.map(encodeURIComponent).join(',')}`);
  if (f.status !== 'all') parts.push(`status=${encodeURIComponent(f.status)}`);
  if (f.gateway !== '') parts.push(`gateway=${encodeURIComponent(f.gateway)}`);
  if (f.hideMalformed) parts.push('malformed=exclude');
  return parts.join('&');
}

/**
 * The whole query of a load: the filters, the newest records first (so the
 * cap cuts the old end), an exact count, and a wait for the storage writer to
 * catch up, so a record broadcast before the request is in the answer.
 */
export function loadQuery(f: HistoryFilter): string {
  const tail = `newest=true&count=true&settle=true&limit=${HISTORY_LIMIT}`;
  const filters = historyParams(f);
  return filters === '' ? tail : `${filters}&${tail}`;
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

interface TimeParts {
  /** Epoch milliseconds, truncated; NaN when the text is not a time. */
  ms: number;
  /** Nanoseconds past the millisecond, 0 to 999,999. */
  sub: number;
}

const CHAR_DOT = 46;
const CHAR_0 = 48;
const CHAR_9 = 57;

/**
 * Splits an ISO-8601 time into whole milliseconds and the rest. The sniffer
 * writes as many fraction digits as a time needs (none, 3, 6 or 9), and
 * browsers differ on more than 3, so the fraction is cut to milliseconds
 * before it reaches `Date.parse`.
 */
function timeParts(timestamp: string): TimeParts {
  // "YYYY-MM-DDTHH:MM:SS" is 19 characters; a fraction starts right after.
  if (timestamp.length < 21 || timestamp.charCodeAt(19) !== CHAR_DOT) {
    return { ms: Date.parse(timestamp), sub: 0 };
  }
  let end = 20;
  while (end < timestamp.length) {
    const c = timestamp.charCodeAt(end);
    if (c < CHAR_0 || c > CHAR_9) break;
    end++;
  }
  const digits = timestamp.slice(20, end);
  if (digits.length === 3) return { ms: Date.parse(timestamp), sub: 0 };
  const fraction = digits.padEnd(9, '0');
  return {
    ms: Date.parse(`${timestamp.slice(0, 19)}.${fraction.slice(0, 3)}${timestamp.slice(end)}`),
    sub: Number(fraction.slice(3, 9)),
  };
}

/** A time in epoch milliseconds, truncated to the millisecond; NaN if unparsable. */
export function parseTimestampMs(timestamp: string): number {
  return timeParts(timestamp).ms;
}

function compareParts(a: TimeParts, b: TimeParts): number {
  // An unparsable time sorts first, so the order stays consistent.
  const am = Number.isNaN(a.ms) ? -Infinity : a.ms;
  const bm = Number.isNaN(b.ms) ? -Infinity : b.ms;
  if (am !== bm) return am < bm ? -1 : 1;
  return a.sub - b.sub;
}

/**
 * Orders two ISO-8601 times as times, to the nanosecond. Comparing the text
 * would put ".5Z" after ".500001Z", and `Date.parse` alone drops everything
 * below the millisecond, which leaves two rows inside one millisecond in
 * arbitrary order.
 */
export function compareTimestamps(a: string, b: string): number {
  return compareParts(timeParts(a), timeParts(b));
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

/**
 * `base` and `extra` merged into one list, oldest first, without duplicate
 * ids, keeping the newest `max` rows. `dropped` is how many rows were cut from
 * the old end.
 *
 * When both sides hold an id, the copy in `base` wins. Pass what the database
 * returned, or what the table holds, as `base` and live records as `extra`:
 * the copy from the database is the one that stays.
 */
export function mergeRecords<T extends Pick<MessageRecord, 'id' | 'timestamp'>>(
  base: readonly T[],
  extra: readonly T[],
  max: number,
): { records: T[]; dropped: number } {
  const seen = new Set<string>();
  const items: { record: T; time: TimeParts }[] = [];
  let ordered = true;

  const add = (record: T) => {
    if (seen.has(record.id)) return;
    seen.add(record.id);
    const time = timeParts(record.timestamp);
    const last = items[items.length - 1];
    if (last !== undefined && compareParts(last.time, time) > 0) ordered = false;
    items.push({ record, time });
  };
  base.forEach(add);
  extra.forEach(add);

  // Array.prototype.sort is stable, so rows with equal times keep the order
  // they were added in.
  if (!ordered) items.sort((a, b) => compareParts(a.time, b.time));

  const dropped = Math.max(0, items.length - Math.max(0, max));
  return { records: items.slice(dropped).map(i => i.record), dropped };
}

/** The live records a load may have to carry over. */
export interface LiveRecords<T> {
  /** Raw live records received after the load was sent. */
  sinceSent: readonly T[];
  /** Every live record the table holds (and the pause buffer), whenever it was received. */
  held: readonly T[];
}

/**
 * The rows after a load completes: the load's result, plus the live records
 * the result lacks that match `f`. Rows held from earlier loads and missing
 * from the result are dropped, as a load replaces rows.
 *
 * Which live records are carried depends on whether the result is settled,
 * that is, whether the server had processed everything queued before the
 * request:
 * - settled: only records received after the load was sent. One received
 *   earlier was queued before the request, so it is in the result unless it
 *   lies beyond the cap, and one received later is either in the result or was
 *   saved after the snapshot.
 * - unsettled: every live record held, whenever it was received, because the
 *   writer was behind and the result can lack records the browser already
 *   showed. Rows that came from earlier loads are not kept this way: they came
 *   from the database, so a result that lacks them has them beyond its cap or
 *   out of its range.
 *
 * Membership by id decides, never a timestamp, so records stamped out of order
 * by different gateway tasks can neither be dropped nor doubled.
 */
export function mergeAfterLoad<T extends HistoryRow>(
  result: readonly T[],
  settled: boolean,
  live: LiveRecords<T>,
  f: HistoryFilter,
  labels: Record<string, string>,
  max: number,
): { records: T[]; dropped: number } {
  const inResult = new Set(result.map(r => r.id));
  const candidates = settled ? live.sinceSent : [...live.held, ...live.sinceSent];
  const carried = candidates.filter(r => !inResult.has(r.id) && matchesFilter(r, f, labels));
  return mergeRecords(result, carried, max);
}

// ---------------------------------------------------------------------------
// Counts
// ---------------------------------------------------------------------------

/**
 * A count header as a number: a non-negative integer, or null when the header
 * is missing (an older server) or not a number.
 */
export function parseCountHeader(value: string | null): number | null {
  if (value === null || !/^\d+$/.test(value.trim())) return null;
  const n = Number(value.trim());
  return Number.isSafeInteger(n) ? n : null;
}

/** `X-Storage-Write-Failures` as a number; 0 when the header is missing. */
export function parseWriteFailures(value: string | null): number {
  return parseCountHeader(value) ?? 0;
}

/**
 * Matching records older than the oldest held row, after a load: the load's
 * total less the rows it returned, plus the held rows cut at `MAX_MESSAGES`.
 * A missing total (an older server) counts nothing beyond the rows.
 */
export function olderAfterLoad(total: number | null, returned: number, dropped: number): number {
  const beyond = total === null ? 0 : Math.max(0, total - returned);
  return beyond + dropped;
}

/**
 * The footer's Matching: the rows shown plus the older matching records. Those
 * count only while the oldest held row is still inside the range; once it
 * has aged out, everything older has too.
 */
export function matchingCount(
  shown: number,
  older: number,
  oldest: Pick<MessageRecord, 'timestamp'> | undefined,
  fromMs: number | null,
): number {
  if (oldest === undefined) return shown;
  if (fromMs !== null && parseTimestampMs(oldest.timestamp) < fromMs) return shown;
  return shown + older;
}

/**
 * The malformed figure right after a load: the server's count plus the
 * matching malformed live records received since the load was sent. A record
 * in both is counted twice, so the figure can be a little high, never low.
 * Null when the server sent no count.
 */
export function malformedAfterLoad(
  header: number | null,
  liveMalformedSinceSent: number,
): number | null {
  return header === null ? null : header + liveMalformedSinceSent;
}

// ---------------------------------------------------------------------------
// Back-off
// ---------------------------------------------------------------------------

/** A wait of `nominalMs`, spread to between half and all of it. */
function jitter(nominalMs: number, random: number): number {
  const r = Math.min(1, Math.max(0, random));
  return nominalMs * (0.5 + r / 2);
}

const RETRY_NOMINAL_MS = [5_000, 10_000, 20_000, 40_000, 60_000];

/** Nominal wait before retry number `step` (0 is the first): 5, 10, 20, 40, then 60 s for good. */
export function retryNominalMs(step: number): number {
  const i = Math.min(Math.max(0, Math.floor(step)), RETRY_NOMINAL_MS.length - 1);
  return RETRY_NOMINAL_MS[i];
}

/**
 * The wait before retry number `step`, `random` in [0, 1): between half and
 * all of the nominal wait, so tabs that went unsettled together do not retry
 * together.
 */
export function nextRetryDelay(step: number, random: number): number {
  return jitter(retryNominalMs(step), random);
}

const RECONNECT_BASE_MS = 3_000;
const RECONNECT_MAX_MS = 60_000;
const RECONNECT_RESET_MS = 60_000;

/**
 * Nominal wait before the next reconnect: 3 s, doubling up to 60 s, and 3 s
 * again after a connection that lasted a minute. `previousNominalMs` is the
 * nominal wait before the last attempt (null if none), not the jittered one:
 * doubling a jittered wait would drift.
 */
export function reconnectNominalMs(
  previousNominalMs: number | null,
  lastConnectionMs: number,
): number {
  if (previousNominalMs === null || lastConnectionMs >= RECONNECT_RESET_MS) return RECONNECT_BASE_MS;
  return Math.min(previousNominalMs * 2, RECONNECT_MAX_MS);
}

/**
 * The next reconnect, with both its nominal wait (to pass back as
 * `previousNominalMs` next time) and the wait to use, `random` in [0, 1).
 * A link that keeps dropping then costs at most one reload a minute, and the
 * tabs of a restarted sniffer do not all reconnect at the same instant.
 */
export function nextReconnect(
  previousNominalMs: number | null,
  lastConnectionMs: number,
  random: number,
): { nominalMs: number; delayMs: number } {
  const nominalMs = reconnectNominalMs(previousNominalMs, lastConnectionMs);
  return { nominalMs, delayMs: jitter(nominalMs, random) };
}

/** `nextReconnect(...).delayMs`: between half and all of the nominal wait. */
export function nextReconnectDelay(
  previousNominalMs: number | null,
  lastConnectionMs: number,
  random: number,
): number {
  return nextReconnect(previousNominalMs, lastConnectionMs, random).delayMs;
}

// ---------------------------------------------------------------------------
// Request state machine
// ---------------------------------------------------------------------------

/**
 * Why a load was sent. A user load follows something the person did (mount
 * without a socket, a filter, typing, a label change, a resume after the pause
 * buffer overflowed) and always replaces the rows. A catch-up load is the
 * WebUI's own (a socket opening, a retry) and, while paused, is held aside so
 * a frozen table does not change.
 */
export type LoadCause = 'user' | 'catchUp';

/** One load: what was asked, and what to do with the answer. */
export interface Load {
  /** Increases with every load; an answer from an older one is stale. */
  generation: number;
  cause: LoadCause;
  /** Sent by the retry series, so a further unsettled answer advances it. */
  retry: boolean;
  /**
   * The filters the answer is for. The browser applies these (including
   * hiding malformed frames) to the rows, and checks the live records it
   * carries over against them.
   */
  filter: HistoryFilter;
  /** Malformed frames are requested although `filter` hides them. */
  includeMalformed: boolean;
  /** The query string to send. */
  query: string;
}

/** A catch-up load that completed while paused. */
export interface HeldLoad {
  load: Load;
  settled: boolean;
}

export interface RequestState {
  /** The last generation issued. */
  generation: number;
  /** The filters as they are now, typed text included. */
  filter: FilterState;
  /** The load whose answer is awaited. */
  inFlight: Load | null;
  /** A catch-up load that completed while paused, waiting for resume. */
  held: HeldLoad | null;
  paused: boolean;
  /** The 400 ms typing timer is pending. */
  typingTimer: boolean;
  /** The retry timer is pending. */
  retryTimer: boolean;
  /** The timer for the load without a socket is pending. */
  mountTimer: boolean;
  /** The live connection has opened at least once. */
  socketSeen: boolean;
  /**
   * The retry series: null when none runs, otherwise the number of the retry
   * last scheduled (0 is the first, after 5 s). While it runs the "database
   * is behind" notice is shown.
   */
  retryStep: number | null;
  /** The database cannot filter: later loads ask for malformed frames too, until the next socket open. */
  noWalInclude: boolean;
  /** The error to show, or null. Kept until a current answer arrives. */
  error: string | null;
}

export type TimerName = 'typing' | 'retry' | 'mount';

export type HistoryEvent =
  /** Mounted: starts the timer for the load without a socket. */
  | { kind: 'mount' }
  /** That timer fired. */
  | { kind: 'mountTimer' }
  /** The range, type, status, gateway or malformed flag changed. */
  | { kind: 'filter'; filter: FilterState }
  /** Only the device text changed. */
  | { kind: 'typed'; filter: FilterState }
  /** The typing timer fired. */
  | { kind: 'timer' }
  /** A label snapshot or change arrived. */
  | { kind: 'labels'; snapshot: boolean }
  /** The live connection opened. */
  | { kind: 'socketOpen' }
  /** The retry timer fired. */
  | { kind: 'retryTimer' }
  /** A load answered. `settled` is the `X-History-Settled` header (false only when it says so). */
  | { kind: 'response'; generation: number; settled: boolean }
  /**
   * A load failed. Transient: a network error, the load timeout, or `503`
   * with code "busy" (or a `500` whose body names a retryable code). Anything
   * else is permanent: `503` "no_wal", other `4xx`, a plain `500`.
   */
  | { kind: 'failed'; generation: number; transient: boolean; message?: string }
  /** A load got `503` with code "no_wal". */
  | { kind: 'noWal'; generation: number; message?: string }
  /**
   * A load got `401`: the session has ended. Not a failure of the load, so it
   * shows no error and joins no retry series; the page goes back to the login,
   * where the person starts again.
   */
  | { kind: 'unauthorized'; generation: number }
  | { kind: 'pause' }
  /** `overflowed`: the pause buffer passed `MAX_MESSAGES`. */
  | { kind: 'resume'; overflowed: boolean };

export type HistoryAction =
  /**
   * Request `load.query`. `resend`: the same load again (same generation and
   * cause) after a no-WAL answer. Aborts nothing: any earlier request has
   * already been aborted by an `abort` action or has answered.
   */
  | { kind: 'send'; load: Load; resend: boolean }
  /** Abort the request in flight. */
  | { kind: 'abort' }
  /** Start the typing timer (restart it if pending). */
  | { kind: 'startTimer'; timer: 'typing'; delayMs: number }
  /** Start the timer for the load without a socket. */
  | { kind: 'startTimer'; timer: 'mount'; delayMs: number }
  /** Start the retry timer for retry number `step`; its wait is `nextRetryDelay(step, Math.random())`. */
  | { kind: 'startTimer'; timer: 'retry'; step: number }
  | { kind: 'cancelTimer'; timer: TimerName }
  /** Replace the rows with this answer (`mergeAfterLoad`). */
  | { kind: 'apply'; load: Load; settled: boolean }
  /** Keep this answer aside until resume. At most one is kept. */
  | { kind: 'hold'; load: Load; settled: boolean }
  /** Apply the held answer, as if it had just arrived. */
  | { kind: 'applyHeld'; load: Load; settled: boolean }
  /** Merge the pause buffer into the rows (`mergeRecords`, rows that match win). */
  | { kind: 'mergeBuffer' }
  /** A stale answer: change nothing, rows and counts alike. */
  | { kind: 'ignore' };

export interface Step {
  state: RequestState;
  actions: HistoryAction[];
}

/**
 * What a load that did not return 200 means. `unauthorized` (401) is the
 * session ending: repeating the request cannot help and an error strip would
 * only sit behind the login page. `noWal` and `busy` are the two answers the
 * database gives on purpose (`503` with the code "no_wal" or "busy"); any
 * other status is a plain `failed`.
 */
export type FailedLoad = 'unauthorized' | 'noWal' | 'busy' | 'failed';

/**
 * Classifies a failed load from its HTTP status and the machine-readable
 * `code` of its JSON body (null when it had none). The status decides alone
 * for a 401, whatever the body says.
 */
export function classifyFailedLoad(status: number, code: string | null): FailedLoad {
  if (status === 401) return 'unauthorized';
  if (status === 503 && code === 'no_wal') return 'noWal';
  if (status === 503 && code === 'busy') return 'busy';
  return 'failed';
}

/** The state for a page that has just mounted with `filter`. */
export function initialRequestState(filter: FilterState): RequestState {
  return {
    generation: 0,
    filter,
    inFlight: null,
    held: null,
    paused: false,
    typingTimer: false,
    retryTimer: false,
    mountTimer: false,
    socketSeen: false,
    retryStep: null,
    noWalInclude: false,
    error: null,
  };
}

/** A load is scheduled (typing timer) or in flight. */
export function isBusy(state: RequestState): boolean {
  return state.inFlight !== null || state.typingTimer;
}

/** The notice "The newest records may be missing: the database is behind. Retrying." is due. */
export function isBehind(state: RequestState): boolean {
  return state.retryStep !== null;
}

function sameList(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

/**
 * The event for a change of the filter bar, or null if nothing changed. Only
 * the device text is debounced; any other change loads at once.
 */
export function filterChangeEvent(prev: FilterState, next: FilterState): HistoryEvent | null {
  const otherChanged =
    !sameList(prev.mtype, next.mtype) ||
    prev.status !== next.status ||
    prev.gateway !== next.gateway ||
    prev.timePreset !== next.timePreset ||
    prev.showMalformed !== next.showMalformed;
  if (otherChanged) return { kind: 'filter', filter: next };
  if (prev.device !== next.device) return { kind: 'typed', filter: next };
  return null;
}

function unchanged(state: RequestState): Step {
  return { state, actions: [] };
}

/**
 * Sends a new load. Every load aborts the one in flight, cancels the timers
 * that would send another (so a stale timer can never fire across a load),
 * starts a new generation and discards a held answer.
 */
function startLoad(state: RequestState, cause: LoadCause, retry: boolean, nowMs: number): Step {
  const actions: HistoryAction[] = [];
  if (state.inFlight !== null) actions.push({ kind: 'abort' });
  if (state.typingTimer) actions.push({ kind: 'cancelTimer', timer: 'typing' });
  if (state.retryTimer) actions.push({ kind: 'cancelTimer', timer: 'retry' });
  if (state.mountTimer) actions.push({ kind: 'cancelTimer', timer: 'mount' });

  const generation = state.generation + 1;
  const filter = toHistoryFilter(state.filter, nowMs);
  const includeMalformed = state.noWalInclude && onlyMalformedFilter(filter);
  const load: Load = {
    generation,
    cause,
    retry,
    filter,
    includeMalformed,
    query: loadQuery(includeMalformed ? { ...filter, hideMalformed: false } : filter),
  };
  actions.push({ kind: 'send', load, resend: false });

  return {
    state: {
      ...state,
      generation,
      inFlight: load,
      held: null,
      typingTimer: false,
      retryTimer: false,
      mountTimer: false,
    },
    actions,
  };
}

/** The retry series after load `cur` ended without a settled answer. */
function advanceSeries(state: RequestState, cur: Load): { state: RequestState; action: HistoryAction } {
  const step = cur.retry && state.retryStep !== null ? state.retryStep + 1 : 0;
  return {
    state: { ...state, retryStep: step, retryTimer: true },
    action: { kind: 'startTimer', timer: 'retry', step },
  };
}

function fail(state: RequestState, cur: Load, transient: boolean, message: string | undefined): Step {
  const base: RequestState = {
    ...state,
    inFlight: null,
    error: message ?? 'The request failed',
  };
  // A catch-up load that failed for a passing reason left its hole unrepaired,
  // so it joins the retry series. A user load is not retried, and a permanent
  // failure would only repeat; either way the series stops and the error stays.
  if (cur.cause === 'catchUp' && transient) {
    const next = advanceSeries(base, cur);
    return { state: next.state, actions: [next.action] };
  }
  return { state: { ...base, retryStep: null }, actions: [] };
}

/**
 * The request rules. Given the state and an event, returns the new state and
 * what the hook must do. Pure: the hook owns the timers, `fetch`, the abort
 * controllers and the rows, and runs the actions in order.
 *
 * - One generation counter covers every load. An answer or failure of an
 *   older generation changes nothing.
 * - Any load aborts the one in flight, cancels pending timers and discards a
 *   held answer. A retry timer therefore never outlives the load that
 *   superseded it.
 * - A socket opening sends a catch-up load with the current filters (typed
 *   text included), whatever was in flight: that load's marker in the storage
 *   queue may predate the new subscription, so it cannot prove nothing was
 *   missed.
 * - A current answer with `settled: false`, or a transient failure of a
 *   catch-up load, starts or advances the retry series (nominal 5, 10, 20, 40,
 *   then 60 s, never giving up). A settled answer, or a failure that is not
 *   retried, ends it.
 * - An answer is routed by the pause state when it arrives: a user load
 *   replaces the rows even while paused, a catch-up load is held.
 * - A 401 (`unauthorized`) ends the load and the retry series without an
 *   error: the session is gone and the page is about to show the login.
 */
export function nextRequests(state: RequestState, event: HistoryEvent, nowMs: number): Step {
  switch (event.kind) {
    case 'mount':
      return {
        state: { ...state, mountTimer: true },
        actions: [{ kind: 'startTimer', timer: 'mount', delayMs: MOUNT_FALLBACK_MS }],
      };

    case 'mountTimer':
      // Cancelled by a socket opening or by any load sent first.
      if (!state.mountTimer) return unchanged(state);
      return startLoad({ ...state, mountTimer: false }, 'user', false, nowMs);

    case 'filter':
      return startLoad({ ...state, filter: event.filter }, 'user', false, nowMs);

    case 'typed':
      return {
        state: { ...state, filter: event.filter, typingTimer: true },
        actions: [{ kind: 'startTimer', timer: 'typing', delayMs: TYPING_DELAY_MS }],
      };

    case 'timer':
      if (!state.typingTimer) return unchanged(state);
      return startLoad({ ...state, typingTimer: false }, 'user', false, nowMs);

    case 'labels':
      // Labels are part of what the server matched, so a change can add or
      // remove rows. With no device text there is nothing to redo.
      if (state.filter.device === '') return unchanged(state);
      return {
        state: { ...state, typingTimer: true },
        actions: [{ kind: 'startTimer', timer: 'typing', delayMs: TYPING_DELAY_MS }],
      };

    case 'socketOpen':
      // A restarted server may have WAL, so the no-WAL fallback is forgotten.
      return startLoad({ ...state, socketSeen: true, noWalInclude: false }, 'catchUp', false, nowMs);

    case 'retryTimer':
      if (!state.retryTimer) return unchanged(state);
      return startLoad({ ...state, retryTimer: false }, 'catchUp', true, nowMs);

    case 'response': {
      const cur = state.inFlight;
      if (cur === null || cur.generation !== event.generation) {
        return { state, actions: [{ kind: 'ignore' }] };
      }
      let next: RequestState = { ...state, inFlight: null, error: null };
      const actions: HistoryAction[] = [];

      if (next.paused && cur.cause === 'catchUp') {
        next = { ...next, held: { load: cur, settled: event.settled } };
        actions.push({ kind: 'hold', load: cur, settled: event.settled });
      } else {
        actions.push({ kind: 'apply', load: cur, settled: event.settled });
      }

      if (event.settled) {
        next = { ...next, retryStep: null };
      } else {
        const series = advanceSeries(next, cur);
        next = series.state;
        actions.push(series.action);
      }
      return { state: next, actions };
    }

    case 'failed': {
      const cur = state.inFlight;
      if (cur === null || cur.generation !== event.generation) {
        return { state, actions: [{ kind: 'ignore' }] };
      }
      return fail(state, cur, event.transient, event.message);
    }

    case 'noWal': {
      const cur = state.inFlight;
      if (cur === null || cur.generation !== event.generation) {
        return { state, actions: [{ kind: 'ignore' }] };
      }
      // Only a load whose one row filter is hiding malformed frames can go
      // on without a database that filters: ask for the range with them and
      // hide them here. Anything else cannot be answered, and retrying would
      // get the same refusal.
      if (cur.includeMalformed || !onlyMalformedFilter(cur.filter)) {
        return fail(
          state,
          cur,
          false,
          event.message ?? 'History filters need the database in WAL mode; see the log',
        );
      }
      const load: Load = {
        ...cur,
        includeMalformed: true,
        query: loadQuery({ ...cur.filter, hideMalformed: false }),
      };
      return {
        state: { ...state, inFlight: load, noWalInclude: true },
        actions: [{ kind: 'send', load, resend: true }],
      };
    }

    case 'unauthorized': {
      const cur = state.inFlight;
      if (cur === null || cur.generation !== event.generation) {
        return { state, actions: [{ kind: 'ignore' }] };
      }
      // No error, and the retry series ends: a retry would be refused too.
      return { state: { ...state, inFlight: null, retryStep: null, error: null }, actions: [] };
    }

    case 'pause':
      return unchanged({ ...state, paused: true });

    case 'resume': {
      if (!state.paused) return unchanged(state);
      const resumed: RequestState = { ...state, paused: false };
      if (event.overflowed) {
        // Rows were lost from the buffer, so merging it would leave a hole.
        return startLoad(resumed, 'user', false, nowMs);
      }
      if (resumed.held !== null) {
        const { load, settled } = resumed.held;
        return {
          state: { ...resumed, held: null },
          actions: [{ kind: 'applyHeld', load, settled }],
        };
      }
      return { state: resumed, actions: [{ kind: 'mergeBuffer' }] };
    }
  }
}
