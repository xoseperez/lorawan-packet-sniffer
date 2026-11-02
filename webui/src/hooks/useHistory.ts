import { useEffect, useRef, useSyncExternalStore } from 'react';
import type { MessageRecord } from '../types/messages';
import type { FilterState } from '../components/FilterBar';
import {
  LOAD_TIMEOUT_MS,
  MAX_MESSAGES,
  classifyFailedLoad,
  compareTimestamps,
  countsAsMalformed,
  filterChangeEvent,
  initialRequestState,
  isBehind,
  isBusy,
  malformedAfterLoad,
  matchesFilter,
  mergeAfterLoad,
  mergeRecords,
  nextRequests,
  nextRetryDelay,
  olderAfterLoad,
  parseCountHeader,
  toHistoryFilter,
} from '../lib/history';
import { reportUnauthorized } from '../session';
import type {
  HistoryAction,
  HistoryEvent,
  HistoryFilter,
  Load,
  RequestState,
  TimerName,
} from '../lib/history';

// ---------------------------------------------------------------------------
// What the hook hands to the page
// ---------------------------------------------------------------------------

export interface HistoryView {
  /** The held rows, oldest first, at most MAX_MESSAGES. Not yet filtered for display. */
  messages: MessageRecord[];
  /** Matching records older than the oldest held row: the load's count less its rows, plus rows cut at MAX_MESSAGES. */
  older: number;
  /**
   * The server's count of malformed records that match the other filters, plus
   * those received live since that load was sent. Null when the server sent no
   * count (an older server, or a database without WAL), and before the first load.
   */
  malformed: number | null;
  /** Insert failures the sniffer has counted since it started, as the last response said. */
  writeFailures: number;
  /** A load is waiting for the typing pause or is in flight. */
  busy: boolean;
  /** The database is behind: loads came back unsettled and are being retried. */
  behind: boolean;
  /** Why the last load failed, or null. Cleared by the next answer. */
  error: string | null;
  /** A load has been applied, so `older` is known. */
  loaded: boolean;
  /** Every gateway seen this session, in loads and in raw live records, sorted. */
  knownGateways: string[];
  /** The device text the held rows were loaded for; null before the first load. */
  loadedDevice: string | null;
}

export interface UseHistory extends HistoryView {
  /** A record from `/ws`. */
  onRecord: (record: MessageRecord) => void;
  /** The live connection opened. */
  onSocketOpen: () => void;
  /** Freeze the table: live records go to the pause buffer. */
  pause: () => void;
  /** Unfreeze it: apply what was held or buffered, or reload if the buffer overflowed. */
  resume: () => void;
}

interface Payload {
  records: MessageRecord[];
  total: number | null;
  malformed: number | null;
}

