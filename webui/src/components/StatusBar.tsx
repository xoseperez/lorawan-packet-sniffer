import { WsStatus } from '../hooks/useWebSocket';

interface Props {
  /**
   * Records that match the time range and filters: the rows shown plus the
   * older matches the last load counted. Null when not known yet.
   */
  matching: number | null;
  /** A load is waiting or in flight: Matching reads "Loading…". */
  busy: boolean;
  shown: number;
  /** Blocked rows among the shown ones. */
  blocked: number;
  /**
   * Malformed records that match the other filters, as counted at the last
   * load plus those received live since. Null when the server did not say.
   */
  malformed: number | null;
  /** Whether the "Show malformed" box is ticked. */
  showMalformed: boolean;
  wsStatus: WsStatus;
  isLive: boolean;
  onToggleLive: () => void;
}

/** The version and the copyright line: at the right of the status bar, and under the login card. */
export const VERSION_LINE = `v${__APP_VERSION__} | © Xose Pérez for RAKwireless 2026`;

const WS_LABEL: Record<WsStatus, string> = {
  connecting:   'Connecting…',
  connected:    'Connected',
  disconnected: 'Disconnected',
};

const MATCHING_TITLE =
  'Records that match the time range and filters: the rows shown plus the older ' +
  'matches the database counted at the last load. It can be high, never low.';

const SHOWN_TITLE = 'Rows in the table.';

const BLOCKED_TITLE = 'Blocked records among the rows shown.';

const MALFORMED_TITLE =
  'Frames that cannot be valid LoRaWAN (CRC failed, bad header or wrong length) ' +
  'that match the other filters, counted at the last load plus those received since. ' +
  'It does not shrink as rows age out until the next load. Tick "Show malformed" ' +
  'in the Type menu to show them.';

const MALFORMED_UNKNOWN_TITLE =
  'Not available: the sniffer could not count them (an older version, or a database ' +
  'that is not in WAL mode).';

/** A number for a count item; the raw value goes in `data-value` for scripts. */
function Count({
  stat, label, value, title, text,
}: {
  stat: string;
  label: string;
  value: number | null;
  title: string;
  /** Replaces the number (for "Loading…"). */
  text?: string;
}) {
  return (
    <span
      className="status-count"
      data-stat={stat}
      data-value={text === undefined && value !== null ? value : undefined}
      title={title}
    >
      {label}: {text ?? (value === null ? '–' : value.toLocaleString())}
    </span>
  );
}

export function StatusBar({
  matching, busy, shown, blocked, malformed, showMalformed, wsStatus, isLive, onToggleLive,
}: Props) {
  return (
    <div className="status-bar">
      <Count
        stat="matching"
        label="Matching"
        value={matching}
        title={MATCHING_TITLE}
        text={busy ? 'Loading…' : undefined}
      />
      <Count stat="shown" label="Shown" value={shown} title={SHOWN_TITLE} />
      <Count stat="blocked" label="Blocked" value={blocked} title={BLOCKED_TITLE} />
      {/* Always rendered, also at 0 or "–", so the bar does not change when a
          malformed row arrives. */}
      <Count
        stat="malformed"
        label={`Malformed ${showMalformed ? 'shown' : 'hidden'}`}
        value={malformed}
        title={malformed === null ? MALFORMED_UNKNOWN_TITLE : MALFORMED_TITLE}
      />

      <span
        className={`ws-indicator ws-${wsStatus}`}
        title={`WebSocket: ${wsStatus}`}
        aria-label={`WebSocket ${wsStatus}`}
      >
        <span className="ws-dot" aria-hidden="true" />
        {WS_LABEL[wsStatus]}
      </span>

      <span className="status-copyright">{VERSION_LINE}</span>

      <button
        className={`live-toggle ${isLive ? 'live' : 'paused'}`}
        onClick={onToggleLive}
        aria-pressed={isLive}
      >
        <span className="live-dot" aria-hidden="true" />
        {isLive ? 'Live' : 'Paused'}
      </button>
    </div>
  );
}
