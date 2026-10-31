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

/**
 * The identifier a row can be labelled by: the DevAddr of a data frame, or the
 * DevEUI of a join request. A record never carries both. Join accepts, rejoin
 * requests, proprietary frames and truncated frames carry neither, so they
 * have no identifier and cannot be labelled.
 */
export function deviceId(m: Pick<MessageRecord, 'dev_addr' | 'dev_eui'>): string | null {
  return m.dev_addr ?? m.dev_eui ?? null;
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
