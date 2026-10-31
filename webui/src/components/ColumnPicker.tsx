export interface ColumnConfig {
  time: boolean;
  dir: boolean;
  mtype: boolean;
  device: boolean;
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
}

export function ColumnPicker({ columns, onChange }: Props) {
  return (
    <details className="column-picker">
      <summary>⚙ Cols</summary>
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
      </div>
    </details>
  );
}
