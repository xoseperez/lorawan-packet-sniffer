import { useEffect, useRef, useState } from 'react';
import { MessageRecord, WsMessage } from '../types/messages';

export type WsStatus = 'connecting' | 'connected' | 'disconnected';

export function useWebSocket(onMessage: (record: MessageRecord) => void): { status: WsStatus } {
  const [status, setStatus] = useState<WsStatus>('connecting');

  // Stable ref so connect() closure always calls the latest onMessage
  // without needing to be re-created on every render.
  const onMessageRef = useRef(onMessage);
  onMessageRef.current = onMessage;

  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;

    function connect() {
      if (cancelled) return;
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = `${proto}//${location.host}/ws`;

      ws = new WebSocket(url);
      setStatus('connecting');

      ws.onopen = () => {
        if (!cancelled) setStatus('connected');
      };

      ws.onmessage = (event: MessageEvent<string>) => {
        try {
          const msg = JSON.parse(event.data) as WsMessage;
          if (msg.type === 'message') onMessageRef.current(msg.data);
        } catch {
          // ignore malformed frames
        }
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
  }, []); // connect once; onMessage changes are handled via ref

  return { status };
}
