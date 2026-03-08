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
export type Status = 'Forwarded' | 'Blocked';
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
}

export interface WsMessage {
  type: 'message';
  data: MessageRecord;
}
