//! The WebUI login: the shared token, the session cookie, the lockout on
//! wrong tokens, and the HTTP layer that asks for a credential.
//!
//! ## Sessions
//!
//! A session is a cookie value `v1.<issued>.<nonce>.<mac>`: the time it was
//! made (unix seconds), 16 random bytes, and an HMAC-SHA256 over the first
//! three parts.  Nothing is stored per session, so a restart keeps them and
//! memory does not grow with logins.  The MAC key is derived from the token
//! *and* from a random secret kept in the database ([`session_key`]), so:
//!
//! * changing the token, or starting on a new database, ends every session;
//! * a guessed token is not enough to build a cookie, which means cookies are
//!   no way around the guessing limit below, and need not be counted by it.
//!
//! ## Guessing limit
//!
//! The token is only as strong as its owner made it, so every comparison of
//! one goes through [`Auth::check_token`] and the [`Lockout`] behind it: at
//! most [`Lockout::LIMIT`] wrong tokens are compared per fixed
//! [`Lockout::WINDOW`], whatever the number of requests; after that every
//! token is refused unexamined until the window ends.  A right token is never
//! counted.
//!
//! ## Secrets
//!
//! The token, the stored secret and session values never reach a format
//! string, a log line or a `Debug` output, and every comparison of one (the
//! token, a MAC) is `ring::hmac::verify`, which takes the same time wherever
//! the inputs first differ.

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use ring::hmac;
use serde::Deserialize;
use thiserror::Error;
use tokio::time::Instant;

use super::{json_error, origin_allowed};
use crate::core::linelog::{error_line, warn_line};
use crate::core::random::{hex, random_bytes, RandomError, RandomSource};
use crate::core::ratelimit::RateLimitedLog;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "lorawan_sniffer_session";

/// Keeps the session key apart from any other use of the stored secret.
pub const SESSION_KEY_LABEL: &[u8] = b"lorawan-sniffer session v1";

/// The version tag at the start of every session value.
const SESSION_VERSION: &str = "v1";

/// Random bytes in a session's nonce.
const NONCE_BYTES: usize = 16;

/// Size of an HMAC-SHA256 tag.
const MAC_BYTES: usize = 32;

// -- Errors ------------------------------------------------------------------

/// The login could not be set up or a session could not be made.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The operating system's random source failed.
    #[error("cannot read random bytes for the WebUI login: {0}")]
    Random(#[from] RandomError),
}

// -- Time --------------------------------------------------------------------

/// Where the lockout reads the time.  A trait so a test can move it by hand.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The production clock.  It is Tokio's, not `std`'s, so a test running on a
/// paused Tokio clock moves the lockout window with everything else.
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

// -- Guessing limit ----------------------------------------------------------

/// What the lockout made of one token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCheck {
    /// Too many wrong tokens in this window: the token was not looked at.
    /// Carries the time left until the window ends.
    Locked(Duration),
    /// The token is the right one.
    Right,
    /// The token is wrong, and has been counted.
    Wrong,
}

