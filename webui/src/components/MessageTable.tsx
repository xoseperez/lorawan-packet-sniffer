import { useMemo } from 'react';
import { TableVirtuoso, TableComponents } from 'react-virtuoso';
import {
  MessageRecord, MType, Status, deviceId, malformedReason, malformedShort, shortEui,
} from '../types/messages';
import { ColumnConfig } from './ColumnPicker';

type SortDir = 'asc' | 'desc';

interface Props {
  messages: MessageRecord[];
  columns: ColumnConfig;
  sortDir: SortDir;
  onToggleSort: () => void;
  isLive: boolean;
  /** Label text by device identifier. */
  labels: Record<string, string>;
  /** Opens the label dialog for an identifier. */
  onEditLabel: (device: string) => void;
}

// Handed to react-virtuoso as `context`. Virtuoso only re-renders the visible
// rows when one of its own inputs changes, and it may keep the item renderer it
// was given earlier, so values that change over time (the labels) go through
// `context` instead of being captured by a closure.
interface TableContext {
  labels: Record<string, string>;
  onEditLabel: (device: string) => void;
}

/** Local date and time as `MM-DD HH:MM:SS.ddd`: the date shows which day a row belongs to once the range spans more than one. */
function formatTime(ts: string): string {
  const d = new Date(ts);
  const mo = String(d.getMonth() + 1).padStart(2, '0');
  const dd = String(d.getDate()).padStart(2, '0');
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  const ms = String(d.getMilliseconds()).padStart(3, '0');
  return `${mo}-${dd} ${hh}:${mm}:${ss}.${ms}`;
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

const STATUS_DOT: Record<Status, string> = {
  Forwarded: 'forwarded',
  Blocked:   'blocked',
  Captured:  'captured',
};

// FCtrl flags in the fixed order they appear in the Flags column. A letter
// means the flag is set, a dot means it is clear, and an empty slot means the
// flag does not exist for this frame (e.g. FPending on an uplink). Keeping an
// empty slot, rather than dropping it, keeps each letter in the same position
// on every row.
const FLAGS: { letter: string; field: 'adr' | 'adr_ack_req' | 'ack' | 'class_b' | 'f_pending' }[] = [
  { letter: 'A', field: 'adr' },
  { letter: 'R', field: 'adr_ack_req' },
  { letter: 'K', field: 'ack' },
  { letter: 'B', field: 'class_b' },
  { letter: 'P', field: 'f_pending' },
];

const FLAGS_LEGEND =
  'A = ADR, R = ADRACKReq, K = ACK, B = ClassB, P = FPending (· = clear, blank = not applicable)';

function FlagsCell({ msg }: { msg: MessageRecord }) {
  const hasAny = FLAGS.some(({ field }) => msg[field] != null);
  return (
    <td className="col-flags mono" title={hasAny ? FLAGS_LEGEND : undefined}>
      {FLAGS.map(({ letter, field }) => {
        const value = msg[field];
        return (
          <span key={letter} className={`flag ${value ? 'flag-on' : 'flag-off'}`}>
            {value == null ? '' : value ? letter : '·'}
          </span>
        );
      })}
    </td>
  );
}

// Defined once at module level. Virtuoso treats a new component function as a
// new component type, so rebuilding this object on every render would unmount
// and remount every row each time a message arrives, dropping keyboard focus
// from a label cell the user had just tabbed to.
//
// Virtuoso passes `context` to custom components. It is taken out here so it is
// not spread onto the DOM element.
const COMPONENTS: TableComponents<MessageRecord, TableContext> = {
  TableRow: ({ item, context: _context, ...props }) => {
    const classes: string[] = [];
    if (item.status === 'Blocked') classes.push('row-blocked');
    if (item.malformed != null) classes.push('row-malformed');
    // The record id lets a script read which records the table holds: rows
    // are in the DOM only while near the viewport, and the CSV export has no id.
    return (
      <tr
        {...props}
        data-id={item.id}
        className={classes.length > 0 ? classes.join(' ') : undefined}
      />
    );
  },
  Table: ({ style, context: _context, ...props }) => (
    <table {...props} style={{ ...style, tableLayout: 'fixed', width: '100%' }} />
  ),
};

export function MessageTable({
  messages, columns, sortDir, onToggleSort, isLive, labels, onEditLabel,
}: Props) {
  const context = useMemo<TableContext>(() => ({ labels, onEditLabel }), [labels, onEditLabel]);

  return (
    <TableVirtuoso<MessageRecord, TableContext>
      data={messages}
      context={context}
      style={{ height: '100%' }}
      // Follow new rows only when they arrive at the bottom (oldest first).
      // Newest first, they appear at the top on their own; following there
      // would scroll to the end, and since an empty table counts as "at the
      // end", the history loaded on start would open on its oldest rows.
      followOutput={isLive && sortDir === 'asc' ? 'smooth' : false}
      components={COMPONENTS}
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
          {columns.label   && <th className="col-label">Label</th>}
          {columns.gateway && <th className="col-gateway">Gateway</th>}
          {columns.joinEui && <th className="col-joineui">JoinEUI</th>}
          {columns.devNonce && <th className="col-devnonce" title="DevNonce">Nonce</th>}
          {columns.fcnt    && <th className="col-fcnt">FCnt</th>}
          {columns.fport   && <th className="col-fport">FP</th>}
          {columns.flags   && <th className="col-flags" title={FLAGS_LEGEND}>Flags</th>}
          {columns.fopts   && <th className="col-fopts">FOpts</th>}
          {columns.freq    && <th className="col-freq">Freq</th>}
          {columns.datarate && <th className="col-dr">DR</th>}
          {columns.rssi    && <th className="col-rssi">RSSI</th>}
          {columns.snr     && <th className="col-snr">SNR</th>}
          {columns.size    && <th className="col-size">Size</th>}
          {columns.toa     && <th className="col-toa">ToA</th>}
          {columns.status  && <th className="col-status">S</th>}
        </tr>
      )}
      itemContent={(_, msg, ctx) => <RowCells msg={msg} columns={columns} ctx={ctx} />}
    />
  );
}

