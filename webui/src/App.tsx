import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { MessageRecord } from './types/messages';
import { useWebSocket } from './hooks/useWebSocket';
import { MessageTable } from './components/MessageTable';
import { FilterBar, FilterState, DEFAULT_FILTERS } from './components/FilterBar';
import { ColumnPicker, ColumnConfig, DEFAULT_COLUMNS } from './components/ColumnPicker';
import { StatusBar } from './components/StatusBar';

const MAX_MESSAGES = 10_000;

// ---------------------------------------------------------------------------
// localStorage hook
// ---------------------------------------------------------------------------

function useLocalStorage<T>(key: string, initial: T): [T, (v: T) => void] {
  const [stored, setStored] = useState<T>(() => {
    try {
      const item = localStorage.getItem(key);
      return item !== null ? (JSON.parse(item) as T) : initial;
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
// Time preset → milliseconds
// ---------------------------------------------------------------------------

const TIME_PRESET_MS: Record<string, number> = {
  '5m':  5  * 60 * 1000,
  '15m': 15 * 60 * 1000,
  '1h':  60 * 60 * 1000,
  '24h': 24 * 60 * 60 * 1000,
};

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

export function App() {
  const [messages, setMessages] = useState<MessageRecord[]>([]);
  const [isLive, setIsLive] = useState(true);
  const pauseBuffer = useRef<MessageRecord[]>([]);
  const isLiveRef = useRef(true);

  const [filters, setFilters] = useLocalStorage<FilterState>('sniffer-filters', DEFAULT_FILTERS);
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc');
  const [columns, setColumns] = useLocalStorage<ColumnConfig>('sniffer-columns', DEFAULT_COLUMNS);

  // Keep isLiveRef in sync without triggering re-renders
  isLiveRef.current = isLive;

  // ------------------------------------------------------------------
  // WebSocket message handler (stable — reads from refs)
  // ------------------------------------------------------------------
  const handleMessage = useCallback((record: MessageRecord) => {
    if (!isLiveRef.current) {
      pauseBuffer.current.push(record);
    } else {
      setMessages(prev => {
        const next = [...prev, record];
        return next.length > MAX_MESSAGES ? next.slice(-MAX_MESSAGES) : next;
      });
    }
  }, []);

  const { status: wsStatus } = useWebSocket(handleMessage);

  // ------------------------------------------------------------------
  // Initial history load from REST API
  // ------------------------------------------------------------------
  useEffect(() => {
    fetch('/api/messages?limit=500')
      .then(r => {
        if (r.ok) return r.json() as Promise<MessageRecord[]>;
        throw new Error(`HTTP ${r.status}`);
      })
      .then(data => setMessages(data))
      .catch(() => {/* server may not be available yet */});
  }, []);

  // ------------------------------------------------------------------
  // Toggle live / paused
  // ------------------------------------------------------------------
  const handleToggleLive = useCallback(() => {
    setIsLive(prev => {
      if (!prev) {
        // Resuming: flush pause buffer
        const buffered = pauseBuffer.current.splice(0);
        if (buffered.length > 0) {
          setMessages(prev2 => {
            const next = [...prev2, ...buffered];
            return next.length > MAX_MESSAGES ? next.slice(-MAX_MESSAGES) : next;
          });
        }
      }
      return !prev;
    });
  }, []);

  // ------------------------------------------------------------------
  // Client-side filtering
  // ------------------------------------------------------------------
  const filteredMessages = useMemo(() => {
    let result = messages;

    if (filters.device) {
      const q = filters.device.toLowerCase();
      result = result.filter(
        m =>
          (m.dev_addr && m.dev_addr.toLowerCase().includes(q)) ||
          (m.dev_eui && m.dev_eui.toLowerCase().includes(q)),
      );
    }

    if (filters.mtype.length > 0) {
      result = result.filter(m => filters.mtype.includes(m.mtype));
    }

    if (filters.direction !== 'all') {
      result = result.filter(m => m.direction === filters.direction);
    }

    if (filters.status !== 'all') {
      result = result.filter(m => m.status === filters.status);
    }

    if (filters.timePreset !== 'all') {
      const ms = TIME_PRESET_MS[filters.timePreset];
      if (ms !== undefined) {
        const cutoff = Date.now() - ms;
        result = result.filter(m => new Date(m.timestamp).getTime() >= cutoff);
      }
    }

    if (sortDir === 'asc') {
      result = [...result].sort((a, b) => a.timestamp.localeCompare(b.timestamp));
    } else {
      // desc — messages arrive in ascending order; reverse in place is fast
      result = sortDir === 'desc' ? [...result].reverse() : result;
    }

    return result;
  }, [messages, filters, sortDir]);

  const blockedCount = useMemo(
    () => messages.filter(m => m.status === 'Blocked').length,
    [messages],
  );

  // ------------------------------------------------------------------
  // CSV export — serialises the current filtered view
  // ------------------------------------------------------------------
  const handleExportCsv = useCallback(() => {
    const HEADERS = [
      'timestamp', 'direction', 'status', 'mtype',
      'dev_addr', 'dev_eui', 'fcnt', 'fport',
      'freq_mhz', 'datarate', 'rssi', 'snr', 'crc_status',
      'payload_size', 'toa_ms', 'blocked_by', 'source_protocol',
    ];

    const escape = (v: string | number | null | undefined): string => {
      if (v === null || v === undefined) return '';
      const s = String(v);
      return s.includes(',') || s.includes('"') || s.includes('\n')
        ? `"${s.replace(/"/g, '""')}"`
        : s;
    };

    const rows = filteredMessages.map(m =>
      [
        m.timestamp, m.direction, m.status, m.mtype,
        m.dev_addr, m.dev_eui, m.fcnt, m.fport,
        m.freq_mhz, m.datarate, m.rssi, m.snr, m.crc_status,
        m.payload_size, m.toa_ms, m.blocked_by, m.source_protocol,
      ].map(escape).join(','),
    );

    const csv = [HEADERS.join(','), ...rows].join('\r\n');
    const blob = new Blob([csv], { type: 'text/csv;charset=utf-8;' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `lorawan-sniffer-${new Date().toISOString().replace(/[:.]/g, '-')}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  }, [filteredMessages]);

  return (
    <div className="app">
      <header className="app-header">
        <span className="app-title">
          <span className="app-title-primary">LoRaWAN</span>
          <span className="app-title-secondary"> Sniffer</span>
        </span>
        <FilterBar filters={filters} onChange={setFilters} />
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

      <main className="app-main">
        <MessageTable
          messages={filteredMessages}
          columns={columns}
          sortDir={sortDir}
          onToggleSort={() => setSortDir(d => (d === 'asc' ? 'desc' : 'asc'))}
          isLive={isLive}
        />
      </main>

      <footer className="app-footer">
        <StatusBar
          total={messages.length}
          shown={filteredMessages.length}
          blocked={blockedCount}
          wsStatus={wsStatus}
          isLive={isLive}
          onToggleLive={handleToggleLive}
        />
      </footer>
    </div>
  );
}
