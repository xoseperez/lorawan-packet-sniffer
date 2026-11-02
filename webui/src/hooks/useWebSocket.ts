import { useEffect, useRef, useState } from 'react';
import { nextReconnect } from '../lib/history';
import { checkSession, reportUnauthorized } from '../session';

export type WsStatus = 'connecting' | 'connected' | 'disconnected';

/**
 * Keeps a WebSocket to `path` (e.g. '/ws') open, reconnecting after a drop, and
 * hands every parsed JSON frame to `onEvent`. The handler decides which frame
 * types it cares about; frames that are not valid JSON are ignored.
 *
 * `onOpen` runs every time the socket opens, the first time included. The
 * history hook reloads the selected range on each one, so a record broadcast
 * while the socket was down is not missed.
 *
 * Reconnects wait about 3 s, doubling after each further drop up to 60 s, and
 * start again at 3 s after a connection that lasted a minute. Each wait is
 * picked at random between half and all of that, so the pages of a restarted
 * sniffer do not all come back at one instant. A link that keeps dropping, or a
 * server that closes a client for falling behind (close code 1013), therefore
 * costs one reconnect a minute at most, not one every 3 s.
 *
 * A browser cannot read why a handshake was refused; it only sees the socket
 * close. So before each reconnect the hook asks `/api/session`: if the session
 * has ended it reports that and stops, instead of retrying a login that no
 * retry can pass. Any other answer, including "unreachable", reconnects as
 * before; a close for falling behind (code 1013) is not a session end.
 */
export function useWebSocket<T>(
  path: string,
  onEvent: (event: T) => void,
  onOpen?: () => void,
): { status: WsStatus } {
  const [status, setStatus] = useState<WsStatus>('connecting');

  // Stable refs so connect() closure always calls the latest handlers
  // without needing to be re-created on every render.
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;
  const onOpenRef = useRef(onOpen);
  onOpenRef.current = onOpen;

  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;
    // The nominal wait before the last attempt, not the randomised one:
    // doubling a randomised wait would drift. Null before the first drop.
    let nominalMs: number | null = null;
    // When the current connection opened, or null if it never did.
    let openedAt: number | null = null;

    function connect() {
      if (cancelled) return;
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = `${proto}//${location.host}${path}`;

      ws = new WebSocket(url);
      setStatus('connecting');

      ws.onopen = () => {
        if (cancelled) return;
        openedAt = Date.now();
        setStatus('connected');
        onOpenRef.current?.();
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
        if (cancelled) return;
        setStatus('disconnected');
        const lastConnectionMs = openedAt === null ? 0 : Date.now() - openedAt;
        openedAt = null;
        void checkSession().then(check => {
          // The page may have left while the check was out.
          if (cancelled) return;
          if (check.kind === 'login') {
            reportUnauthorized();
            return;
          }
          const next = nextReconnect(nominalMs, lastConnectionMs, Math.random());
          nominalMs = next.nominalMs;
          reconnectTimer = setTimeout(connect, next.delayMs);
        });
      };

      ws.onerror = () => ws?.close();
    }

    connect();

    return () => {
      cancelled = true;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      ws?.close();
    };
  }, [path]); // reconnect only if the path changes; handler changes are handled via refs

  return { status };
}
