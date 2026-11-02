import { useRef } from 'react';

export interface ColumnConfig {
  time: boolean;
  dir: boolean;
  mtype: boolean;
  device: boolean;
  label: boolean;
  gateway: boolean;
  joinEui: boolean;
  devNonce: boolean;
  fcnt: boolean;
  fport: boolean;
  flags: boolean;
  fopts: boolean;
  freq: boolean;
  datarate: boolean;
  rssi: boolean;
  snr: boolean;
  size: boolean;
  toa: boolean;
  status: boolean;
}

export const DEFAULT_COLUMNS: ColumnConfig = {
  time: true,
  dir: true,
  mtype: true,
  device: true,
  // Opt-in like the other wide columns: it would push the table past 800 px.
  label: false,
  // The short gateway form fits in the width left over at 800 px; the other
  // new columns are opt-in because they would push the table past it.
  gateway: true,
  joinEui: false,
  devNonce: false,
  fcnt: true,
  fport: true,
  flags: false,
  fopts: false,
  freq: true,
  datarate: true,
  rssi: true,
  snr: true,
  size: true,
  toa: true,
  status: true,
};

const LABELS: Record<keyof ColumnConfig, string> = {
  time: 'Time',
  dir: 'Dir',
  mtype: 'Type',
  device: 'Device',
  label: 'Label',
  gateway: 'Gateway',
  joinEui: 'JoinEUI',
  devNonce: 'DevNonce',
  fcnt: 'FCnt',
  fport: 'FPort',
  flags: 'Flags',
  fopts: 'FOpts',
  freq: 'Freq',
  datarate: 'DR',
  rssi: 'RSSI',
  snr: 'SNR',
  size: 'Size',
  toa: 'ToA',
  status: 'Status',
};

interface Props {
  columns: ColumnConfig;
  onChange: (c: ColumnConfig) => void;
  /** Downloads the rows shown as CSV. */
  onExport: () => void;
  /** How many rows the export would hold; with none the item is disabled. */
  exportCount: number;
  /**
   * Ends the session. Given only when login is on: the menu then closes with
   * a "Log out" item.
   */
  onLogout?: () => void;
}

/**
 * The ⚙ Settings menu: which columns the table shows, then the actions, the
 * CSV export and, when login is on, "Log out". The header has no width to
 * spare at 800 px, so everything that is not a filter lives here.
 */
export function ColumnPicker({ columns, onChange, onExport, exportCount, onLogout }: Props) {
  const menu = useRef<HTMLDetailsElement>(null);

  // An action is done once chosen, so the menu gets out of the way.
  const act = (action: () => void) => () => {
    if (menu.current) menu.current.open = false;
    action();
  };

  return (
    <details className="column-picker" ref={menu}>
      <summary>⚙ Settings</summary>
      <div className="column-picker-list">
        {(Object.keys(columns) as Array<keyof ColumnConfig>).map(key => (
          <label key={key}>
            <input
              type="checkbox"
              checked={columns[key]}
              onChange={e => onChange({ ...columns, [key]: e.target.checked })}
            />
            {LABELS[key]}
          </label>
        ))}
        <div className="column-picker-sep" role="separator" />
        <button
          type="button"
          className="column-picker-action"
          data-action="export-csv"
          onClick={act(onExport)}
          disabled={exportCount === 0}
          title={`Export ${exportCount} rows as CSV`}
        >
          Export CSV
        </button>
        {onLogout !== undefined && (
          <button
            type="button"
            className="column-picker-action"
            data-action="logout"
            onClick={act(onLogout)}
          >
            Log out
          </button>
        )}
      </div>
    </details>
  );
}
