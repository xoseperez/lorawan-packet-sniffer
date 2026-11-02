//! Random bytes from the operating system, behind a trait that can fail.
//!
//! The WebUI login needs random bytes for its token, its session secret and
//! every session it hands out.  Reading them can fail (no entropy source in a
//! stripped-down container, a broken `getrandom`), and when it does the
//! sniffer must report an error: never panic, fall back to something weaker,
//! or carry on with an empty or short value.  `ring`'s own `SecureRandom`
//! trait is sealed, so this small trait wraps it and lets tests put a source
//! that fails in its place.

use std::fmt;

/// Reading random bytes failed.  `ring` gives no detail, so neither does this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RandomError;

impl fmt::Display for RandomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the operating system's random source failed")
    }
}

impl std::error::Error for RandomError {}

/// A source of random bytes that may fail.
pub trait RandomSource: Send + Sync {
    /// Fill `out` completely, or fail without a partial result being used.
    fn fill(&self, out: &mut [u8]) -> Result<(), RandomError>;
}

/// The production source: the operating system's, through `ring`.
pub struct SystemRandomSource(ring::rand::SystemRandom);

impl SystemRandomSource {
    pub fn new() -> Self {
        SystemRandomSource(ring::rand::SystemRandom::new())
    }
}

impl Default for SystemRandomSource {
    fn default() -> Self {
        Self::new()
    }
}

impl RandomSource for SystemRandomSource {
    fn fill(&self, out: &mut [u8]) -> Result<(), RandomError> {
        use ring::rand::SecureRandom;
        self.0.fill(out).map_err(|_| RandomError)
    }
}

/// `N` random bytes from `source`.
pub fn random_bytes<const N: usize>(source: &dyn RandomSource) -> Result<[u8; N], RandomError> {
    let mut out = [0u8; N];
    source.fill(&mut out)?;
    Ok(out)
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// A fresh login token: 16 random bytes as 32 lower-case hex characters,
/// the shape the installer and `openssl rand -hex 16` produce.
pub fn generate_token(source: &dyn RandomSource) -> Result<String, RandomError> {
    Ok(hex(&random_bytes::<16>(source)?))
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// A source that always fails.
    pub struct FailingRandom;

    impl RandomSource for FailingRandom {
        fn fill(&self, _: &mut [u8]) -> Result<(), RandomError> {
            Err(RandomError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FailingRandom;
    use super::*;

    #[test]
    fn tokens_are_32_hex_characters_and_differ() {
        let source = SystemRandomSource::new();
        let a = generate_token(&source).unwrap();
        let b = generate_token(&source).unwrap();
        assert_eq!(a.len(), 32);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_failing_source_is_an_error_not_a_short_value() {
        assert_eq!(generate_token(&FailingRandom), Err(RandomError));
        assert_eq!(random_bytes::<32>(&FailingRandom), Err(RandomError));
    }

    #[test]
    fn hex_is_lower_case_and_two_digits_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