// ---------------------------------------------------------------------------
// Reading answers
// ---------------------------------------------------------------------------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** The reason and machine-readable code of a failed answer: `{"error": "...", "code": "..."}`, else the status line. */
async function describeFailure(res: Response): Promise<{ message: string; code: string | null }> {
  const statusLine = `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ''}`;
  let text = '';
  try {
    text = (await res.text()).trim();
  } catch {
    return { message: statusLine, code: null };
  }
  try {
    const body: unknown = JSON.parse(text);
    if (isObject(body)) {
      const message = typeof body.error === 'string' && body.error !== '' ? body.error : statusLine;
      return { message, code: typeof body.code === 'string' ? body.code : null };
    }
  } catch {
    /* not JSON: a proxy's error page, or the plain-text refusal of the Host check */
  }
  const isPlainText = (res.headers.get('content-type') ?? '').startsWith('text/plain');
  if (isPlainText && text !== '' && text.length <= 300) return { message: `${statusLine}: ${text}`, code: null };
  return { message: statusLine, code: null };
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/** A 200 whose body is not a list of records. */
class UnexpectedAnswer extends Error {}

/** A request on the wire, with the reason it may have been cut short. */
interface Request {
  controller: AbortController;
  timer: ReturnType<typeof setTimeout>;
  /** Aborted because a newer load replaced it: not an error. */
  superseded: boolean;
  /** Aborted because it did not answer in time: a transient failure. */
  timedOut: boolean;
}

/**
 * Most live records kept for the load in flight (or held) to carry over. A
 * load lasts 30 s at most, but a held one waits for resume; past this many the
 * pause buffer has overflowed too, and resume reloads everything instead.
 */
const SINCE_SENT_CAP = 2 * MAX_MESSAGES;

/**
 * Everything the history hook keeps outside React: the rows, the live records,
 * the pause buffer, the timers and the requests. `nextRequests` decides what
 * happens; this class runs its actions and does the bookkeeping the pure
 * functions need as input.
 *
 * It lives outside React state because a record, a timer and an answer can all
 * arrive between two renders, and each must see the effect of the one before.
 * React reads it through `useSyncExternalStore`.
 */
class HistoryEngine {
  private state: RequestState;
  private labels: Record<string, string> = {};

  // The rows.
  private rows: MessageRecord[] = [];
  private ids = new Set<string>();
  /** Rows that arrived live and have not been replaced by a fetched copy. */
  private liveRows = new WeakSet<MessageRecord>();
  private older = 0;
  private loadedDevice: string | null = null;

  // Counts that come with a load.
  private lastLoadFilter: HistoryFilter | null = null;
  private malformedHeader: number | null = null;
  private liveMalformed = 0;
  private writeFailures = 0;
  private gateways = new Set<string>();
  private gatewayList: string[] = [];

  // Live records while a load is out or held, and while paused.
  /**
   * Every raw live record received since the load in flight (or the held one)
   * was sent, including those the filters rejected on arrival: the malformed
   * count needs them. Null when no load is waiting to be applied. A resend
   * keeps the list, since it is the same load.
   */
  private sinceSent: MessageRecord[] | null = null;
  private buffer: MessageRecord[] = [];
  private overflowed = false;
  /** The answer of the response being dispatched. */
  private payload: Payload | null = null;
  /** The answer of the catch-up load held until resume. */
  private heldPayload: Payload | null = null;

  private timers: Record<TimerName, ReturnType<typeof setTimeout> | null> = {
    typing: null,
    retry: null,
    mount: null,
  };
  private request: Request | null = null;

  private listeners = new Set<() => void>();
  private view: HistoryView;

  constructor(filter: FilterState, labels: Record<string, string>) {
    this.state = initialRequestState(filter);
    this.labels = labels;
    this.view = this.buildView();
  }

  // --- React side -----------------------------------------------------------

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): HistoryView => this.view;

  setLabels(labels: Record<string, string>) {
    this.labels = labels;
  }

  /** Starts the timer for the best-effort load when no socket opens. */
  start() {
    this.dispatch({ kind: 'mount' });
  }

  /** Cancels every timer and request. The engine can be started again. */
  stop() {
    for (const name of ['typing', 'retry', 'mount'] as const) this.clearTimer(name);
    this.abortRequest();
    this.state = initialRequestState(this.state.filter);
    this.sinceSent = null;
    this.heldPayload = null;
    this.payload = null;
    this.buffer = [];
    this.overflowed = false;
  }

  // --- events -------------------------------------------------------------------

  dispatch = (event: HistoryEvent) => {
    const step = nextRequests(this.state, event, Date.now());
    this.state = step.state;
    for (const action of step.actions) this.run(action);
    if (this.state.held === null) this.heldPayload = null;
    if (this.state.inFlight === null && this.state.held === null) this.sinceSent = null;
    this.commit();
  };

  onSocketOpen = () => this.dispatch({ kind: 'socketOpen' });

  pause = () => this.dispatch({ kind: 'pause' });

  resume = () => this.dispatch({ kind: 'resume', overflowed: this.overflowed });

  onRecord = (r: MessageRecord) => {
    if (r.gateway_eui && !this.gateways.has(r.gateway_eui)) {
      this.gateways.add(r.gateway_eui);
      this.gatewayList = [...this.gateways].sort();
    }

    if (this.sinceSent !== null) {
      this.sinceSent.push(r);
      if (this.sinceSent.length > SINCE_SENT_CAP) this.sinceSent = this.sinceSent.slice(-MAX_MESSAGES);
    }
    if (this.lastLoadFilter !== null && countsAsMalformed(r, this.lastLoadFilter, this.labels)) {
      this.liveMalformed++;
    }

    if (this.state.paused) {
      // Raw, like the list above: the filters may change before resume, and
      // the buffer is filtered then. Past the cap the rest is not kept, since
      // an overflowed buffer is replaced by a full load on resume.
      if (this.buffer.length < MAX_MESSAGES) this.buffer.push(r);
      else this.overflowed = true;
      this.commit();
      return;
    }

    if (matchesFilter(r, toHistoryFilter(this.state.filter, Date.now()), this.labels)) this.addLive(r);
    this.commit();
  };

  // --- rows -------------------------------------------------------------------

  /** Adds a live record that matches: once per id, in time order, cut at MAX_MESSAGES. */
  private addLive(r: MessageRecord) {
    // The copy already held wins, as when a fetched copy meets a live one.
    if (this.ids.has(r.id)) return;

    const last = this.rows[this.rows.length - 1];
    if (last === undefined || compareTimestamps(last.timestamp, r.timestamp) <= 0) {
      // The usual case: the newest record so far.
      let rows = this.rows.concat(r);
      this.ids.add(r.id);
      if (rows.length > MAX_MESSAGES) {
        const cut = rows.length - MAX_MESSAGES;
        for (let i = 0; i < cut; i++) this.ids.delete(rows[i].id);
        rows = rows.slice(cut);
        this.older += cut;
      }
      this.rows = rows;
    } else {
      // Stamped before a record already held: two gateway tasks can broadcast
      // in the opposite order to their stamps.
      const merged = mergeRecords(this.rows, [r], MAX_MESSAGES);
      this.rows = merged.records;
      this.older += merged.dropped;
      this.ids = new Set(this.rows.map(x => x.id));
    }
    this.liveRows.add(r);
  }

  /**
   * Replaces the rows with a load's answer, plus the live records it lacks
   * (see `mergeAfterLoad`).
   */
  private applyLoad(load: Load, settled: boolean, payload: Payload) {
    const sinceSent = this.sinceSent ?? [];
    // Only an unsettled answer keeps live rows received before the load was
    // sent; a settled one has them in its snapshot.
    const held = settled
      ? []
      : [...this.rows.filter(r => this.liveRows.has(r)), ...this.buffer];

    const merged = mergeAfterLoad(
      payload.records,
      settled,
      { sinceSent, held },
      load.filter,
      this.labels,
      MAX_MESSAGES,
    );
    this.rows = merged.records;
    this.ids = new Set(this.rows.map(r => r.id));
    this.older = olderAfterLoad(payload.total, payload.records.length, merged.dropped);

    const fetched = new Set(payload.records.map(r => r.id));
    this.liveRows = new WeakSet();
    for (const r of this.rows) if (!fetched.has(r.id)) this.liveRows.add(r);

    this.lastLoadFilter = load.filter;
    this.malformedHeader = payload.malformed;
    this.liveMalformed = sinceSent.filter(r => countsAsMalformed(r, load.filter, this.labels)).length;
    this.loadedDevice = load.filter.device;

    let grew = false;
    for (const r of payload.records) {
      if (r.gateway_eui && !this.gateways.has(r.gateway_eui)) {
        this.gateways.add(r.gateway_eui);
        grew = true;
      }
    }
    if (grew) this.gatewayList = [...this.gateways].sort();
  }

  /** Resume with no held load: the buffered records that match join the rows. */
  private mergeBuffer() {
    const f = toHistoryFilter(this.state.filter, Date.now());
    const keep = this.buffer.filter(r => matchesFilter(r, f, this.labels));
    const before = this.ids;
    const merged = mergeRecords(this.rows, keep, MAX_MESSAGES);
    this.rows = merged.records;
    this.ids = new Set(this.rows.map(r => r.id));
    this.older += merged.dropped;
    for (const r of keep) if (!before.has(r.id)) this.liveRows.add(r);
    this.buffer = [];
    this.overflowed = false;
  }

  // --- actions ------------------------------------------------------------------

  private run(a: HistoryAction) {
    switch (a.kind) {
      case 'send':
        this.send(a.load, a.resend);
        break;
      case 'abort':
        this.abortRequest();
        break;
      case 'startTimer':
        this.startTimer(a);
        break;
      case 'cancelTimer':
        this.clearTimer(a.timer);
        break;
      case 'apply':
        if (this.payload !== null) this.applyLoad(a.load, a.settled, this.payload);
        break;
      case 'hold':
        this.heldPayload = this.payload;
        break;
      case 'applyHeld':
        if (this.heldPayload !== null) this.applyLoad(a.load, a.settled, this.heldPayload);
        this.heldPayload = null;
        this.buffer = [];
        this.overflowed = false;
        break;
      case 'mergeBuffer':
        this.mergeBuffer();
        break;
      case 'ignore':
        break;
    }
  }

  private startTimer(a: Extract<HistoryAction, { kind: 'startTimer' }>) {
    this.clearTimer(a.timer);
    let delayMs: number;
    let event: HistoryEvent;
    switch (a.timer) {
      case 'typing':
        delayMs = a.delayMs;
        event = { kind: 'timer' };
        break;
      case 'mount':
        delayMs = a.delayMs;
        event = { kind: 'mountTimer' };
        break;
      case 'retry':
        delayMs = nextRetryDelay(a.step, Math.random());
        event = { kind: 'retryTimer' };
        break;
    }
    const name = a.timer;
    this.timers[name] = setTimeout(() => {
      this.timers[name] = null;
      this.dispatch(event);
    }, delayMs);
  }

  private clearTimer(name: TimerName) {
    const t = this.timers[name];
    if (t !== null) clearTimeout(t);
    this.timers[name] = null;
  }

  // --- requests -----------------------------------------------------------------

  private abortRequest() {
    const req = this.request;
    if (req === null) return;
    req.superseded = true;
    clearTimeout(req.timer);
    req.controller.abort();
    this.request = null;
  }

  private send(load: Load, resend: boolean) {
    if (!resend) {
      // The position of the load among the live records is where it was first
      // sent; a resend after a no-WAL answer is the same load.
      this.sinceSent = [];
      // A buffer that overflowed lost records, so the reload on resume stands
      // in for it. It is dropped now, and what arrives from here on goes to
      // the rows and to the list above.
      if (load.cause === 'user' && this.overflowed && !this.state.paused) {
        this.buffer = [];
        this.overflowed = false;
      }
    }

    const controller = new AbortController();
    const req: Request = {
      controller,
      superseded: false,
      timedOut: false,
      timer: setTimeout(() => {
        req.timedOut = true;
        controller.abort();
      }, LOAD_TIMEOUT_MS),
    };
    this.request = req;
    void this.fetchLoad(load, req);
  }

  private async fetchLoad(load: Load, req: Request) {
    const generation = load.generation;
    let event: HistoryEvent | null = null;
    let payload: Payload | null = null;

    try {
      const res = await fetch(`/api/messages?${load.query}`, {
        signal: req.controller.signal,
        cache: 'no-store',
      });

      // A server-wide counter: every answer updates it, a stale one too. An
      // answer without the header (a proxy's error page) says nothing, so the
      // last known count stays.
      const failures = parseCountHeader(res.headers.get('X-Storage-Write-Failures'));
      if (failures !== null && failures !== this.writeFailures) {
        this.writeFailures = failures;
        this.commit();
      }

      if (this.state.inFlight?.generation !== generation) {
        // Overtaken: nothing to read. Release the body instead of leaving it
        // buffered.
        void res.body?.cancel().catch(() => undefined);
        return;
      }

      if (res.ok) {
        const body: unknown = await res.json();
        if (!Array.isArray(body)) throw new UnexpectedAnswer();
        payload = {
          records: body as MessageRecord[],
          total: parseCountHeader(res.headers.get('X-Total-Count')),
          malformed: parseCountHeader(res.headers.get('X-Malformed-Count')),
        };
        event = {
          kind: 'response',
          generation,
          settled: res.headers.get('X-History-Settled') !== 'false',
        };
      } else {
        const { message, code } = await describeFailure(res);
        switch (classifyFailedLoad(res.status, code)) {
          case 'unauthorized':
            // The session ended. No error strip and no retry: the page goes
            // back to the login, and this hook stops with it.
            event = { kind: 'unauthorized', generation };
            reportUnauthorized();
            break;
          case 'noWal':
            event = { kind: 'noWal', generation, message };
            break;
          case 'busy':
            // A busy reader is worth another try.
            event = { kind: 'failed', generation, transient: true, message };
            break;
          case 'failed':
            // Anything else would repeat.
            event = { kind: 'failed', generation, transient: false, message };
            break;
        }
      }
    } catch (e) {
      if (req.superseded) return; // aborted on purpose, by a newer load
      let message: string;
      let transient = true;
      if (req.timedOut) {
        message = `the sniffer did not answer within ${Math.round(LOAD_TIMEOUT_MS / 1000)} s`;
      } else if (e instanceof UnexpectedAnswer || e instanceof SyntaxError) {
        // Not JSON, or not a list: a proxy's page or a mismatched server.
        // Asking again would get the same answer.
        message = 'unexpected answer from the sniffer';
        transient = false;
      } else {
        message = `could not reach the sniffer${e instanceof Error && e.message ? ` (${e.message})` : ''}`;
      }
      event = { kind: 'failed', generation, transient, message };
    } finally {
      clearTimeout(req.timer);
      if (this.request === req) this.request = null;
    }

    if (event === null) return;
    this.payload = payload;
    try {
      this.dispatch(event);
    } finally {
      this.payload = null;
    }
  }

  // --- the view -----------------------------------------------------------------

  private buildView(): HistoryView {
    const s = this.state;
    return {
      messages: this.rows,
      older: this.older,
      malformed:
        this.lastLoadFilter === null
          ? null
          : malformedAfterLoad(this.malformedHeader, this.liveMalformed),
      writeFailures: this.writeFailures,
      busy: isBusy(s),
      behind: isBehind(s),
      error: s.error,
      loaded: this.lastLoadFilter !== null,
      knownGateways: this.gatewayList,
      loadedDevice: this.loadedDevice,
    };
  }

  /** Publishes a new view if anything in it changed. */
  private commit() {
    const next = this.buildView();
    const prev = this.view;
    let same = true;
    for (const key of Object.keys(next) as (keyof HistoryView)[]) {
      if (next[key] !== prev[key]) {
        same = false;
        break;
      }
    }
    if (same) return;
    this.view = next;
    for (const listener of this.listeners) listener();
  }
}

