export type MType =
  | 'JoinRequest'
  | 'JoinAccept'
  | 'UnconfirmedDataUp'
  | 'UnconfirmedDataDown'
  | 'ConfirmedDataUp'
  | 'ConfirmedDataDown'
  | 'RejoinRequest'
  | 'Proprietary'
  | 'Unknown';

export type Direction = 'up' | 'down';
// 'Captured' is used in listen mode, where nothing is forwarded or blocked.
export type Status = 'Forwarded' | 'Blocked' | 'Captured';
export type Protocol = 'udp' | 'concentratord';
export type CrcStatus = 'Ok' | 'Fail' | 'NoCrc';
// Why a frame cannot be a valid LoRaWAN 1.0/1.1 frame: the gateway reported a
// failed CRC, the MHDR has non-zero reserved or major-version bits, or the
// length does not fit the frame's type.
export type Malformed = 'crc' | 'mhdr' | 'length';

export interface MessageRecord {
  id: string;
  timestamp: string; // ISO-8601
  direction: Direction;
  status: Status;
  blocked_by: string | null;
  source_protocol: Protocol;
  freq_mhz: number;
  datarate: string;
  rssi: number | null;
  snr: number | null;
  crc_status: CrcStatus | null;
  mtype: MType;
  dev_addr: string | null;
  dev_eui: string | null;
  fcnt: number | null;
  fport: number | null;
  payload_size: number;
  toa_ms: number | null;
  // EUIs are colon-separated uppercase hex, e.g. "AA:BB:CC:DD:00:11:22:33".
  gateway_eui: string | null;
  // Join requests only.
  join_eui: string | null;
  dev_nonce: number | null;
  // FCtrl flags of data frames. null means the flag does not exist for the
  // frame's direction or type (e.g. f_pending on an uplink), as opposed to
  // false, which means the bit was present and clear.
  adr: boolean | null;
  adr_ack_req: boolean | null;
  ack: boolean | null;
  class_b: boolean | null;
  f_pending: boolean | null;
  f_opts_len: number | null; // 0-15
  // Set when the frame cannot be valid LoRaWAN. The type and the radio data
  // are kept, but everything decoded from the frame body (identifiers, FCnt,
  // FPort, the FCtrl flags, FOptsLen) is null, because for such a frame it
  // would be noise.
  malformed: Malformed | null;
}

/**
 * Compact form of an EUI for narrow places: the last 8 hex digits without
 * colons (e.g. "AA:BB:CC:DD:00:11:22:33" -> "00112233").
 */
export function shortEui(eui: string): string {
  return eui.replace(/:/g, '').slice(-8);
}

export interface WsMessage {
  type: 'message';
  data: MessageRecord;
}

// The identifier of a row (DevAddr, else DevEUI) is needed by the matching rule
// in lib/history.ts, which must load on its own in the Node test, so it is
// defined there and re-exported for the components.
export { deviceId } from '../lib/history';

// What a frame was expected to be, for the "wrong length" reason. A frame that
// is not malformed never needs it; Proprietary and Unknown have no length
// rule, so "frame" is only a fallback.
const TYPE_WORDS: Record<MType, string> = {
  JoinRequest:         'join request',
  JoinAccept:          'join accept',
  RejoinRequest:       'rejoin request',
  UnconfirmedDataUp:   'data uplink',
  ConfirmedDataUp:     'data uplink',
  UnconfirmedDataDown: 'data downlink',
  ConfirmedDataDown:   'data downlink',
  Proprietary:         'frame',
  Unknown:             'frame',
};

/**
 * The full reason a frame is malformed, in plain words, or null when it is not.
 * For the hover text of a row.
 */
export function malformedReason(m: Pick<MessageRecord, 'malformed' | 'mtype'>): string | null {
  switch (m.malformed) {
    case 'crc':    return 'CRC check failed';
    case 'mhdr':   return 'not a LoRaWAN 1.0/1.1 header';
    case 'length': return `wrong length for a ${TYPE_WORDS[m.mtype] ?? 'frame'}`;
    default:       return null;
  }
}

/**
 * The reason in two or three words, short enough to sit in a table cell and
 * be read without hovering (a touch screen has no hover), or null when the
 * frame is not malformed.
 */
export function malformedShort(m: Pick<MessageRecord, 'malformed'>): string | null {
  switch (m.malformed) {
    case 'crc':    return 'CRC failed';
    case 'mhdr':   return 'bad header';
    case 'length': return 'wrong length';
    default:       return null;
  }
}

/**
 * Full set of labels, keyed by identifier. The server sends it when the
 * socket connects and again whenever a client has lagged, so a receiver must
 * replace what it holds rather than merge.
 */
export interface LabelsSnapshot {
  type: 'labels';
  data: Record<string, string>;
}

/** One label change. `label` is null when the label was removed. */
export interface LabelEvent {
  type: 'label';
  data: { device: string; label: string | null };
}

export type LabelsFrame = LabelsSnapshot | LabelEvent;
