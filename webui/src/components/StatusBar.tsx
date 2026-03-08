import { WsStatus } from '../hooks/useWebSocket';

interface Props {
  total: number;
  shown: number;
  blocked: number;
  wsStatus: WsStatus;
  isLive: boolean;
  onToggleLive: () => void;
}

const WS_LABEL: Record<WsStatus, string> = {
  connecting:   'Connecting…',
  connected:    'Connected',
  disconnected: 'Disconnected',
};

export function StatusBar({ total, shown, blocked, wsStatus, isLive, onToggleLive }: Props) {
  return (
    <div className="status-bar">
      <span>Total: {total.toLocaleString()}</span>
      <span>Shown: {shown.toLocaleString()}</span>
      <span>Blocked: {blocked.toLocaleString()}</span>

      <span
        className={`ws-indicator ws-${wsStatus}`}
        title={`WebSocket: ${wsStatus}`}
        aria-label={`WebSocket ${wsStatus}`}
      >
        <span className="ws-dot" aria-hidden="true" />
        {WS_LABEL[wsStatus]}
      </span>

      <span className="status-copyright">&copy; RAKwireless 2026</span>

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
