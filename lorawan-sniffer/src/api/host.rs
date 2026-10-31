//! Host check: the sniffer answers only requests addressed to it by an IP
//! address, by `localhost`, or by a name the operator listed in
//! `[sniffer.api] allowed_hosts`.
//!
//! The API has no authentication, so the browser's same-origin rule is the
//! only thing keeping other web sites away from the captured traffic.  DNS
//! rebinding defeats that rule: a page on `evil.example` makes the name resolve
//! to the sniffer's address, and from then on the browser treats the sniffer as
//! the page's own origin, so `Origin` and `Host` agree and CORS never applies.
//! What the attacker cannot change is the name: the request still says
//! `Host: evil.example`.  An address typed as an IP literal cannot be rebound
//! (a page at `http://10.0.0.5:8888` really is that address), which is why IP
//! addresses are always allowed and only names need listing.
//!
//! [`check_host`] is a middleware meant to wrap the whole router, so that the
//! WebUI, the REST API and the WebSockets are all covered.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The host names configured in `allowed_hosts`, ready for comparison.
#[derive(Debug, Clone, Default)]
pub struct AllowedHosts {
    /// `["*"]` was configured: the check is off.
    any: bool,
    /// Lower-case, without a trailing dot, which is how requests are compared.
    names: Vec<String>,
}

impl AllowedHosts {
    /// Build the set from the `allowed_hosts` setting.
    ///
    /// Names are compared case-insensitively and without the optional trailing
    /// dot of a fully qualified name, so both sides are brought to that form.
    pub fn new(list: &[String]) -> Self {
        let mut allowed = AllowedHosts::default();
        for entry in list {
            let entry = entry.trim();
            if entry == "*" {
                allowed.any = true;
                continue;
            }
            let name = normalise(entry);
            if !name.is_empty() {
                allowed.names.push(name);
            }
        }
        allowed
    }
}

/// Whether a request addressed to `host` (the value of a `Host` header, with
/// an optional `:port`) may be served.
///
/// Allowed: everything when the check is off; any IPv4 or IPv6 address, with
/// or without a port (IPv6 in brackets, as a `Host` header writes it);
/// `localhost`; and the listed names.  A value that is not a well-formed host
/// is refused.
pub fn host_allowed(host: &str, allowed: &AllowedHosts) -> bool {
    if allowed.any {
        return true;
    }
    match parse_host(host) {
        Some(Target::Ip) => true,
        Some(Target::Name(name)) => name == "localhost" || allowed.names.contains(&name),
        None => false,
    }
}

/// Middleware for `axum::middleware::from_fn_with_state`: refuse requests
/// whose host is not allowed.
///
/// The host is the `Host` header, or, when there is none, the authority of the
/// request URI (an HTTP/2 request carries `:authority` instead of `Host`, and
/// hyper puts it in the URI).  A request with neither is a malformed request
/// and gets 400.  A refusal is 403 as plain text, with `nosniff` so that a
/// browser never renders the echoed host name as HTML, and names the setting
/// to change.
///
/// Refusals are logged at debug level only: the sender decides how many there
/// are, and a warning per request would let any page on the network fill the
/// log.
pub async fn check_host(
    State(allowed): State<Arc<AllowedHosts>>,
    req: Request,
    next: Next,
) -> Response {
    // With the check off even a request without a host goes through.
    if allowed.any {
        return next.run(req).await;
    }

    let refusal = match request_host(&req) {
        Ok(host) if host_allowed(host, &allowed) => None,
        Ok(host) => {
            debug!("Host check refused host {:?}", host);
            Some(refused(host))
        }
        Err(problem) => {
            debug!("Host check refused a request: {}", problem);
            Some(plain_text(StatusCode::BAD_REQUEST, problem.to_string()))
        }
    };

    match refusal {
        None => next.run(req).await,
        Some(response) => response,
    }
}

/// What the request says it is addressed to.
fn request_host(req: &Request) -> Result<&str, &'static str> {
    let mut values = req.headers().get_all(header::HOST).iter();
    if let Some(value) = values.next() {
        // Two Host headers are read differently by different servers and
        // proxies, which is the opening a request smuggler looks for.
        if values.next().is_some() {
            return Err("The request has more than one Host header.");
        }
        return std::str::from_utf8(value.as_bytes())
            .map_err(|_| "The Host header is not valid UTF-8.");
    }
    // `host()` leaves out the port and any user information.
    match req.uri().authority() {
        Some(authority) => Ok(authority.host()),
        None => Err("The request has no Host header."),
    }
}