/// Whole seconds in `left`, rounded up, so a wait of a fraction of a second
/// is never announced as none.  For `Retry-After` and the log line.
pub fn ceil_secs(left: Duration) -> u64 {
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

/// The current counting window.
struct Window {
    start: Instant,
    /// Wrong tokens compared since `start`.
    wrong: u32,
    /// The refusal has been logged for this window.
    warned: bool,
}

/// The global limit on wrong tokens: fixed windows, so ten wrong tokens at
/// the end of one window and ten at the start of the next are twenty within
/// a few seconds; never more than twenty in any [`Lockout::WINDOW`].
///
/// The first window starts when the lockout is built (at startup), later ones
/// at the first attempt after the previous ended.  Nothing here waits, sleeps
/// or queues.
pub struct Lockout {
    inner: Mutex<Window>,
    clock: Arc<dyn Clock>,
}

impl Lockout {
    /// How long a window lasts.
    pub const WINDOW: Duration = Duration::from_secs(60);
    /// Wrong tokens that fill a window.
    pub const LIMIT: u32 = 10;

    pub fn new(clock: Arc<dyn Clock>) -> Self {
        let start = clock.now();
        Lockout {
            inner: Mutex::new(Window {
                start,
                wrong: 0,
                warned: false,
            }),
            clock,
        }
    }

    /// Judge one token: `compare` says whether it is the right one.
    ///
    /// The whole attempt happens under one lock: read the clock, end the
    /// window if its time is up, look at the count, compare, count.  Anything
    /// looser lets many requests that arrive together all pass the count
    /// before any of them has added to it, and then all be compared.  The
    /// clock is read under the lock so that an attempt is never judged with a
    /// time older than the state it is checked against.
    ///
    /// `compare` runs with the lock held, so it must be quick and must not
    /// wait for anything; nothing in here awaits.  A panic inside it leaves
    /// the count as it was (the wrong token was not counted yet) and the next
    /// attempt recovers the poisoned lock.
    pub fn attempt(&self, compare: impl FnOnce() -> bool) -> TokenCheck {
        let (check, announce) = {
            let mut window = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let now = self.clock.now();
            if now.saturating_duration_since(window.start) >= Self::WINDOW {
                *window = Window {
                    start: now,
                    wrong: 0,
                    warned: false,
                };
            }
            if window.wrong >= Self::LIMIT {
                let left = (window.start + Self::WINDOW).saturating_duration_since(now);
                let first = !window.warned;
                window.warned = true;
                (TokenCheck::Locked(left), first.then_some(left))
            } else if compare() {
                (TokenCheck::Right, None)
            } else {
                window.wrong += 1;
                (TokenCheck::Wrong, None)
            }
        };
        // Logged after the lock is released: a slow log sink must not hold
        // up the attempts behind this one, which is when it would hurt most.
        if let Some(left) = announce {
            warn_line!(
                "WebUI login locked for {} s after {} wrong tokens",
                ceil_secs(left),
                Self::LIMIT
            );
        }
        check
    }
}

// -- Sessions ----------------------------------------------------------------

/// The key that signs sessions: HMAC-SHA256 under the stored `secret` of
/// `label ‖ 0x00 ‖ token`, taken as the key bytes of a new HMAC key.
///
/// The label keeps this key apart from any other use of the secret, and the
/// zero byte keeps label and token apart.  Without the secret, a token (right
/// or guessed) builds no valid session.
fn session_key(secret: &[u8; 32], token: &str) -> hmac::Key {
    let derive = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut context = hmac::Context::with_key(&derive);
    context.update(SESSION_KEY_LABEL);
    context.update(&[0]);
    context.update(token.as_bytes());
    hmac::Key::new(hmac::HMAC_SHA256, context.sign().as_ref())
}

/// The value of a hex digit, lower case only.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// `text` as exactly `N` bytes of lower-case hex.  Upper case is refused so a
/// session has one spelling only.
fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let digits = text.as_bytes();
    if digits.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

/// Split a session value into the signed text and its MAC, if it has the
/// public shape: `v1.<digits>.<32 hex>.<64 hex>`, lower case.  Says nothing
/// about whether the MAC is right.
fn parse_session(value: &str) -> Option<(&str, [u8; MAC_BYTES])> {
    let (signed, mac) = value.rsplit_once('.')?;
    let mut parts = signed.split('.');
    let (version, issued, nonce) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || version != SESSION_VERSION {
        return None;
    }
    if issued.is_empty() || !issued.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    unhex::<NONCE_BYTES>(nonce)?;
    Some((signed, unhex::<MAC_BYTES>(mac)?))
}

// -- Auth --------------------------------------------------------------------

/// A comparison of a candidate token with the right one.
pub type Comparer = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// Whether `candidate` is the token whose HMAC under `compare_key` is
/// `token_tag`.
///
/// `hmac::verify` compares the two fixed-length tags in constant time, so the
/// step that depends on the secret leaks nothing.  Hashing the candidate takes
/// time in proportion to its length, which the sender already knows.
fn token_matches(compare_key: &hmac::Key, token_tag: &[u8], candidate: &str) -> bool {
    hmac::verify(compare_key, candidate.as_bytes(), token_tag).is_ok()
}

/// Everything the login needs once it is set up.  No `Debug`: it holds the
/// key that signs sessions.
pub struct Auth {
    /// Signs and checks sessions.
    session_key: hmac::Key,
    /// Says whether a token is the right one.
    compare: Comparer,
    random: Arc<dyn RandomSource>,
    /// Failed logins and rejected sessions, which an attacker can cause at
    /// will.
    failure_log: RateLimitedLog,
    /// A proxy that changes `Host`, which every login and logout through it
    /// would otherwise report.
    origin_log: RateLimitedLog,
    lockout: Lockout,
}

impl Auth {
    /// Set up the login for `token` and the stored `secret`.
    ///
    /// Draws a per-process key from `random` to compare tokens with, so the
    /// token itself is not kept (only its HMAC under that key).  Fails when
    /// the random source does; the caller stops startup.
    pub fn new(
        token: &str,
        secret: &[u8; 32],
        random: Arc<dyn RandomSource>,
    ) -> Result<Self, AuthError> {
        Self::build(token, secret, random, Arc::new(TokioClock), None)
    }

    /// Like [`Auth::new`], with the clock and the comparison chosen by the
    /// test: a clock it can move, and a comparison that counts its calls or
    /// takes a few milliseconds to widen any race.  Without `compare` the
    /// real comparison is used.
    #[cfg(test)]
    pub fn with_parts(
        token: &str,
        secret: &[u8; 32],
        random: Arc<dyn RandomSource>,
        clock: Arc<dyn Clock>,
        compare: Option<Comparer>,
    ) -> Result<Self, AuthError> {
        Self::build(token, secret, random, clock, compare)
    }

    fn build(
        token: &str,
        secret: &[u8; 32],
        random: Arc<dyn RandomSource>,
        clock: Arc<dyn Clock>,
        compare: Option<Comparer>,
    ) -> Result<Self, AuthError> {
        // Drawn even when a test brings its own comparison, so that both
        // paths ask the source for the same bytes.
        let compare_key_bytes = random_bytes::<32>(&*random)?;
        let compare = compare.unwrap_or_else(|| {
            let compare_key = hmac::Key::new(hmac::HMAC_SHA256, &compare_key_bytes);
            let token_tag = hmac::sign(&compare_key, token.as_bytes());
            Box::new(move |candidate| token_matches(&compare_key, token_tag.as_ref(), candidate))
        });
        Ok(Auth {
            session_key: session_key(secret, token),
            compare,
            random,
            failure_log: RateLimitedLog::new(),
            origin_log: RateLimitedLog::new(),
            lockout: Lockout::new(clock),
        })
    }

    /// Judge a token presented by a request.  The only way to compare one:
    /// everything that does goes through the lockout.
    pub fn check_token(&self, candidate: &str) -> TokenCheck {
        self.lockout.attempt(|| (self.compare)(candidate))
    }

    /// A new session value, issued at `now`.  Fails when the random source
    /// does, and then no session exists.
    pub fn new_session(&self, now: DateTime<Utc>) -> Result<String, AuthError> {
        let nonce = hex(&random_bytes::<NONCE_BYTES>(&*self.random)?);
        // A clock before 1970 would give a sign no session may carry.
        let issued = now.timestamp().max(0);
        let signed = format!("{SESSION_VERSION}.{issued}.{nonce}");
        let mac = hmac::sign(&self.session_key, signed.as_bytes());
        Ok(format!("{signed}.{}", hex(mac.as_ref())))
    }

    /// Whether `value` is a session this sniffer made, for this token and
    /// this stored secret.  Anything else, however malformed, is `false`.
    ///
    /// Refusing a malformed shape before the MAC is checked reveals only that
    /// the shape is wrong, and the shape is public.
    pub fn session_valid(&self, value: &str) -> bool {
        let Some((signed, mac)) = parse_session(value) else {
            return false;
        };
        hmac::verify(&self.session_key, signed.as_bytes(), &mac).is_ok()
    }

    /// Log a refused login, rate-limited, with the client's address if known
    /// and never what the client sent.  `what` names the kind of failure.
    /// Counts nothing: wrong tokens are counted inside [`Auth::check_token`].
    pub fn note_failure(&self, addr: Option<SocketAddr>, what: &'static str) {
        let from: &dyn fmt::Display = match &addr {
            Some(addr) => addr,
            None => &"unknown",
        };
        self.failure_log
            .warn(format_args!("WebUI login failed from {from}: {what}"));
    }

    /// Log, rate-limited, a login or logout whose `Origin` and `Host` differ.
    pub fn warn_origin(&self, message: impl fmt::Display) {
        self.origin_log.warn(message);
    }
}

// -- Credentials in a request --------------------------------------------------

/// What a request offers as proof it may use the API.
///
/// It has no `==` on purpose: a token must only ever be compared through the
/// lockout, in constant time, never with an ordinary comparison.
#[derive(Clone, Copy)]
pub enum Credential<'a> {
    None,
    /// The token, from `Authorization: Bearer <token>`.
    Bearer(&'a str),
    /// The value of the session cookie.
    Cookie(&'a str),
}

// Written out so that a stray `{:?}` of a request's credential cannot print
// the token or the session.
impl fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Credential::None => "Credential::None",
            Credential::Bearer(_) => "Credential::Bearer(..)",
            Credential::Cookie(_) => "Credential::Cookie(..)",
        })
    }
}