// ---------------------------------------------------------------------------
// The hook
// ---------------------------------------------------------------------------

export interface UseHistoryInput {
  filters: FilterState;
  labels: Record<string, string>;
  /** The first `/ws/labels` snapshot has arrived. */
  labelsSynced: boolean;
}

/**
 * The table's rows and the counts that come with them. The server decides what
 * a time range and a set of filters contain: every page load, range change,
 * settled filter change and socket open loads the newest matching records, and
 * the live feed adds to them. See `nextRequests` for the rules.
 */
export function useHistory({ filters, labels, labelsSynced }: UseHistoryInput): UseHistory {
  const ref = useRef<HistoryEngine | null>(null);
  if (ref.current === null) ref.current = new HistoryEngine(filters, labels);
  const engine = ref.current;

  const view = useSyncExternalStore(engine.subscribe, engine.getSnapshot);

  useEffect(() => {
    engine.start();
    return () => engine.stop();
  }, [engine]);

  // A change of the filter bar. Only the device text waits for a typing pause.
  const prevFilters = useRef(filters);
  useEffect(() => {
    const prev = prevFilters.current;
    prevFilters.current = filters;
    const event = filterChangeEvent(prev, filters);
    if (event !== null) engine.dispatch(event);
  }, [engine, filters]);

  // A label change can add rows to the match or take them away, and the first
  // snapshot counts: until it arrives the browser cannot tell which rows the
  // server matched by label.
  const prevLabels = useRef(labels);
  const prevSynced = useRef(labelsSynced);
  useEffect(() => {
    engine.setLabels(labels);
    const changed = prevLabels.current !== labels;
    const snapshot = labelsSynced && !prevSynced.current;
    prevLabels.current = labels;
    prevSynced.current = labelsSynced;
    if (changed) engine.dispatch({ kind: 'labels', snapshot });
  }, [engine, labels, labelsSynced]);

  return {
    ...view,
    onRecord: engine.onRecord,
    onSocketOpen: engine.onSocketOpen,
    pause: engine.pause,
    resume: engine.resume,
  };
}