/// The 403 for a host that is not allowed.
fn refused(host: &str) -> Response {
    let body = match parse_host(host) {
        Some(Target::Name(name)) => format!(
            "Host '{host}' is not allowed. Add \"{name}\" to [sniffer.api] allowed_hosts \
             (SNIFFER_API_ALLOWED_HOSTS in Docker); IP addresses and localhost always work."
        ),
        // Listing a malformed value would not help, so do not suggest it.
        _ => format!(
            "Host '{host}' is not allowed because it is not a valid host name. \
             IP addresses and localhost always work."
        ),
    };
    plain_text(StatusCode::FORBIDDEN, body)
}

fn plain_text(status: StatusCode, body: String) -> Response {
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        body,
    )
        .into_response()
}

/// A host as far as the check cares.
enum Target {
    /// An IPv4 or IPv6 address.
    Ip,
    /// A host name, lower-case and without port or trailing dot.
    Name(String),
}

/// Split a host (as written in a `Host` header) into what it names.  `None`
/// for anything malformed.
fn parse_host(host: &str) -> Option<Target> {
    if host.is_empty() {
        return None;
    }

    // `[v6]` or `[v6]:port`: the only form in which an IPv6 address may be
    // written in a Host header, because its own colons would be taken for a
    // port separator.
    if let Some(rest) = host.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        if !after.is_empty() && !is_port(after.strip_prefix(':')?) {
            return None;
        }
        return inner.parse::<Ipv6Addr>().ok().map(|_| Target::Ip);
    }

    // A bare address: IPv4, or IPv6 without brackets, which no browser sends
    // but which unambiguously names an address.
    if host.parse::<IpAddr>().is_ok() {
        return Some(Target::Ip);
    }

    // Anything else has at most one colon, in front of a port.  More than one
    // would be an IPv6 address that failed to parse, or garbage.
    let name = match host.matches(':').count() {
        0 => host,
        1 => {
            let (name, port) = host.split_once(':')?;
            if !is_port(port) {
                return None;
            }
            name
        }
        _ => return None,
    };

    // Only characters that `allowed_hosts` itself accepts, so that a name
    // the check suggests adding can really be added.
    let name = normalise(name);
    if name.is_empty() || !name.bytes().all(crate::config::is_host_name_byte) {
        return None;
    }
    // The address with a port, or with a trailing dot.
    if name.parse::<Ipv4Addr>().is_ok() {
        return Some(Target::Ip);
    }
    Some(Target::Name(name))
}