/// The credential a request carries.
///
/// An `Authorization: Bearer` header comes first: a script sends it on
/// purpose, while a browser sends the cookie whatever it is doing.  A request
/// is judged by exactly one credential, so a header that says Bearer but is
/// unusable (no token, a token with spaces, a repeated `Authorization`
/// header) gives [`Credential::None`] rather than falling back to a cookie
/// the same request happens to carry; the client's mistake shows up as a
/// refusal instead of being quietly answered by something it did not mean to
/// present.
///
/// An `Authorization` header of any other scheme is not ours and is ignored.
/// A reverse proxy that asks for its own password (basic authentication in
/// front of the sniffer) makes the browser send `Authorization: Basic …` on
/// every request, the ones that carry the session cookie included; if that
/// header cancelled the cookie, nobody behind such a proxy could ever log in.
/// Without a Bearer header, the session cookie is used, whatever other
/// cookies come with it.
pub fn credential(headers: &HeaderMap) -> Credential<'_> {
    let claims_bearer = headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .any(|value| says_bearer(value.as_bytes()));
    if claims_bearer {
        let mut authorization = headers.get_all(header::AUTHORIZATION).iter();
        return match (authorization.next(), authorization.next()) {
            (Some(only), None) => match bearer_token(only.to_str().ok()) {
                Some(token) => Credential::Bearer(token),
                None => Credential::None,
            },
            _ => Credential::None,
        };
    }
    match session_cookie(headers) {
        Some(value) => Credential::Cookie(value),
        None => Credential::None,
    }
}

