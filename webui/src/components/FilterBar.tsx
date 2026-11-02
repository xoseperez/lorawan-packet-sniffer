import { MType, Status, shortEui } from '../types/messages';

export interface FilterState {
  device: string;
  mtype: MType[];
  status: 'all' | Status;
  gateway: string; // gateway EUI, '' = all gateways
  timePreset: '5m' | '15m' | '1h' | '24h' | 'all';
  // Malformed frames are noise most of the time, so they stay hidden until
  // asked for. The box is in the Type list because the header has no room
  // left for another control at 800 px.
  showMalformed: boolean;
}

export const DEFAULT_FILTERS: FilterState = {
  device: '',
  mtype: [],
  status: 'all',
  gateway: '',
  timePreset: '5m',
  showMalformed: false,
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
  gateways: string[]; // EUIs of the gateways seen so far
}

/**
 * Option labels for the gateway select. The short form is used unless two
 * gateways share the same last 8 digits, in which case those use the full EUI
 * so they can still be told apart.
 */
function gatewayOptions(gateways: string[], selected: string) {
  // A gateway saved in the filter may not have shown up in the loaded history
  // yet; keep it in the list so the select does not silently show "All" while
  // the filter is still applied.
  const euis = selected && !gateways.includes(selected) ? [...gateways, selected] : gateways;

  const shortCount = new Map<string, number>();
  for (const eui of euis) {
    const short = shortEui(eui);
    shortCount.set(short, (shortCount.get(short) ?? 0) + 1);
  }

  return euis.map(eui => ({
    value: eui,
    label: (shortCount.get(shortEui(eui)) ?? 0) > 1 ? eui : shortEui(eui),
  }));
}

export function FilterBar({ filters, onChange, gateways }: Props) {
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
          <label className="filter-malformed">
            <input
              type="checkbox"
              checked={filters.showMalformed}
              onChange={e => update({ showMalformed: e.target.checked })}
            />
            Show malformed
          </label>
        </div>
      </details>

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
        <option value="Captured">Captured</option>
      </select>

      <select
        className="filter-select filter-gateway"
        value={filters.gateway}
        title={filters.gateway || undefined}
        onChange={e => update({ gateway: e.target.value })}
      >
        <option value="">GW: All</option>
        {gatewayOptions(gateways, filters.gateway).map(({ value, label }) => (
          <option key={value} value={value}>{label}</option>
        ))}
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