/// Digits only and at most five of them.  The value itself is never used.
fn is_port(text: &str) -> bool {
    (1..=5).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// Lower-case, without the single trailing dot of a fully qualified name.
fn normalise(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.strip_suffix('.') {
        Some(stripped) => stripped.to_string(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn hosts(list: &[&str]) -> AllowedHosts {
        let list: Vec<String> = list.iter().map(|host| host.to_string()).collect();
        AllowedHosts::new(&list)
    }

    fn assert_allowed(host: &str, allowed: &AllowedHosts) {
        assert!(host_allowed(host, allowed), "{host:?} should be allowed");
    }

    fn assert_refused(host: &str, allowed: &AllowedHosts) {
        assert!(!host_allowed(host, allowed), "{host:?} should be refused");
    }

    // -- AllowedHosts ---------------------------------------------------------

    #[test]
    fn new_normalises_names() {
        let allowed = hosts(&["Sniffer.Example.COM", "lab.", " padded ", ""]);

        assert!(!allowed.any);
        assert_eq!(allowed.names, ["sniffer.example.com", "lab", "padded"]);
    }

    #[test]
    fn new_recognises_the_star() {
        assert!(hosts(&["*"]).any);
        assert!(hosts(&[" * "]).any);
        assert!(hosts(&["*"]).names.is_empty());
        assert!(!hosts(&[]).any);
        assert!(!hosts(&["example.com"]).any);
    }

    // -- host_allowed ---------------------------------------------------------

    #[test]
    fn ip_addresses_are_always_allowed() {
        let none = hosts(&[]);

        for host in [
            "127.0.0.1",
            "127.0.0.1:8888",
            "192.168.1.20",
            "192.168.1.20:80",
            "192.168.1.20.",
            "0.0.0.0:8888",
            "[::1]",
            "[::1]:8888",
            "[2001:db8::1]",
            "[2001:DB8::1]:443",
            "[::ffff:192.168.1.5]:8888",
            "::1",
            "2001:db8::1",
        ] {
            assert_allowed(host, &none);
        }
    }

    #[test]
    fn localhost_is_always_allowed_in_any_case() {
        let none = hosts(&[]);

        for host in [
            "localhost",
            "LOCALHOST",
            "LocalHost:8888",
            "localhost:80",
            "localhost.",
            "Localhost.:8888",
        ] {
            assert_allowed(host, &none);
        }
    }

    #[test]
    fn listed_names_are_allowed_with_or_without_port_and_dot() {
        let allowed = hosts(&["sniffer.example.com", "Lab-PC."]);

        for host in [
            "sniffer.example.com",
            "sniffer.example.com:8888",
            "sniffer.example.com.",
            "sniffer.example.com.:443",
            "SNIFFER.EXAMPLE.COM",
            "Sniffer.Example.Com:8888",
            "lab-pc",
            "LAB-PC:80",
            "lab-pc.",
        ] {
            assert_allowed(host, &allowed);
        }
    }

    #[test]
    fn unlisted_names_are_refused() {
        let allowed = hosts(&["sniffer.example.com"]);

        for host in [
            "evil.example",
            "evil.example:8888",
            "evil.example.",
            "sniffer.example.org",
            "example.com",
            "xsniffer.example.com",
            "sniffer.example.com.evil.example",
            "evil.example.sniffer.example.com",
            "sniffer.example.com@evil.example",
            "sniffer.example.com/",
            "foo.localhost",
            "localhost.evil.example",
            "localhost@evil.example",
        ] {
            assert_refused(host, &allowed);
            assert_refused(host, &hosts(&[]));
        }
    }

    #[test]
    fn a_listed_name_does_not_open_other_names() {
        let allowed = hosts(&["a.example"]);

        assert_allowed("a.example", &allowed);
        assert_refused("b.example", &allowed);
        // Only the exact name: no suffix or subdomain matching.
        assert_refused("www.a.example", &allowed);
        assert_refused("a.example.b.example", &allowed);
    }

    #[test]
    fn the_star_allows_everything() {
        let any = hosts(&["*"]);

        for host in [
            "evil.example",
            "evil.example:8888",
            "127.0.0.1",
            "localhost",
            "",
            "a b",
            "[::1",
            "::::",
            "host:port",
        ] {
            assert_allowed(host, &any);
        }
    }

    #[test]
    fn malformed_hosts_are_refused() {
        let allowed = hosts(&["sniffer.example.com", "a"]);

        for host in [
            "",
            ":",
            ":80",
            "localhost:",
            "localhost:80x",
            "localhost:-1",
            "localhost:+80",
            "localhost:123456",
            "localhost:80:80",
            "sniffer.example.com:",
            "sniffer.example.com::80",
            "a:b:c",
            "[::1",
            "[::1]x",
            "[::1]:",
            "[::1]:80x",
            "[::1]]",
            "[nope]",
            "[nope]:80",
            "[127.0.0.1]",
            "[]",
            "[",
            "]",
            ".",
            "..",
            ".:80",
            "a b",
            "a\tb",
            "caf\u{e9}.example",
            "http://sniffer.example.com",
            "1.2.3",
            "1.2.3.4.5",
            "256.1.1.1",
            "01.2.3.4",
            "::g",
        ] {
            assert_refused(host, &allowed);
        }
    }

    // -- check_host -----------------------------------------------------------

    fn app(list: &[&str]) -> Router {
        Router::new()
            .route("/", get(|| async { "served" }))
            .fallback(|| async { "fallback" })
            .layer(from_fn_with_state(Arc::new(hosts(list)), check_host))
    }

    async fn send(app: Router, request: axum::http::request::Builder) -> Response {
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn get_with_host(app: Router, uri: &str, host: &str) -> Response {
        send(app, Request::builder().uri(uri).header("host", host)).await
    }

    async fn text_of(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn allowed_hosts_reach_the_handler() {
        for host in [
            "127.0.0.1:8888",
            "[::1]:8888",
            "localhost:8888",
            "LOCALHOST",
            "192.168.1.20",
            "sniffer.example.com",
            "Sniffer.Example.com:8888",
        ] {
            let response = get_with_host(app(&["sniffer.example.com"]), "/", host).await;

            assert_eq!(response.status(), StatusCode::OK, "{host}");
            assert_eq!(text_of(response).await, "served", "{host}");
        }
    }

    #[tokio::test]
    async fn foreign_host_gets_a_plain_text_403_naming_the_setting() {
        let response = get_with_host(app(&[]), "/", "Sniffer.Example.com:8888").await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        // The request's own spelling, then the name to list: lower-case and
        // without the port.
        let body = text_of(response).await;
        assert_eq!(
            body,
            "Host 'Sniffer.Example.com:8888' is not allowed. Add \"sniffer.example.com\" to \
             [sniffer.api] allowed_hosts (SNIFFER_API_ALLOWED_HOSTS in Docker); \
             IP addresses and localhost always work."
        );
    }

    #[tokio::test]
    async fn the_check_covers_every_route_method_and_fallback() {
        let get_other_path = get_with_host(app(&[]), "/no/such/path", "evil.example").await;
        assert_eq!(get_other_path.status(), StatusCode::FORBIDDEN);

        let post = send(
            app(&[]),
            Request::builder()
                .method("POST")
                .uri("/")
                .header("host", "evil.example"),
        )
        .await;
        assert_eq!(post.status(), StatusCode::FORBIDDEN);

        // An allowed host still reaches the fallback.
        let fallback = get_with_host(app(&[]), "/no/such/path", "127.0.0.1:8888").await;
        assert_eq!(text_of(fallback).await, "fallback");
    }

    #[tokio::test]
    async fn dns_rebinding_request_is_refused() {
        // The page at evil.example rebound its name to the sniffer, so the
        // browser sends the page's own origin along with the foreign host.
        let response = send(
            app(&["sniffer.example.com"]),
            Request::builder()
                .uri("/api/messages")
                .header("host", "evil.example:8888")
                .header("origin", "http://evil.example:8888"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let body = text_of(response).await;
        assert!(body.contains("Add \"evil.example\" to [sniffer.api] allowed_hosts"));
    }

    #[tokio::test]
    async fn an_ip_host_passes_whatever_the_origin_says() {
        // The Host check does not judge Origin; that is a separate rule.
        let response = send(
            app(&[]),
            Request::builder()
                .uri("/")
                .header("host", "127.0.0.1:8888")
                .header("origin", "http://evil.example"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_star_turns_the_check_off() {
        for host in ["evil.example:8888", "a b", "[::1"] {
            let response = get_with_host(app(&["*"]), "/", host).await;
            assert_eq!(response.status(), StatusCode::OK, "{host}");
        }

        // Even a request with no host at all, which is not really a host
        // check any more.
        let response = send(app(&["*"]), Request::builder().uri("/")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_uri_authority_decides_when_there_is_no_host_header() {
        // What hyper builds from an HTTP/2 request's :authority.
        for (uri, status) in [
            ("http://evil.example:8888/", StatusCode::FORBIDDEN),
            ("http://evil.example/", StatusCode::FORBIDDEN),
            ("http://user@evil.example/", StatusCode::FORBIDDEN),
            ("http://127.0.0.1:8888/", StatusCode::OK),
            ("http://[::1]:8888/", StatusCode::OK),
            ("http://localhost/", StatusCode::OK),
            ("http://sniffer.example.com:8888/", StatusCode::OK),
            ("http://Sniffer.Example.com/", StatusCode::OK),
        ] {
            let response = send(app(&["sniffer.example.com"]), Request::builder().uri(uri)).await;
            assert_eq!(response.status(), status, "{uri}");
        }

        let response = send(
            app(&[]),
            Request::builder().uri("http://evil.example:8888/"),
        )
        .await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        let body = text_of(response).await;
        assert!(body.contains("Add \"evil.example\" to"), "{body}");
    }

    #[tokio::test]
    async fn a_request_with_neither_host_nor_authority_is_a_bad_request() {
        let response = send(app(&["sniffer.example.com"]), Request::builder().uri("/")).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(text_of(response).await.contains("no Host header"));
    }

    #[tokio::test]
    async fn two_host_headers_are_a_bad_request() {
        let response = send(
            app(&[]),
            Request::builder()
                .uri("/")
                .header("host", "127.0.0.1")
                .header("host", "evil.example"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_host_that_is_not_utf8_is_a_bad_request() {
        let response = send(
            app(&[]),
            Request::builder()
                .uri("/")
                .header("host", HeaderValue::from_bytes(b"caf\xe9.example").unwrap()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(text_of(response).await.contains("UTF-8"));
    }

    #[tokio::test]
    async fn a_malformed_host_is_refused_without_a_suggestion() {
        for host in ["", "a b", "evil.example:80:80", "[::1", "localhost:x"] {
            let response = get_with_host(app(&["sniffer.example.com"]), "/", host).await;

            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host:?}");
            let body = text_of(response).await;
            assert!(body.contains("not a valid host name"), "{host:?}: {body}");
            assert!(!body.contains("Add "), "{host:?}: {body}");
        }
    }
}
