export interface ColumnConfig {
  time: boolean;
  dir: boolean;
  mtype: boolean;
  device: boolean;
  fcnt: boolean;
  fport: boolean;
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
  fcnt: true,
  fport: true,
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
  fcnt: 'FCnt',
  fport: 'FPort',
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
