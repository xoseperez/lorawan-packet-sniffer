import { TableVirtuoso, TableComponents } from 'react-virtuoso';
import { MessageRecord, MType } from '../types/messages';
import { ColumnConfig } from './ColumnPicker';

type SortDir = 'asc' | 'desc';

interface Props {
  messages: MessageRecord[];
  columns: ColumnConfig;
  sortDir: SortDir;
  onToggleSort: () => void;
  isLive: boolean;
}

function formatTime(ts: string): string {
  const d = new Date(ts);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  const ms = String(d.getMilliseconds()).padStart(3, '0');
  return `${hh}:${mm}:${ss}.${ms}`;
}

const MTYPE_ABBREV: Record<MType, string> = {
  JoinRequest:         'JReq',
  JoinAccept:          'JAcc',
  UnconfirmedDataUp:   'UncUp',
  UnconfirmedDataDown: 'UncDn',
  ConfirmedDataUp:     'ConUp',
  ConfirmedDataDown:   'ConDn',
  RejoinRequest:       'RJoin',
  Proprietary:         'Prop',
  Unknown:             'Unkn',
};

// Maps each MType to a CSS badge category class
const MTYPE_CAT: Record<MType, string> = {
  JoinRequest:         'join',
  JoinAccept:          'join',
  RejoinRequest:       'join',
  UnconfirmedDataUp:   'up',
  ConfirmedDataUp:     'up',
  UnconfirmedDataDown: 'down',
  ConfirmedDataDown:   'down',
  Proprietary:         'other',
  Unknown:             'other',
};

function rssiClass(rssi: number | null): string {
  if (rssi === null) return '';
  if (rssi >= -100) return 'signal-good';
  if (rssi >= -110) return 'signal-fair';
  return 'signal-poor';
}

// Minimum decodable SNR per spreading factor (Semtech LoRa spec)
const SF_MIN_SNR: Record<string, number> = {
  SF7:  -7.5,
  SF8:  -10.0,
  SF9:  -12.5,
  SF10: -15.0,
  SF11: -17.5,
  SF12: -20.0,
};

function snrClass(snr: number | null, datarate: string): string {
  if (snr === null) return '';
  const sf = datarate.match(/^(SF\d+)/i)?.[1]?.toUpperCase();
  const floor = sf !== undefined ? (SF_MIN_SNR[sf] ?? -7.5) : -7.5;
  if (snr >= floor + 10) return 'signal-good';
  if (snr >= floor + 3)  return 'signal-fair';
  return 'signal-poor';
}

function makeComponents(columns: ColumnConfig): TableComponents<MessageRecord> {
  return {
    TableRow: ({ item, ...props }) => (
      <tr
        {...props}
        className={item.status === 'Blocked' ? 'row-blocked' : undefined}
      />
    ),
    Table: ({ style, ...props }) => (
      <table {...props} style={{ ...style, tableLayout: 'fixed', width: '100%' }} />
    ),
  };
  void columns;
}

export function MessageTable({ messages, columns, sortDir, onToggleSort, isLive }: Props) {
  const components = makeComponents(columns);

  return (
    <TableVirtuoso<MessageRecord>
      data={messages}
      style={{ height: '100%' }}
      followOutput={isLive ? 'smooth' : false}
      components={components}
      fixedHeaderContent={() => (
        <tr>
          {columns.time && (
            <th className="col-time sortable" onClick={onToggleSort}>
              Time {sortDir === 'desc' ? '▼' : '▲'}
            </th>
          )}
          {columns.dir     && <th className="col-dir">Dir</th>}
          {columns.mtype   && <th className="col-mtype">Type</th>}
          {columns.device  && <th className="col-device">Device</th>}
          {columns.fcnt    && <th className="col-fcnt">FCnt</th>}
          {columns.fport   && <th className="col-fport">FP</th>}
          {columns.freq    && <th className="col-freq">Freq</th>}
          {columns.datarate && <th className="col-dr">DR</th>}
          {columns.rssi    && <th className="col-rssi">RSSI</th>}
          {columns.snr     && <th className="col-snr">SNR</th>}
          {columns.size    && <th className="col-size">Size</th>}
          {columns.toa     && <th className="col-toa">ToA</th>}
          {columns.status  && <th className="col-status">S</th>}
        </tr>
      )}
      itemContent={(_, msg) => <RowCells msg={msg} columns={columns} />}
    />
  );
}

function RowCells({ msg, columns }: { msg: MessageRecord; columns: ColumnConfig }) {
  const device    = msg.dev_addr ?? msg.dev_eui ?? '';
  const isBlocked = msg.status === 'Blocked';
  const mcat      = MTYPE_CAT[msg.mtype] ?? 'other';

  return (
    <>
      {columns.time && (
        <td className="col-time mono">{formatTime(msg.timestamp)}</td>
      )}
      {columns.dir && (
        <td className={`col-dir ${msg.direction === 'up' ? 'dir-up' : 'dir-down'}`}>
          {msg.direction === 'up' ? '▲' : '▼'}
        </td>
      )}
      {columns.mtype && (
        <td className="col-mtype">
          <span className={`mtype-badge mtype-${mcat}`}>
            {MTYPE_ABBREV[msg.mtype] ?? msg.mtype}
          </span>
        </td>
      )}
      {columns.device  && <td className="col-device mono">{device}</td>}
      {columns.fcnt    && <td className="col-fcnt mono">{msg.fcnt ?? ''}</td>}
      {columns.fport   && <td className="col-fport mono">{msg.fport ?? ''}</td>}
      {columns.freq    && <td className="col-freq mono">{msg.freq_mhz.toFixed(3)}</td>}
      {columns.datarate && <td className="col-dr mono">{msg.datarate}</td>}
      {columns.rssi && (
        <td className={`col-rssi mono ${rssiClass(msg.rssi)}`}>
          {msg.rssi ?? ''}
        </td>
      )}
      {columns.snr && (
        <td className={`col-snr mono ${snrClass(msg.snr, msg.datarate)}`}>
          {msg.snr !== null ? msg.snr.toFixed(1) : ''}
        </td>
      )}
      {columns.size && <td className="col-size mono">{msg.payload_size}</td>}
      {columns.toa && (
        <td className="col-toa mono">
          {msg.toa_ms !== null ? `${msg.toa_ms.toFixed(0)}ms` : ''}
        </td>
      )}
      {columns.status && (
        <td className="col-status" title={msg.status}>
          <span className={`status-dot status-dot-${isBlocked ? 'blocked' : 'forwarded'}`} />
        </td>
      )}
    </>
  );
}
