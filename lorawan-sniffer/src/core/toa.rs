/// Compute the estimated LoRa Time-on-Air in milliseconds.
///
/// Uses the standard Semtech formula from the SX1276 datasheet.
/// Returns `None` for FSK datarates or unparseable strings.
///
/// # Parameters
/// - `payload_size`: PHY payload length in bytes (including MIC).
/// - `datarate`: Data rate string from the UDP JSON, e.g. `"SF7BW125"`.
/// - `coding_rate`: Coding rate string from the UDP JSON, e.g. `"4/5"`.
pub fn compute_lora_toa(payload_size: u16, datarate: &str, coding_rate: &str) -> Option<f64> {
    let (sf, bw_hz) = parse_datarate(datarate)?;
    let cr = parse_coding_rate(coding_rate).unwrap_or(1); // default 4/5

    // Symbol duration [s]
    let t_sym = 2f64.powi(sf as i32) / bw_hz as f64;

    // Preamble: 8 symbols + 4.25 sync symbols (standard LoRa)
    let t_preamble = (8.0 + 4.25) * t_sym;

    // Low data-rate optimisation (DE) is mandatory for SF11 and SF12
    let de: i32 = if sf >= 11 { 1 } else { 0 };

    // Explicit header (H = 0), CRC enabled (16-bit)
    let h: i32 = 0;
    let n_payload = payload_size as f64;
    let sf_f = sf as f64;
    let cr_f = cr as f64;
    let de_f = de as f64;
    let h_f = h as f64;

    let payload_sym_nb = 8.0
        + f64::max(
            f64::ceil(
                (8.0 * n_payload - 4.0 * sf_f + 28.0 + 16.0 - 20.0 * h_f)
                    / (4.0 * (sf_f - 2.0 * de_f)),
            ) * (cr_f + 4.0),
            0.0,
        );

    let t_payload = payload_sym_nb * t_sym;
    let toa_ms = (t_preamble + t_payload) * 1000.0;

    Some(toa_ms)
}

/// Parse a LoRa datarate string like `"SF7BW125"` into `(sf, bw_hz)`.
///
/// Returns `None` for FSK rates (no `"SF"` prefix).
fn parse_datarate(s: &str) -> Option<(u8, u32)> {
    if !s.starts_with("SF") {
        return None; // FSK or unknown
    }
    let rest = &s[2..];
    let bw_pos = rest.find("BW")?;
    let sf: u8 = rest[..bw_pos].parse().ok()?;
    let bw_khz: u32 = rest[bw_pos + 2..].parse().ok()?;
    Some((sf, bw_khz * 1_000))
}

/// Parse a coding-rate string like `"4/5"` into the `CR` denominator offset.
///
/// `"4/5"` → 1, `"4/6"` → 2, `"4/7"` → 3, `"4/8"` → 4.
fn parse_coding_rate(s: &str) -> Option<u8> {
    let slash = s.find('/')?;
    let denom: u8 = s[slash + 1..].parse().ok()?;
    if !(5..=8).contains(&denom) {
        return None;
    }
    Some(denom - 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_datarate() {
        assert_eq!(parse_datarate("SF7BW125"), Some((7, 125_000)));
        assert_eq!(parse_datarate("SF12BW125"), Some((12, 125_000)));
        assert_eq!(parse_datarate("SF9BW500"), Some((9, 500_000)));
        assert_eq!(parse_datarate("50000"), None); // FSK
    }

    #[test]
    fn test_parse_coding_rate() {
        assert_eq!(parse_coding_rate("4/5"), Some(1));
        assert_eq!(parse_coding_rate("4/8"), Some(4));
        assert_eq!(parse_coding_rate("4/9"), None);
    }

    #[test]
    fn test_toa_sf7bw125() {
        // SF7BW125, 23-byte payload: ~61.7 ms (typical unconfirmed uplink)
        let toa = compute_lora_toa(23, "SF7BW125", "4/5").unwrap();
        assert!(toa > 55.0 && toa < 70.0, "toa={:.1} ms", toa);
    }

    #[test]
    fn test_toa_sf12bw125() {
        // SF12BW125, 23-byte payload: ~1400 ms
        let toa = compute_lora_toa(23, "SF12BW125", "4/5").unwrap();
        assert!(toa > 1300.0 && toa < 1600.0, "toa={:.1} ms", toa);
    }
}
