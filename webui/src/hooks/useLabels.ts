import { useCallback, useState } from 'react';
import { LabelsFrame } from '../types/messages';
import { useWebSocket } from './useWebSocket';

// How long a save or delete may wait for the server before the dialog gives
// up and shows an error. Without a limit, a connection that stalls would leave
// the dialog busy, with its buttons disabled, until the browser's own (very
// long) timeout.
const REQUEST_TIMEOUT_MS = 15_000;

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/**
 * Turns a failed response into text for the dialog. The API answers with
 * `{"error": "..."}`. Other answers, such as the plain-text 403 of the Host
 * check or a proxy's error page, fall back to the status line, plus the body
 * when it is a short plain-text message that says what to do.
 */
async function describeFailure(res: Response): Promise<string> {
  const statusLine = `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ''}`;

  let text = '';
  try {
    text = (await res.text()).trim();
  } catch {
    return statusLine;
  }

  try {
    const body: unknown = JSON.parse(text);
    if (isRecord(body) && typeof body.error === 'string' && body.error !== '') {
      return body.error;
    }
  } catch {
    /* not JSON */
  }

  const isPlainText = (res.headers.get('content-type') ?? '').startsWith('text/plain');
  if (isPlainText && text !== '' && text.length <= 300) {
    return `${statusLine}: ${text}`;
  }
  return statusLine;
}

/** Sends one label request. Resolves on success, throws Error(message) otherwise. */
async function sendLabelRequest(device: string, init: RequestInit): Promise<void> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

  let res: Response;
  try {
    res = await fetch(`/api/labels/${encodeURIComponent(device)}`, {
      ...init,
      signal: controller.signal,
    });
  } catch (e) {
    if (controller.signal.aborted) {
      throw new Error('The sniffer did not answer in time. Check the connection and try again.');
    }
    throw new Error(
      `Could not reach the sniffer${e instanceof Error && e.message ? ` (${e.message})` : ''}. ` +
        'Check the connection and try again.',
    );
  } finally {
    clearTimeout(timer);
  }

  if (!res.ok) throw new Error(await describeFailure(res));
}

export interface UseLabels {
  /** Label text by device identifier (DevAddr or DevEUI, as in the records). */
  labels: Record<string, string>;
  /** Sets the label. Empty or whitespace-only text removes it. Throws Error(message) on failure. */
  saveLabel: (device: string, text: string) => Promise<void>;
  /** Removes the label; succeeds even when there is none. Throws Error(message) on failure. */
  removeLabel: (device: string) => Promise<void>;
}

/**
 * Labels of the devices seen by the sniffer.
 *
 * The state comes only from the `/ws/labels` stream: a full snapshot replaces
 * it and each change event updates one entry. The PUT and DELETE responses are
 * deliberately never written into the state. A response can arrive after a
 * newer change made in another browser, and applying it would put the older
 * label back; the stream delivers changes in the order the server saved them,
 * so following it alone always ends on the stored value.
 */
export function useLabels(): UseLabels {
  const [labels, setLabels] = useState<Record<string, string>>({});

  const handleFrame = useCallback((frame: LabelsFrame | null) => {
    if (!isRecord(frame)) return;

    if (frame.type === 'labels') {
      // Replace, never merge: a snapshot is also sent after a lag, when
      // entries removed meanwhile would otherwise linger.
      const next: Record<string, string> = {};
      if (isRecord(frame.data)) {
        for (const [device, label] of Object.entries(frame.data)) {
          if (typeof label === 'string') next[device] = label;
        }
      }
      setLabels(next);
      return;
    }

    if (frame.type === 'label' && isRecord(frame.data)) {
      const { device, label } = frame.data;
      if (typeof device !== 'string') return;
      setLabels(prev => {
        if (typeof label === 'string' && label !== '') {
          return prev[device] === label ? prev : { ...prev, [device]: label };
        }
        if (!Object.prototype.hasOwnProperty.call(prev, device)) return prev;
        const next = { ...prev };
        delete next[device];
        return next;
      });
    }
  }, []);

  useWebSocket<LabelsFrame | null>('/ws/labels', handleFrame);

  const removeLabel = useCallback(
    (device: string) => sendLabelRequest(device, { method: 'DELETE' }),
    [],
  );

  const saveLabel = useCallback(
    (device: string, text: string) => {
      const label = text.trim();
      // The server treats an empty label as a removal too. Going through DELETE
      // keeps one path for it, and DELETE succeeds whether or not a label exists.
      if (label === '') return removeLabel(device);
      return sendLabelRequest(device, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ label }),
      });
    },
    [removeLabel],
  );

  return { labels, saveLabel, removeLabel };
}
