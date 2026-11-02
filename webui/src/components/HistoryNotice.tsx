import { Fragment } from 'react';
import type { FilterState } from './FilterBar';

/** The span the time selector names, as it reads after "matching messages in". */
const RANGE_WORDS: Record<FilterState['timePreset'], string> = {
  '5m':  'the last 5 min',
  '15m': 'the last 15 min',
  '1h':  'the last hour',
  '24h': 'the last 24 h',
  'all': 'the database',
};

export type NoticeKind = 'error' | 'write-failures' | 'behind' | 'cap';

export interface NoticePart {
  kind: NoticeKind;
  text: string;
}

export interface NoticeInput {
  /** Why the last load failed (no final full stop), or null. */
  error: string | null;
  /** The table holds rows, so "the earlier rows" are there to be seen. */
  hasRows: boolean;
  /** Records the database failed to store since the sniffer started. */
  writeFailures: number;
  /** Loads come back unsettled and are being retried. */
  behind: boolean;
  /** Set when more records match than the table holds. */
  cap: { shown: number; matching: number; timePreset: FilterState['timePreset'] } | null;
}

/**
 * The messages that apply, in the order they are shown: a failed load, records
 * that could not be saved, a database that is behind, and a capped range.
 */
export function noticeParts({ error, hasRows, writeFailures, behind, cap }: NoticeInput): NoticePart[] {
  const parts: NoticePart[] = [];

  if (error !== null) {
    const reason = error.replace(/\.+$/, '');
    parts.push({
      kind: 'error',
      text: `Could not load messages: ${reason}.${hasRows ? ' The table still shows the earlier rows.' : ''}`,
    });
  }

  if (writeFailures > 0) {
    parts.push({
      kind: 'write-failures',
      text:
        `${writeFailures.toLocaleString()} ${writeFailures === 1 ? 'record' : 'records'} could not be ` +
        'saved to the database since the sniffer last started (the count restarts at 0 with it); ' +
        'see its log.',
    });
  }

  if (behind) {
    parts.push({
      kind: 'behind',
      text: 'The newest records may be missing: the database is behind. Retrying.',
    });
  }

  if (cap !== null) {
    const range = RANGE_WORDS[cap.timePreset] ?? RANGE_WORDS.all;
    parts.push({
      kind: 'cap',
      text:
        `Showing the newest ${cap.shown.toLocaleString()} of ` +
        `${cap.matching.toLocaleString()} matching messages in ${range}`,
    });
  }

  return parts;
}

/**
 * One line between the table and the footer: why the table may not show
 * everything. Rendered only when there is something to say. An error is drawn
 * in the error colours; the full text is in the title, since a line this long
 * is cut with an ellipsis at 800 px.
 */
export function HistoryNotice(props: NoticeInput) {
  const parts = noticeParts(props);
  if (parts.length === 0) return null;

  const full = parts.map(p => p.text).join(' ');
  const isError = parts.some(p => p.kind === 'error');

  return (
    <div
      id="history-notice"
      className={`history-notice${isError ? ' error' : ''}`}
      role="status"
      title={full}
    >
      {parts.map((p, i) => (
        <Fragment key={p.kind}>
          {i > 0 && ' '}
          <span data-notice={p.kind}>{p.text}</span>
        </Fragment>
      ))}
    </div>
  );
}