/// Whether an `Authorization` value names the Bearer scheme: the word in any
/// case, then the end of the value or a space or tab.  Says nothing about
/// whether a usable token follows.
fn says_bearer(value: &[u8]) -> bool {
    const SCHEME: &[u8] = b"Bearer";
    value.len() >= SCHEME.len()
        && value[..SCHEME.len()].eq_ignore_ascii_case(SCHEME)
        && value
            .get(SCHEME.len())
            .is_none_or(|next| matches!(next, b' ' | b'\t'))
}

/// The token in `Bearer <token>`: the scheme in any case, then one or more
/// spaces, then a token of at least one character without whitespace.
fn bearer_token(value: Option<&str>) -> Option<&str> {
    let (scheme, rest) = value?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = rest.trim_start_matches(' ');
    if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some(token)
}

/// The first non-empty value of the session cookie in any `Cookie` header.
/// An empty value is no credential.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|line| line.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim_matches([' ', '\t']).split_once('='))
        .find(|(name, value)| *name == SESSION_COOKIE && !value.is_empty())
        .map(|(_, value)| value)
}

// -- The HTTP layer -----------------------------------------------------------

/// The body of every 401: what to do, and nothing from the data behind it.
const LOG_IN_FIRST: &str =
    "Log in to the sniffer first: open the WebUI, or send Authorization: Bearer <token>.";

