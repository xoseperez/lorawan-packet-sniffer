/// Minimal hand-written prost stubs for the chirpstack-concentratord v4 wire
/// format.
///
/// Field numbers verified against:
///   https://github.com/chirpstack/chirpstack/blob/master/api/proto/gw/gw.proto
///
/// Only the fields accessed by the concentratord proxy are declared; prost
/// silently skips unknown fields on decode, so missing fields are harmless.

// ─────────────────────────────────────────────────────────────────────────────
// Event wrapper  (PUB/SUB channel: concentratord → gateway bridge)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Event {
    #[prost(oneof = "event::Event", tags = "1, 2")]
    pub event: Option<event::Event>,
}

pub mod event {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        UplinkFrame(super::UplinkFrame),
        #[prost(message, tag = "2")]
        GatewayStats(super::GatewayStats),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Command wrapper  (REQ/REP channel: gateway bridge → concentratord)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Command {
    #[prost(oneof = "command::Command", tags = "1, 2, 3, 4")]
    pub command: Option<command::Command>,
}

pub mod command {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Command {
        /// SendDownlinkFrame — tag 1
        #[prost(message, tag = "1")]
        SendDownlinkFrame(super::DownlinkFrame),
        /// SetGatewayConfiguration — tag 2
        #[prost(message, tag = "2")]
        SetGatewayConfiguration(super::Empty),
        /// GetGatewayId — tag 3
        #[prost(message, tag = "3")]
        GetGatewayId(super::Empty),
        /// GetLocation — tag 4
        #[prost(message, tag = "4")]
        GetLocation(super::Empty),
    }
}

/// Placeholder for command variants we relay without inspecting.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Empty {}

// ─────────────────────────────────────────────────────────────────────────────
// Uplink
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UplinkFrame {
    #[prost(bytes = "vec", tag = "1")]
    pub phy_payload: Vec<u8>,
    // tags 2, 3 = legacy fields (skipped)
    #[prost(message, optional, tag = "4")]
    pub tx_info: Option<UplinkTxInfo>,
    #[prost(message, optional, tag = "5")]
    pub rx_info: Option<UplinkRxInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UplinkTxInfo {
    #[prost(uint32, tag = "1")]
    pub frequency: u32,
    #[prost(message, optional, tag = "2")]
    pub modulation: Option<Modulation>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct UplinkRxInfo {
    // fields 1–5: gateway_id, uplink_id, timestamps (skipped)
    #[prost(int32, tag = "6")]
    pub rssi: i32,
    #[prost(float, tag = "7")]
    pub snr: f32,
    // fields 8–15: channel, rf_chain, board, antenna, location, context, metadata (skipped)
    #[prost(int32, tag = "16")]
    pub crc_status: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct GatewayStats {
    // fields 1–4: gateway_id_legacy, time, location, config_version (skipped)
    #[prost(uint32, tag = "5")]
    pub rx_packets_received: u32,
    #[prost(uint32, tag = "6")]
    pub rx_packets_received_ok: u32,
    #[prost(uint32, tag = "7")]
    pub tx_packets_received: u32,
    #[prost(uint32, tag = "8")]
    pub tx_packets_emitted: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Modulation  (shared by UplinkTxInfo and DownlinkTxInfo)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Modulation {
    // tags 1, 2 absent/reserved
    #[prost(oneof = "modulation::Parameters", tags = "3, 4, 5")]
    pub parameters: Option<modulation::Parameters>,
}

pub mod modulation {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Parameters {
        #[prost(message, tag = "3")]
        Lora(super::LoraModulationInfo),
        #[prost(message, tag = "4")]
        Fsk(super::FskModulationInfo),
        #[prost(message, tag = "5")]
        LrFhss(super::Empty),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LoraModulationInfo {
    /// Bandwidth in Hz (e.g. 125000 = 125 kHz).
    #[prost(uint32, tag = "1")]
    pub bandwidth: u32,
    #[prost(uint32, tag = "2")]
    pub spreading_factor: u32,
    /// Legacy string form, e.g. `"4/5"` (populated by older concentratord).
    #[prost(string, tag = "3")]
    pub code_rate_legacy: String,
    // tag 4 = polarization_inversion (skipped)
    /// Preferred enum form; use `CodeRate::try_from(code_rate)`.
    #[prost(int32, tag = "5")]
    pub code_rate: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FskModulationInfo {
    // tag 1 = frequency_deviation (skipped)
    #[prost(uint32, tag = "2")]
    pub datarate: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Downlink
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DownlinkFrame {
    // tags 1, 2 absent; tag 4 = downlink_id_legacy (skipped)
    #[prost(uint32, tag = "3")]
    pub downlink_id: u32,
    #[prost(message, repeated, tag = "5")]
    pub items: Vec<DownlinkFrameItem>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DownlinkFrameItem {
    #[prost(bytes = "vec", tag = "1")]
    pub phy_payload: Vec<u8>,
    // tag 2 = tx_info_legacy (skipped)
    #[prost(message, optional, tag = "3")]
    pub tx_info: Option<DownlinkTxInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DownlinkTxInfo {
    #[prost(uint32, tag = "1")]
    pub frequency: u32,
    // tag 2 = power (skipped)
    #[prost(message, optional, tag = "3")]
    pub modulation: Option<Modulation>,
}

// ─────────────────────────────────────────────────────────────────────────────
// DownlinkTxAck  (REP response to SendDownlinkFrame command)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DownlinkTxAck {
    // tag 1 = gateway_id_legacy; tag 3 absent; tag 4 = downlink_id_legacy (all skipped)
    #[prost(uint32, tag = "2")]
    pub downlink_id: u32,
    #[prost(message, repeated, tag = "5")]
    pub items: Vec<DownlinkTxAckItem>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DownlinkTxAckItem {
    #[prost(int32, tag = "1")]
    pub status: i32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Enums
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum TxAckStatus {
    Ignored = 0,
    Ok = 1,
    TooLate = 2,
    TooEarly = 3,
    CollisionPacket = 4,
    CollisionBeacon = 5,
    TxFreq = 6,
    TxPower = 7,
    GpsUnlocked = 8,
    QueueFull = 9,
    InternalError = 10,
    DutyCycleOverflow = 11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum CrcStatus {
    NoCrc = 0,
    BadCrc = 1,
    CrcOk = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum CodeRate {
    CrUndefined = 0,
    Cr45 = 1,
    Cr46 = 2,
    Cr47 = 3,
    Cr48 = 4,
    Cr38 = 5,
    Cr26 = 6,
    Cr14 = 7,
    Cr16 = 8,
    Cr56 = 9,
    CrLi45 = 10,
    CrLi46 = 11,
    CrLi48 = 12,
}
