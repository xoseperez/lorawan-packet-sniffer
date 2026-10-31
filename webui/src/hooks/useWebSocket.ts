import { useEffect, useRef, useState } from 'react';

export type WsStatus = 'connecting' | 'connected' | 'disconnected';

/**
 * Keeps a WebSocket to `path` (e.g. '/ws') open, reconnecting after a drop, and
 * hands every parsed JSON frame to `onEvent`. The handler decides which frame
 * types it cares about; frames that are not valid JSON are ignored.
 */
export function useWebSocket<T>(
  path: string,
  onEvent: (event: T) => void,
): { status: WsStatus } {
  const [status, setStatus] = useState<WsStatus>('connecting');

  // Stable ref so connect() closure always calls the latest onEvent
  // without needing to be re-created on every render.
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;

  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;

    function connect() {
      if (cancelled) return;
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = `${proto}//${location.host}${path}`;

      ws = new WebSocket(url);
      setStatus('connecting');

      ws.onopen = () => {
        if (!cancelled) setStatus('connected');
      };

      ws.onmessage = (event: MessageEvent<string>) => {
        let frame: T;
        try {
          frame = JSON.parse(event.data) as T;
        } catch {
          return; // ignore malformed frames
        }
        onEventRef.current(frame);
      };

      ws.onclose = () => {
        if (!cancelled) {
          setStatus('disconnected');
          reconnectTimer = setTimeout(connect, 3000);
        }
      };

      ws.onerror = () => ws?.close();
    }

    connect();

    return () => {
      cancelled = true;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      ws?.close();
    };
  }, [path]); // reconnect only if the path changes; onEvent changes are handled via ref

  return { status };
}