/// Largest body `POST /api/login` reads.  The body is `{"token": "…"}`, so
/// anything near this is not a login, and refusing it early keeps the default
/// 2 MB read off an endpoint that needs no credential.
pub const LOGIN_BODY_LIMIT: usize = 4 * 1024;

/// The longest client-supplied value that goes into a log line.
const LOGGED_VALUE_CHARS: usize = 64;

/// Whether `path` is served without a credential: the WebUI bundle, which
/// holds nothing but the page that asks for one, and the two endpoints that
/// have to be reachable before there is a session.  Everything under `/ws`
/// and `/api/` needs one, `/api/session` included, so that asking it is how
/// a page learns whether it is logged in.
fn is_public(path: &str) -> bool {
    match path {
        "/api/login" | "/api/logout" => true,
        "/ws" => false,
        _ => !(path.starts_with("/ws/") || path.starts_with("/api/")),
    }
}

/// The address the request came from.  Optional so that a request that did
/// not come through a listener (the router tests) simply has none.  Behind a
/// reverse proxy this is the proxy; `X-Forwarded-For` is never trusted.
fn client_addr(req: &Request) -> Option<SocketAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0)
}

fn show(addr: Option<SocketAddr>) -> String {
    addr.map_or_else(|| "unknown".to_string(), |addr| addr.to_string())
}

/// The plain-text 401.  No `WWW-Authenticate` challenge and no redirect: a
/// challenge makes the browser open its own password dialog over the WebUI,
/// and a redirect would turn an API call into an HTML page.
fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, LOG_IN_FIRST).into_response()
}

fn retry_after(left: Duration) -> HeaderValue {
    HeaderValue::from(ceil_secs(left))
}

fn too_many_tokens(left: Duration) -> String {
    format!("Too many wrong tokens; try again in {} s", ceil_secs(left))
}

/// Middleware: let a request through only if it carries a credential.
///
/// * Public paths ([`is_public`]) go straight through.
/// * No credential: 401.
/// * A session cookie that is valid: through.  One that is not: 401.  It is
///   logged but not counted against the guessing limit, because a cookie
///   cannot be built without the stored secret, so it is no way to try tokens.
/// * A Bearer token goes through [`Auth::check_token`]: `Right` passes,
///   `Wrong` is 401, and `Locked` is 429 with `Retry-After` without the token
///   having been compared.
///
/// The prefix test runs on the path as the router sees it.  Odd spellings
/// (`//api/…`, `/%61pi/…`, `/api/../ws`) either match the prefix and are
/// refused, or reach no data route and are answered by the page itself.
pub async fn require_login(State(auth): State<Arc<Auth>>, req: Request, next: Next) -> Response {
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    let addr = client_addr(&req);
    let refusal = match credential(req.headers()) {
        Credential::None => Some(unauthorized()),
        Credential::Cookie(session) if auth.session_valid(session) => None,
        Credential::Cookie(_) => {
            auth.note_failure(addr, "invalid session");
            Some(unauthorized())
        }
        Credential::Bearer(token) => match auth.check_token(token) {
            TokenCheck::Right => None,
            TokenCheck::Wrong => {
                auth.note_failure(addr, "wrong token");
                Some(unauthorized())
            }
            TokenCheck::Locked(left) => Some(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, retry_after(left))],
                    too_many_tokens(left),
                )
                    .into_response(),
            ),
        },
    };
    match refusal {
        None => next.run(req).await,
        Some(refusal) => refusal,
    }
}

/// `GET /api/session` with login on: reaching it at all means the request
/// carried a credential.
pub async fn session_open() -> Response {
    Json(serde_json::json!({ "login": true })).into_response()
}

