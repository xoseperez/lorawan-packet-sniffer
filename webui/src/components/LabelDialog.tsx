import { FormEvent, useEffect, useId, useRef, useState } from 'react';

// Same limit the server enforces on the trimmed label.
const MAX_LABEL_CHARS = 64;

interface Props {
  /** Identifier being labelled: a DevAddr ("00E8A80B") or a DevEUI ("36:31:...:39"). */
  device: string;
  /** Label stored for the identifier right now, if any. Follows changes made elsewhere. */
  current: string | undefined;
  /** Saves the text (empty removes the label). Rejects with Error(message) on failure. */
  onSave: (text: string) => Promise<void>;
  /** Removes the label. Rejects with Error(message) on failure. */
  onDelete: () => Promise<void>;
  onClose: () => void;
}

function errorText(e: unknown): string {
  return e instanceof Error && e.message ? e.message : 'The change could not be saved.';
}

export function LabelDialog({ device, current, onSave, onDelete, onClose }: Props) {
  // Only the initial value comes from `current`. If the label changes in
  // another browser while this dialog is open, the field must not be rewritten
  // under the user's hands; the Delete button, which depends on whether a
  // label exists, does follow the live value.
  const [text, setText] = useState(current ?? '');
  // Which request is in flight, if any. Buttons are disabled meanwhile.
  const [pending, setPending] = useState<'save' | 'delete' | null>(null);
  const busy = pending !== null;
  const [error, setError] = useState<string | null>(null);

  const titleId = useId();
  const inputId = useId();
  const errorId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // The document-level key handler outlives renders; it reads these refs so it
  // always sees the current values.
  const busyRef = useRef(busy);
  busyRef.current = busy;
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  // True while a press that began inside the dialog is still in progress. A
  // text selection dragged out of the field ends with a click on the backdrop,
  // and that must not count as a click outside.
  const pressStartedInside = useRef(false);

  const kind = device.includes(':') ? 'DevEUI' : 'DevAddr';

  // Focus the field with its text selected on open, and give focus back to
  // whatever opened the dialog (normally the label cell) on close.
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    inputRef.current?.focus();
    inputRef.current?.select();
    return () => {
      // A virtualised row may have been recycled while the dialog was open.
      if (opener && opener.isConnected) opener.focus();
    };
  }, []);

  // Escape cancels and Tab stays inside the dialog. This listens on the
  // document rather than on the dialog because focus can end up outside it,
  // for instance on <body> when a focused button becomes disabled.
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (e.isComposing) return; // Escape/Tab belong to the input method editor
      if (e.key === 'Escape') {
        e.preventDefault();
        if (!busyRef.current) onCloseRef.current();
        return;
      }
      if (e.key !== 'Tab') return;

      const dialog = dialogRef.current;
      if (!dialog) return;
      const focusable = Array.from(
        dialog.querySelectorAll<HTMLElement>('input, button'),
      ).filter(el => !el.hasAttribute('disabled'));
      if (focusable.length === 0) {
        e.preventDefault();
        dialog.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;
      if (!dialog.contains(active)) {
        e.preventDefault();
        (e.shiftKey ? last : first).focus();
      } else if (e.shiftKey && (active === first || active === dialog)) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    }
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, []);

  async function run(kindOfRequest: 'save' | 'delete', action: () => Promise<void>) {
    if (busyRef.current) return;
    busyRef.current = true; // before the re-render, so a double click cannot send twice
    setPending(kindOfRequest);
    setError(null);
    try {
      await action();
    } catch (e) {
      setError(errorText(e));
      setPending(null);
      // The button that was pressed is disabled now and dropped focus; put it
      // back in the field so the keyboard keeps working.
      requestAnimationFrame(() => inputRef.current?.focus());
      return;
    }
    // Success: the label itself reaches the table through the label stream.
    onClose();
  }

  function handleSubmit(e: FormEvent) {
    e.preventDefault(); // Enter in the field submits the form
    void run('save', () => onSave(text));
  }

  return (
    <div
      className="modal-backdrop"
      onPointerDown={e => {
        pressStartedInside.current = e.target !== e.currentTarget;
      }}
      onClick={e => {
        const outside = e.target === e.currentTarget && !pressStartedInside.current;
        pressStartedInside.current = false;
        if (outside && !busy) onClose();
      }}
    >
      <div
        ref={dialogRef}
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
      >
        <h2 id={titleId} className="modal-title">
          Label for {kind} <span className="modal-device mono">{device}</span>
        </h2>

        <form onSubmit={handleSubmit}>
          <label className="modal-field-label" htmlFor={inputId}>
            Label
          </label>
          <input
            ref={inputRef}
            id={inputId}
            className="modal-input"
            type="text"
            value={text}
            maxLength={MAX_LABEL_CHARS}
            readOnly={busy}
            autoComplete="off"
            placeholder="e.g. Kitchen sensor"
            aria-invalid={error !== null}
            aria-describedby={error !== null ? errorId : undefined}
            onChange={e => setText(e.target.value)}
          />
          {error !== null && (
            <p id={errorId} className="modal-error" role="alert">
              {error}
            </p>
          )}

          <div className="modal-actions">
            {current !== undefined && (
              <button
                type="button"
                className="modal-btn modal-btn-danger"
                disabled={busy}
                onClick={() => void run('delete', onDelete)}
              >
                {pending === 'delete' ? 'Deleting…' : 'Delete'}
              </button>
            )}
            <button type="button" className="modal-btn" disabled={busy} onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="modal-btn modal-btn-primary" disabled={busy}>
              {pending === 'save' ? 'Saving…' : 'Save'}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
