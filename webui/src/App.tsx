import { useCallback, useEffect, useMemo, useState } from 'react';
import { MessageRecord, WsMessage, deviceId } from './types/messages';
import { useWebSocket } from './hooks/useWebSocket';
import { useLabels } from './hooks/useLabels';
import { useHistory } from './hooks/useHistory';
import { compareTimestamps, matchesFilter, matchingCount, toHistoryFilter } from './lib/history';
import { MessageTable } from './components/MessageTable';
import { LabelDialog } from './components/LabelDialog';
import { FilterBar, FilterState, DEFAULT_FILTERS } from './components/FilterBar';
import { ColumnPicker, ColumnConfig, DEFAULT_COLUMNS } from './components/ColumnPicker';
import { StatusBar } from './components/StatusBar';
import { HistoryNotice } from './components/HistoryNotice';

// How often the view re-checks which rows are still inside the time range, so
// rows leave on their own when no record arrives.
const AGING_TICK_MS = 5_000;

// ---------------------------------------------------------------------------
// localStorage hook
// ---------------------------------------------------------------------------

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function useLocalStorage<T>(key: string, initial: T): [T, (v: T) => void] {
  const [stored, setStored] = useState<T>(() => {
    try {
      const item = localStorage.getItem(key);
      if (item === null) return initial;
      const parsed: unknown = JSON.parse(item);
      // Settings saved by an older version lack the keys added since (new
      // columns, new filters). Lay the stored values over the defaults so
      // those keys still exist instead of staying undefined forever.
      if (isPlainObject(initial) && isPlainObject(parsed)) {
        return { ...initial, ...parsed } as T;
      }
      return parsed as T;
    } catch {
      return initial;
    }
  });

  const set = useCallback(
    (value: T) => {
      setStored(value);
      try {
        localStorage.setItem(key, JSON.stringify(value));
      } catch {
        /* storage full */
      }
    },
    [key],
  );

  return [stored, set];
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

export function App() {
  const [isLive, setIsLive] = useState(true);

  const [filters, setFilters] = useLocalStorage<FilterState>('sniffer-filters', DEFAULT_FILTERS);
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');
  const [columns, setColumns] = useLocalStorage<ColumnConfig>('sniffer-columns', DEFAULT_COLUMNS);

  // Labels come from their own WebSocket; the dialog edits one identifier at a time.
  const { labels, synced: labelsSynced, saveLabel, removeLabel } = useLabels();
  const [editing, setEditing] = useState<{ device: string } | null>(null);
  const handleEditLabel = useCallback((device: string) => setEditing({ device }), []);

  // The rows come from the server: it decides what the time range and the
  // filters contain, and the live feed adds to them.
  const history = useHistory({ filters, labels, labelsSynced });
  const { messages, onRecord, onSocketOpen, pause, resume } = history;

  // The message feed carries only 'message' events; anything else is ignored.
  const handleFrame = useCallback(
    (frame: WsMessage | null) => {
      if (frame?.type === 'message') onRecord(frame.data);
    },
    [onRecord],
  );

  const { status: wsStatus } = useWebSocket<WsMessage | null>('/ws', handleFrame, onSocketOpen);

  // A tick for the aging of rows, see AGING_TICK_MS.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setTick(t => t + 1), AGING_TICK_MS);
    return () => clearInterval(id);
  }, []);

  // ------------------------------------------------------------------
  // Toggle live / paused
  // ------------------------------------------------------------------
  const handleToggleLive = useCallback(() => {
    if (isLive) {
      pause();
      setIsLive(false);
    } else {
      resume();
      setIsLive(true);
    }
  }, [isLive, pause, resume]);

  // ------------------------------------------------------------------
  // What is shown: the held rows that pass the filters now
  // ------------------------------------------------------------------
  const { rows: filteredMessages, fromMs, blockedCount } = useMemo(() => {
    const f = toHistoryFilter(filters, Date.now());

    // Until the first label snapshot the browser cannot see the labels the
    // server matched on. Rows loaded for this very device text were matched
    // there, so they are not hidden for want of a label.
    const serverMatchedDevice =
      !labelsSynced && history.loadedDevice !== null && history.loadedDevice === filters.device;

    let result = messages.filter(m => matchesFilter(m, f, labels, { serverMatchedDevice }));

    if (sortDir === 'asc') {
      // The held rows are already in time order; sorting by the full
      // timestamp, not its text (".5Z" would sort after ".500001Z") and not
      // just to the millisecond, keeps that order whatever feeds them.
      result = [...result].sort((a, b) => compareTimestamps(a.timestamp, b.timestamp));
    } else {
      result = [...result].reverse();
    }

    let blockedCount = 0;
    for (const m of result) if (m.status === 'Blocked') blockedCount++;

    return { rows: result, fromMs: f.fromMs, blockedCount };
    // `tick` is a dependency on purpose: the range is counted back from now.
  }, [messages, filters, labels, labelsSynced, history.loadedDevice, sortDir, tick]);

  const matching = history.loaded
    ? matchingCount(filteredMessages.length, history.older, messages[0], fromMs)
    : null;

  // ------------------------------------------------------------------
  // CSV export — serialises the current filtered view
  // ------------------------------------------------------------------
  const handleExportCsv = useCallback(() => {
    const HEADERS = [
      'timestamp', 'direction', 'status', 'mtype',
      'dev_addr', 'dev_eui', 'fcnt', 'fport',
      'freq_mhz', 'datarate', 'rssi', 'snr', 'crc_status',
      'payload_size', 'toa_ms', 'blocked_by', 'source_protocol',
      'gateway_eui', 'join_eui', 'dev_nonce',
      'adr', 'adr_ack_req', 'ack', 'class_b', 'f_pending', 'f_opts_len',
      // New columns are added at the end, so tools that read the columns by
      // position keep working.
      'malformed', 'label',
    ];

    const escape = (v: string | number | boolean | null | undefined): string => {
      if (v === null || v === undefined) return '';
      const s = String(v);
      return s.includes(',') || s.includes('"') || s.includes('\n')
        ? `"${s.replace(/"/g, '""')}"`
        : s;
    };

    // The label is always quoted, so a comma or quote in it cannot shift the
    // columns. A spreadsheet treats a cell that starts with = + - or @ as a
    // formula, and a label is text typed by whoever can open the WebUI, so
    // those get a leading apostrophe to make the cell open as text. Only this
    // column is treated that way; the rest of the export is unchanged.
    const labelField = (m: MessageRecord): string => {
      const id = deviceId(m);
      const label = id !== null ? labels[id] : undefined;
      if (!label) return '';
      const text = /^[=+\-@]/.test(label) ? `'${label}` : label;
      return `"${text.replace(/"/g, '""')}"`;
    };

    const rows = filteredMessages.map(m =>
      [
        m.timestamp, m.direction, m.status, m.mtype,
        m.dev_addr, m.dev_eui, m.fcnt, m.fport,
        m.freq_mhz, m.datarate, m.rssi, m.snr, m.crc_status,
        m.payload_size, m.toa_ms, m.blocked_by, m.source_protocol,
        m.gateway_eui, m.join_eui, m.dev_nonce,
        m.adr, m.adr_ack_req, m.ack, m.class_b, m.f_pending, m.f_opts_len,
        m.malformed,
      ].map(escape).concat(labelField(m)).join(','),
    );

    const csv = [HEADERS.join(','), ...rows].join('\r\n');
    const blob = new Blob([csv], { type: 'text/csv;charset=utf-8;' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `lorawan-sniffer-${new Date().toISOString().replace(/[:.]/g, '-')}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  }, [filteredMessages, labels]);

  return (
    <div className="app">
      <header className="app-header">
        <span className="app-title">
          <span className="app-title-primary">LoRaWAN</span>
          <span className="app-title-secondary"> Sniffer</span>
        </span>
        <FilterBar filters={filters} onChange={setFilters} gateways={history.knownGateways} />
        <ColumnPicker columns={columns} onChange={setColumns} />
        <button
          className="export-btn"
          onClick={handleExportCsv}
          title={`Export ${filteredMessages.length} rows as CSV`}
          disabled={filteredMessages.length === 0}
        >
          ↓ CSV
        </button>
      </header>

      <main className="app-main" aria-busy={history.busy}>
        <MessageTable
          messages={filteredMessages}
          columns={columns}
          sortDir={sortDir}
          onToggleSort={() => setSortDir(d => (d === 'asc' ? 'desc' : 'asc'))}
          isLive={isLive}
          labels={labels}
          onEditLabel={handleEditLabel}
        />
      </main>

      <HistoryNotice
        error={history.error}
        hasRows={filteredMessages.length > 0}
        writeFailures={history.writeFailures}
        behind={history.behind}
        cap={
          matching !== null && matching > filteredMessages.length
            ? { shown: filteredMessages.length, matching, timePreset: filters.timePreset }
            : null
        }
      />

      <footer className="app-footer">
        <StatusBar
          matching={matching}
          busy={history.busy}
          shown={filteredMessages.length}
          blocked={blockedCount}
          malformed={history.malformed}
          showMalformed={filters.showMalformed}
          wsStatus={wsStatus}
          isLive={isLive}
          onToggleLive={handleToggleLive}
        />
      </footer>

      {editing !== null && (
        <LabelDialog
          key={editing.device}
          device={editing.device}
          current={labels[editing.device]}
          onSave={text => saveLabel(editing.device, text)}
          onDelete={() => removeLabel(editing.device)}
          onClose={() => setEditing(null)}
        />
      )}
    </div>
  );
}
