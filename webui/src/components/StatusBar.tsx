import { WsStatus } from '../hooks/useWebSocket';

interface Props {
  total: number;
  shown: number;
  blocked: number;
  /** Malformed rows among the loaded rows that pass the other filters. */
  malformedCount: number;
  /** Whether the "Show malformed" box is ticked. */
  showMalformed: boolean;
  wsStatus: WsStatus;
  isLive: boolean;
  onToggleLive: () => void;
}

const WS_LABEL: Record<WsStatus, string> = {
  connecting:   'Connecting…',
  connected:    'Connected',
  disconnected: 'Disconnected',
};

const MALFORMED_TITLE =
  'Frames that cannot be valid LoRaWAN (CRC failed, bad header or wrong length), ' +
  'among the rows that pass the other filters. Tick "Show malformed" in the Type ' +
  'menu to show them. Total and Blocked count every loaded row, malformed ones included.';

export function StatusBar({
  total, shown, blocked, malformedCount, showMalformed, wsStatus, isLive, onToggleLive,
}: Props) {
  return (
    <div className="status-bar">
      <span>Total: {total.toLocaleString()}</span>
      <span>Shown: {shown.toLocaleString()}</span>
      <span>Blocked: {blocked.toLocaleString()}</span>
      {/* Always rendered, also at 0, so the bar does not change when a malformed
          row arrives. */}
      <span title={MALFORMED_TITLE}>
        Malformed {showMalformed ? 'shown' : 'hidden'}: {malformedCount.toLocaleString()}
      </span>

      <span
        className={`ws-indicator ws-${wsStatus}`}
        title={`WebSocket: ${wsStatus}`}
        aria-label={`WebSocket ${wsStatus}`}
      >
        <span className="ws-dot" aria-hidden="true" />
        {WS_LABEL[wsStatus]}
      </span>

      <span className="status-copyright">
        v{__APP_VERSION__} | &copy; Xose Pérez for RAKwireless 2026
      </span>

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
