import { MType } from '../types/messages';

export interface FilterState {
  device: string;
  mtype: MType[];
  direction: 'all' | 'up' | 'down';
  status: 'all' | 'Forwarded' | 'Blocked';
  timePreset: '5m' | '15m' | '1h' | '24h' | 'all';
}

export const DEFAULT_FILTERS: FilterState = {
  device: '',
  mtype: [],
  direction: 'all',
  status: 'all',
  timePreset: '5m',
};

const ALL_MTYPES: { value: MType; label: string }[] = [
  { value: 'JoinRequest',        label: 'JReq'  },
  { value: 'JoinAccept',         label: 'JAcc'  },
  { value: 'UnconfirmedDataUp',  label: 'UncUp' },
  { value: 'UnconfirmedDataDown',label: 'UncDn' },
  { value: 'ConfirmedDataUp',    label: 'ConUp' },
  { value: 'ConfirmedDataDown',  label: 'ConDn' },
  { value: 'RejoinRequest',      label: 'RJoin' },
  { value: 'Proprietary',        label: 'Prop'  },
];

interface Props {
  filters: FilterState;
  onChange: (f: FilterState) => void;
}

export function FilterBar({ filters, onChange }: Props) {
  const update = (partial: Partial<FilterState>) =>
    onChange({ ...filters, ...partial });

  const toggleMType = (m: MType) => {
    const next = filters.mtype.includes(m)
      ? filters.mtype.filter(t => t !== m)
      : [...filters.mtype, m];
    update({ mtype: next });
  };

  const mtypeLabel =
    filters.mtype.length === 0
      ? 'Type: All'
      : `Type: ${filters.mtype.length}`;

  return (
    <div className="filter-bar">
      <input
        className="filter-device"
        placeholder="DevAddr / EUI"
        value={filters.device}
        onChange={e => update({ device: e.target.value })}
      />

      {/* MType multi-select via <details> — touch-friendly */}
      <details className="filter-mtype-dropdown">
        <summary>{mtypeLabel}</summary>
        <div className="filter-mtype-list">
          {ALL_MTYPES.map(({ value, label }) => (
            <label key={value}>
              <input
                type="checkbox"
                checked={filters.mtype.includes(value)}
                onChange={() => toggleMType(value)}
              />
              {label}
            </label>
          ))}
        </div>
      </details>

      <select
        className="filter-select"
        value={filters.direction}
        onChange={e =>
          update({ direction: e.target.value as FilterState['direction'] })
        }
      >
        <option value="all">Dir: All</option>
        <option value="up">Up ▲</option>
        <option value="down">Down ▼</option>
      </select>

      <select
        className="filter-select"
        value={filters.status}
        onChange={e =>
          update({ status: e.target.value as FilterState['status'] })
        }
      >
        <option value="all">Status: All</option>
        <option value="Forwarded">Forwarded</option>
        <option value="Blocked">Blocked</option>
      </select>

      <select
        className="filter-select"
        value={filters.timePreset}
        onChange={e =>
          update({ timePreset: e.target.value as FilterState['timePreset'] })
        }
      >
        <option value="5m">Last 5m</option>
        <option value="15m">Last 15m</option>
        <option value="1h">Last 1h</option>
        <option value="24h">Last 24h</option>
        <option value="all">All time</option>
      </select>

      <button
        className="filter-clear"
        title="Clear all filters"
        onClick={() => onChange(DEFAULT_FILTERS)}
      >
        ✕
      </button>
    </div>
  );
}
