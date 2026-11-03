import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { MType, Status, shortEui } from '../types/messages';
import { pickDay, todayKey } from '../lib/history';

export interface FilterState {
  device: string;
  mtype: MType[];
  status: 'all' | Status;
  gateway: string; // gateway EUI, '' = all gateways
  // A span counted back from now, or 'day': one local day, in `day`.
  timePreset: '5m' | '15m' | '1h' | '24h' | 'day';
  day: string | null; // local date 'YYYY-MM-DD'; only read when timePreset is 'day'
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
  day: null,
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

  // The calendar is the browser's own: an invisible date input laid over the
  // time select, opened with showPicker() so the popup appears under the
  // select. The select stays controlled by `filters.timePreset`, so choosing
  // "Select day" and then dismissing the calendar leaves the old entry shown.
  const timeSelect = useRef<HTMLSelectElement>(null);
  const dayInput = useRef<HTMLInputElement>(null);
  // Set when showPicker() is missing or refused: the date input is then shown
  // in the select's place, to be typed into.
  const [pickerFallback, setPickerFallback] = useState(false);
  // The select is hidden while the fallback shows, so it can take the focus
  // back only after that render.
  const refocusSelect = useRef(false);

  useEffect(() => {
    if (!pickerFallback && refocusSelect.current) {
      refocusSelect.current = false;
      timeSelect.current?.focus();
    }
  }, [pickerFallback]);

  const openPicker = () => {
    const input = dayInput.current;
    if (input === null) return;
    // Days to come are greyed out. "Today" moves on at midnight, so it is
    // read now rather than when the bar was drawn. With no day chosen the
    // input starts empty: the browser sends no change when the date picked is
    // the one already in the input, and today must be pickable.
    input.max = todayKey(Date.now());
    input.value = filters.day ?? '';
    try {
      // Needs the user's activation, so it runs inside the select's change
      // event, before anything is awaited.
      input.showPicker();
    } catch {
      setPickerFallback(true);
      input.focus();
    }
  };

  // A native listener, not React's onChange: React compares the value with the
  // one it saw last and misses a date that was set through the input's value
  // property (a script, a form-filling extension), which the browser still
  // reports as a change.
  useEffect(() => {
    const input = dayInput.current;
    if (input === null) return;
    const onDayChange = () => {
      // A partly typed date (the fallback input) or a day to come is not a
      // pick: it changes nothing, and the fallback stays for the next try.
      const day = pickDay(input.value, todayKey(Date.now()));
      if (day === null) return;
      if (pickerFallback) {
        refocusSelect.current = true;
        setPickerFallback(false);
      }
      onChange({ ...filters, timePreset: 'day', day });
    };
    input.addEventListener('change', onDayChange);
    return () => input.removeEventListener('change', onDayChange);
  }, [filters, onChange, pickerFallback]);

  const onDayKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape' && pickerFallback) {
      refocusSelect.current = true;
      setPickerFallback(false);
    }
  };

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

      <span className={`filter-time-wrap${pickerFallback ? ' picking' : ''}`}>
        <select
          ref={timeSelect}
          className="filter-select filter-time"
          value={filters.timePreset}
          onChange={e => {
            const value = e.target.value;
            if (value === 'pick') openPicker();
            else update({ timePreset: value as FilterState['timePreset'], day: null });
          }}
        >
          <option value="5m">Last 5m</option>
          <option value="15m">Last 15m</option>
          <option value="1h">Last 1h</option>
          <option value="24h">Last 24h</option>
          {filters.timePreset === 'day' && filters.day !== null && (
            <option value="day">{filters.day}</option>
          )}
          <option value="pick">Select day</option>
        </select>
        <input
          ref={dayInput}
          type="date"
          className="day-picker"
          data-day-picker
          data-fallback={pickerFallback ? 'true' : undefined}
          aria-label="Day"
          aria-hidden={pickerFallback ? undefined : true}
          tabIndex={-1}
          onBlur={() => setPickerFallback(false)}
          onKeyDown={onDayKeyDown}
        />
      </span>

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