/// `GET /api/session` with login off.
pub async fn session_closed() -> Response {
    Json(serde_json::json!({ "login": false })).into_response()
}

// -- Login and logout --------------------------------------------------------

/// Whether the text after a media type's first `;` is a list of well-formed
/// `name=value` parameters: a token, `=`, then a token or a quoted string.
///
/// The `mime` crate is lenient about one such form (a value that is empty at
/// the very end, as in `charset=`), and a login must not be accepted on a
/// header that no sender of JSON would write.
fn parameters_well_formed(text: &str) -> bool {
    fn is_token_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
    }
    let mut rest = text.as_bytes();
    while let Some((&b';', after)) = rest.split_first() {
        rest = after;
        while let Some((b' ' | b'\t', after)) = rest.split_first() {
            rest = after;
        }
        if rest.is_empty() {
            return true;
        }
        let name = rest.iter().take_while(|&&b| is_token_char(b)).count();
        if name == 0 || rest.get(name) != Some(&b'=') {
            return false;
        }
        rest = &rest[name + 1..];
        if let Some(quoted) = rest.strip_prefix(b"\"") {
            let mut i = 0;
            loop {
                match quoted.get(i) {
                    Some(b'"') => break,
                    Some(b'\\') if i + 1 < quoted.len() => i += 2,
                    Some(_) => i += 1,
                    None => return false,
                }
            }
            rest = &quoted[i + 1..];
        } else {
            let value = rest.iter().take_while(|&&b| is_token_char(b)).count();
            if value == 0 {
                return false;
            }
            rest = &rest[value..];
        }
    }
    rest.is_empty()
}

/// Whether the request declares exactly one `Content-Type`, and it is
/// `application/json`: any case, any well-formed parameters (`charset`), no
/// `+suffix`.
fn declares_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    let Ok(text) = value.to_str() else {
        return false;
    };
    let Ok(media) = text.parse::<mime::Mime>() else {
        return false;
    };
    let params = text.find(';').map_or("", |at| &text[at..]);
    media.type_() == mime::APPLICATION
        && media.subtype() == mime::JSON
        && media.suffix().is_none()
        && parameters_well_formed(params)
}

/// `value` as it goes into a log line when it came from the client: at most
/// [`LOGGED_VALUE_CHARS`] characters and then `…`, and everything outside
/// printable ASCII shown as `?`.  Works on the raw bytes, so a header that is
/// not valid UTF-8 (which `to_str` would refuse) is still logged safely.
fn log_value(value: &[u8]) -> String {
    let mut shown: String = value
        .iter()
        .take(LOGGED_VALUE_CHARS)
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .collect();
    if value.len() > LOGGED_VALUE_CHARS {
        shown.push('…');
    }
    shown
}

/// The part of an `Origin` after `scheme://`, as written; the whole value if
/// it has no scheme (a sandboxed page sends `null`).
fn origin_authority(origin: &[u8]) -> &[u8] {
    origin
        .windows(3)
        .position(|w| w == b"://")
        .map_or(origin, |at| &origin[at + 3..])
}

/// The checks `POST /api/login` and `POST /api/logout` both begin with: the
/// request must declare JSON, and if it names an `Origin`, that must be the
/// host the request was sent to.  `Err` carries the answer to send.
///
/// Together they keep another web site from logging a browser in or out.  A
/// form, a `no-cors` fetch or a beacon cannot send `application/json`, and a
/// page that does send it first needs a preflight, which this server never
/// answers for `POST` (the CORS layer does not allow it).  The `Origin` rule
/// then catches a request that is JSON and allowed through, but from a page
/// that is not the sniffer's own.  It compares host and port and not the
/// scheme: behind a TLS-terminating proxy the sniffer cannot know which
/// scheme the browser used, and browsers leave default ports out of both
/// `Origin` and `Host`, so `https://h` and `http://h` compare equal.  The
/// JSON rule closes that gap.  `cors_origins` are not accepted here: they let
/// other sites read the API, not log a browser in or out.
///
/// The content type is judged first, so a request that is refused for it never
/// reaches the token.  `X-Forwarded-*` headers are never read.
// The error is the answer itself, built at most once per refused request, so
// its size costs nothing that boxing it would save.
#[allow(clippy::result_large_err)]
pub fn same_host_json(auth: &Auth, headers: &HeaderMap, uri: &Uri) -> Result<(), Response> {
    if !declares_json(headers) {
        return Err(json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON"));
    }
    if !origin_allowed(headers, uri, &[]) {
        let origin = headers
            .get(header::ORIGIN)
            .map_or(&b""[..], |value| origin_authority(value.as_bytes()));
        let host = match headers.get(header::HOST) {
            Some(value) => value.as_bytes(),
            None => uri.authority().map_or(&b""[..], |a| a.as_str().as_bytes()),
        };
        auth.warn_origin(format_args!(
            "WebUI login refused: Origin host {} does not match Host {}; \
             a reverse proxy must pass the browser's Host header",
            log_value(origin),
            log_value(host)
        ));
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "Login refused: the request came from another site, or a reverse proxy \
             changed the Host header",
        ));
    }
    Ok(())
}