// The label of the row's device. The whole cell is the button, so the touch
// target is as large as the cell, and Enter and Space work as on any button.
// Rows without an identifier (join accepts, rejoin requests, proprietary,
// truncated and malformed frames) get an empty, inert cell. A malformed frame
// has none because what would be read from it is noise, and a label on noise
// would stick to a device that does not exist. The label is rendered as text,
// never as HTML.
function LabelCell({ msg, ctx }: { msg: MessageRecord; ctx: TableContext }) {
  const device = deviceId(msg);
  if (device === null) return <td className="col-label" />;

  const label: string | undefined = ctx.labels[device];

  return (
    <td className="col-label">
      <button
        type="button"
        className={`label-cell ${label ? 'label-set' : 'label-empty'}`}
        title={label ?? 'Add a label'}
        aria-label={label ? `Label ${label} of ${device}, edit` : `Add a label for ${device}`}
        onClick={() => ctx.onEditLabel(device)}
      >
        {label ?? '+'}
      </button>
    </td>
  );
}

function RowCells({
  msg, columns, ctx,
}: { msg: MessageRecord; columns: ColumnConfig; ctx: TableContext }) {
  const device    = deviceId(msg) ?? '';
  const mcat      = MTYPE_CAT[msg.mtype] ?? 'other';
  // Null for a valid frame. A malformed frame keeps its type, read from the
  // first byte, but that type is not trustworthy, so its badge is drawn
  // struck through instead of in the colour of the type.
  const reason    = malformedReason(msg);

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
          <span
            className={`mtype-badge ${reason !== null ? 'mtype-malformed' : `mtype-${mcat}`}`}
            title={reason !== null ? `Malformed: ${reason}` : undefined}
          >
            {MTYPE_ABBREV[msg.mtype] ?? msg.mtype}
          </span>
        </td>
      )}
      {columns.device  && (
        <td className="col-device mono">
          {/* The cell is empty on a malformed row, so it carries the reason in
              short words: a touch screen cannot hover to read the title. */}
          {reason !== null
            ? <span className="malformed-tag" title={reason}>malformed: {malformedShort(msg)}</span>
            : device}
        </td>
      )}
      {columns.label   && <LabelCell msg={msg} ctx={ctx} />}
      {columns.gateway && (
        <td className="col-gateway mono" title={msg.gateway_eui ?? undefined}>
          {msg.gateway_eui ? shortEui(msg.gateway_eui) : ''}
        </td>
      )}
      {columns.joinEui && <td className="col-joineui mono">{msg.join_eui ?? ''}</td>}
      {columns.devNonce && <td className="col-devnonce mono">{msg.dev_nonce ?? ''}</td>}
      {columns.fcnt    && <td className="col-fcnt mono">{msg.fcnt ?? ''}</td>}
      {columns.fport   && <td className="col-fport mono">{msg.fport ?? ''}</td>}
      {columns.flags   && <FlagsCell msg={msg} />}
      {columns.fopts   && <td className="col-fopts mono">{msg.f_opts_len ?? ''}</td>}
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
          <span className={`status-dot status-dot-${STATUS_DOT[msg.status] ?? 'forwarded'}`} />
        </td>
      )}
    </>
  );
}
