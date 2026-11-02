/**
 * The WebUI's side of the login: asking the sniffer whether a session is
 * needed and held, logging in and out, and a small bus that carries "the
 * session is gone" from wherever a 401 is seen to the page that shows the login.
 *
 * No React in here, and only relative URLs: the sniffer may sit behind a
 * reverse proxy under any scheme or host, and the cookie is the browser's to
 * keep, so nothing here ever reads or stores the session.
 */

export type SessionCheck =
  /** Login is off: the WebUI is open to anyone who can reach it. */
  | { kind: 'open' }
  /** Login is on and this browser holds a valid session. */
  | { kind: 'logged-in' }
  /** Login is on and this browser has no valid session. */
  | { kind: 'login' }
  /** No usable answer: the sniffer is down, restarting, or something else answered. */
  | { kind: 'unreachable' };

// How long a request may wait before it counts as not answered. Without a
// limit, a connection that stalls would leave the page on "Connecting…", or
// the login button disabled, until the browser's own (very long) timeout.
const CHECK_TIMEOUT_MS = 10_000;
const REQUEST_TIMEOUT_MS = 15_000;

const UNREACHABLE_TEXT = 'Cannot reach the sniffer';

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/**
 * Asks `GET /api/session`. A 200 says whether login is on; a 401 means it is
 * on and this browser has no valid session. Anything else, a network error
 * included, is "unreachable": the caller tries again later.
 */
export async function checkSession(): Promise<SessionCheck> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), CHECK_TIMEOUT_MS);
  try {
    const res = await fetch('/api/session', { cache: 'no-store', signal: controller.signal });
    if (res.status === 401) return { kind: 'login' };
    if (!res.ok) return { kind: 'unreachable' };
    const body: unknown = await res.json();
    if (isRecord(body) && body.login === true) return { kind: 'logged-in' };
    if (isRecord(body) && body.login === false) return { kind: 'open' };
    // A 200 that is not the sniffer's answer, such as a proxy's own page.
    return { kind: 'unreachable' };
  } catch {
    return { kind: 'unreachable' };
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Turns a failed response into text. The API answers with
 * `{"error": "..."}`; any other body (a proxy's page, a body cut off by the
 * size limit) falls back to the status line.
 */
async function failureText(res: Response): Promise<string> {
  try {
    const body: unknown = JSON.parse(await res.text());
    if (isRecord(body) && typeof body.error === 'string' && body.error !== '') return body.error;
  } catch {
    /* not JSON, or the body could not be read */
  }
  return `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ''}`;
}

/**
 * Posts JSON. Both login and logout require the JSON content type: a request
 * a web page on another site can make without a preflight cannot carry it, so
 * it cannot start or end a session here.
 */
function postJson(path: string, body: unknown, signal: AbortSignal): Promise<Response> {
  return fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    cache: 'no-store',
    signal,
  });
}

/**
 * Sends the token to `POST /api/login`. Resolves to null when the sniffer
 * accepted it (the browser now holds the session cookie), else to the text to
 * show: the server's own message ("Wrong token", the lockout wait, the refusal
 * that names a proxy changing the Host header), "Cannot reach the sniffer", or
 * the status line.
 */
export async function logIn(token: string): Promise<string | null> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    let res: Response;
    try {
      res = await postJson('/api/login', { token }, controller.signal);
    } catch {
      return UNREACHABLE_TEXT;
    }
    if (res.ok) return null;
    return await failureText(res);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Ends the session in this browser with `POST /api/logout`. A browser that
 * already has no valid session counts as logged out. Rejects with an
 * `Error` carrying the text to show when the sniffer could not be told, so the
 * page does not claim a logout that did not happen.
 */
export async function logOut(): Promise<void> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    let res: Response;
    try {
      res = await postJson('/api/logout', {}, controller.signal);
    } catch {
      throw new Error(UNREACHABLE_TEXT);
    }
    if (res.ok || res.status === 401) return;
    throw new Error(await failureText(res));
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// "The session is gone"
//
// A 401 can surface anywhere: a history load, a label save, a WebSocket that
// is refused. Each place reports it here, and the one page that owns the login
// listens, so no component needs to know about the others.
// ---------------------------------------------------------------------------

const unauthorizedListeners = new Set<() => void>();

/** Calls `listener` whenever a 401 is reported. Returns the function that stops it. */
export function onUnauthorized(listener: () => void): () => void {
  unauthorizedListeners.add(listener);
  return () => {
    unauthorizedListeners.delete(listener);
  };
}

/** Reports that the sniffer refused a request for want of a session. */
export function reportUnauthorized(): void {
  for (const listener of [...unauthorizedListeners]) listener();
}