/// The body of `POST /api/login`.  No `Debug`: it holds the token.
#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

/// A `Set-Cookie` header for the session cookie with the attributes every
/// answer that touches it carries: not readable by scripts, never sent along
/// with a request that another site causes, for the whole site.  No `Secure`
/// attribute, because the sniffer is often reached over plain HTTP and a
/// browser would not store the cookie then.
fn session_cookie_header(value: &str, extra: &str) -> Option<HeaderValue> {
    let mut header = HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; HttpOnly; SameSite=Strict; Path=/{extra}"
    ))
    .ok()?;
    header.set_sensitive(true);
    Some(header)
}

/// `POST /api/login` with `{"token": "…"}`: 204 and the session cookie, or
/// an error as `{"error": "…"}`.
pub async fn login(State(auth): State<Arc<Auth>>, req: Request) -> Response {
    let addr = client_addr(&req);
    if let Err(refusal) = same_host_json(&auth, req.headers(), req.uri()) {
        return refusal;
    }
    let body = match Bytes::from_request(req, &()).await {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "The request is too large");
        }
        Err(_) => return json_error(StatusCode::BAD_REQUEST, SEND_A_TOKEN),
    };
    let Ok(LoginBody { token }) = serde_json::from_slice(&body) else {
        return json_error(StatusCode::BAD_REQUEST, SEND_A_TOKEN);
    };
    match auth.check_token(&token) {
        TokenCheck::Locked(left) => {
            let mut answer = json_error(StatusCode::TOO_MANY_REQUESTS, too_many_tokens(left));
            answer
                .headers_mut()
                .insert(header::RETRY_AFTER, retry_after(left));
            answer
        }
        TokenCheck::Wrong => {
            auth.note_failure(addr, "wrong token");
            json_error(StatusCode::UNAUTHORIZED, "Wrong token")
        }
        TokenCheck::Right => match auth.new_session(Utc::now()) {
            Ok(session) => match session_cookie_header(&session, "") {
                Some(cookie) => {
                    info!("WebUI login from {}", show(addr));
                    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
                }
                None => session_failure("the session value is not a valid header"),
            },
            Err(e) => session_failure(e),
        },
    }
}

const SEND_A_TOKEN: &str = "Send a JSON object like {\"token\": \"...\"}";

/// The 500 for a right token that could not be given a session.  The cause is
/// logged; the answer says only where to look.
fn session_failure(cause: impl fmt::Display) -> Response {
    error_line!("WebUI login: cannot start a session: {cause}");
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "The sniffer could not start a session; see its log",
    )
}

/// `POST /api/logout`: 204 and a cookie that has expired.  The body is not
/// read; the WebUI sends `{}` only because the JSON rule applies to it too.
pub async fn logout(State(auth): State<Arc<Auth>>, req: Request) -> Response {
    if let Err(refusal) = same_host_json(&auth, req.headers(), req.uri()) {
        return refusal;
    }
    match session_cookie_header("", "; Max-Age=0") {
        Some(cookie) => (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
