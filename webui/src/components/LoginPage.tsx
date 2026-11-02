import { FormEvent, useRef, useState } from 'react';
import { logIn } from '../session';
import type { SessionCheck } from '../session';
import { VERSION_LINE } from './StatusBar';

interface Props {
  /**
   * Called once the sniffer has accepted the token. Asks the page to check the
   * session again and resolves to what it found, so this page can tell a login
   * that took from one that did not.
   */
  onLoggedIn: () => Promise<SessionCheck>;
}

const EMPTY_TOKEN_TEXT = 'Enter the token';

// The sniffer said yes and the next check still found no session, so the
// browser dropped the cookie: blocked for this site, or removed on the way by a
// proxy. Without this line the page would simply come back, empty, with no clue.
const NO_SESSION_TEXT =
  'The sniffer accepted the token, but this browser did not keep the session. ' +
  'Check that cookies are allowed for this address.';

export function LoginPage({ onLoggedIn }: Props) {
  const [token, setToken] = useState('');
  // A request is out (the login itself, or the check that follows it).
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const inputRef = useRef<HTMLInputElement>(null);
  // Read before the re-render, so a double tap or a key repeat cannot send the
  // token twice: every wrong token counts against everyone's attempts.
  const pendingRef = useRef(false);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault(); // Enter in the field submits the form
    if (pendingRef.current) return;

    // Whitespace around a pasted token is never part of it. Sending a blank
    // field would only use up one of the attempts the sniffer allows.
    const trimmed = token.trim();
    if (trimmed === '') {
      setError(EMPTY_TOKEN_TEXT);
      inputRef.current?.focus();
      return;
    }

    pendingRef.current = true;
    setPending(true);
    setError(null);

    let failure = await logIn(trimmed);
    if (failure === null) {
      // The cookie is set. If the page moves on this component goes away.
      const next = await onLoggedIn();
      if (next.kind === 'login') failure = NO_SESSION_TEXT;
    }

    pendingRef.current = false;
    setPending(false);
    if (failure !== null) {
      // The text stays in the field for another try. The button that was
      // pressed was disabled and dropped focus: put it back in the field.
      setError(failure);
      inputRef.current?.focus();
    }
  }

  return (
    <div className="login-page">
      <div className="login-card">
        <h1 className="login-title">
          <span className="app-title-primary">LoRaWAN</span>
          <span className="app-title-secondary"> Sniffer</span>
        </h1>

        <form onSubmit={e => void handleSubmit(e)} noValidate>
          <input
            ref={inputRef}
            id="login-token"
            className="login-input"
            type="password"
            autoComplete="current-password"
            autoFocus
            aria-label="Token"
            placeholder="Token"
            value={token}
            readOnly={pending}
            aria-invalid={error !== null}
            aria-describedby={error !== null ? 'login-error' : undefined}
            onChange={e => setToken(e.target.value)}
          />
          {error !== null && (
            <p id="login-error" className="login-error" role="alert">
              {error}
            </p>
          )}
          <button id="login-submit" className="login-btn" type="submit" disabled={pending}>
            Log in
          </button>
        </form>
      </div>

      <div className="login-version">{VERSION_LINE}</div>
    </div>
  );
}
