//! Address policy for URLs the OAuth client takes from server responses.
//!
//! OAuth discovery makes the client fetch URLs that a remote party chose: the
//! `resource_metadata` URL in a `WWW-Authenticate` challenge, every issuer in
//! Protected Resource Metadata `authorization_servers`, and the token and
//! registration endpoints in authorization server metadata. Without a check, a
//! malicious MCP server can point a server-side client (a gateway, an agent
//! host) at loopback, cloud metadata, or private-network services.
//! [`OAuthAddressPolicy`] blocks those destinations by default.
//!
//! See [`OAuthAddressPolicy`] for the blocked ranges, the exemptions, and
//! where the policy is enforced.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use super::oauth::OAuthClientError;

// =============================================================================
// Address classification
// =============================================================================

/// The class of an IP address as far as the policy is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressClass {
    /// Globally routable unicast.
    Public,
    /// `127.0.0.0/8`, `::1`.
    Loopback,
    /// RFC 1918, CGNAT `100.64.0.0/10`, IPv6 unique local `fc00::/7`.
    Private,
    /// `169.254.0.0/16` (including cloud instance metadata), `fe80::/10`.
    LinkLocal,
    /// `0.0.0.0/8`, `::`.
    Unspecified,
    /// `224.0.0.0/4`, `ff00::/8`.
    Multicast,
    /// `255.255.255.255`.
    Broadcast,
    /// Documentation, benchmarking, IETF protocol assignments, and other
    /// ranges that are never a legitimate OAuth server.
    Reserved,
}

impl AddressClass {
    fn describe(self) -> &'static str {
        match self {
            Self::Public => "a public address",
            Self::Loopback => "a loopback address",
            Self::Private => "a private-network address (RFC 1918, CGNAT, or IPv6 unique local)",
            Self::LinkLocal => {
                "a link-local address (this range includes cloud instance metadata services)"
            }
            Self::Unspecified => "an unspecified address",
            Self::Multicast => "a multicast address",
            Self::Broadcast => "the broadcast address",
            Self::Reserved => "a reserved, documentation, or benchmarking address",
        }
    }
}

/// Classify an address, looking through IPv6 forms that embed an IPv4
/// address (IPv4-mapped, IPv4-compatible, NAT64, 6to4).
pub(crate) fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> AddressClass {
    let [a, b, c, _] = ip.octets();
    if ip.is_broadcast() {
        AddressClass::Broadcast
    } else if a == 0 {
        AddressClass::Unspecified
    } else if a == 127 {
        AddressClass::Loopback
    } else if a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 100 && (64..=127).contains(&b))
    {
        AddressClass::Private
    } else if a == 169 && b == 254 {
        AddressClass::LinkLocal
    } else if (224..=239).contains(&a) {
        AddressClass::Multicast
    } else if a >= 240
        || matches!(
            (a, b, c),
            (192, 0, 0) | (192, 0, 2) | (192, 88, 99) | (198, 51, 100) | (203, 0, 113)
        )
        || (a == 198 && (b == 18 || b == 19))
    {
        AddressClass::Reserved
    } else {
        AddressClass::Public
    }
}

fn classify_v6(ip: Ipv6Addr) -> AddressClass {
    if ip.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if ip.is_loopback() {
        return AddressClass::Loopback;
    }
    if let Some(v4) = embedded_ipv4(&ip) {
        return classify_v4(v4);
    }
    let s = ip.segments();
    if s[0] & 0xffc0 == 0xfe80 {
        AddressClass::LinkLocal
    } else if s[0] & 0xfe00 == 0xfc00 {
        AddressClass::Private
    } else if s[0] & 0xff00 == 0xff00 {
        AddressClass::Multicast
    } else if (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x3fff && s[1] < 0x1000)
        || (s[0] == 0x2001 && s[1] == 0)
        // Everything outside global unicast 2000::/3 that was not matched
        // above: site-local fec0::/10, discard 100::/64, NAT64 local-use
        // 64:ff9b:1::/48, and unassigned space.
        || s[0] & 0xe000 != 0x2000
    {
        // Documentation (2001:db8::/32, 3fff::/20), Teredo (2001::/32),
        // and non-global space.
        AddressClass::Reserved
    } else {
        AddressClass::Public
    }
}

/// The IPv4 address embedded in an IPv6 address, for the forms that
/// route to or stand for an IPv4 destination.
fn embedded_ipv4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let tail = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    match s {
        // IPv4-mapped ::ffff:0:0/96.
        [0, 0, 0, 0, 0, 0xffff, _, _] => Some(tail),
        // IPv4-translated ::ffff:0:0:0/96 (RFC 2765).
        [0, 0, 0, 0, 0xffff, 0, _, _] => Some(tail),
        // IPv4-compatible ::/96 (deprecated); `::` and `::1` are handled
        // before this is called.
        [0, 0, 0, 0, 0, 0, _, _] => Some(tail),
        // NAT64 well-known prefix 64:ff9b::/96.
        [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Some(tail),
        // 6to4 2002::/16 carries the IPv4 address in bits 16..48.
        [0x2002, high, low, ..] => Some(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low))),
        _ => None,
    }
}

// =============================================================================
// Networks
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IpNetwork {
    addr: IpAddr,
    prefix: u8,
}

impl IpNetwork {
    fn new(addr: IpAddr, prefix: u8) -> Self {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        Self {
            addr,
            prefix: prefix.min(max),
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => prefix_matches(
                u128::from(u32::from(net)),
                u128::from(u32::from(ip)),
                self.prefix,
                32,
            ),
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_matches(u128::from(net), u128::from(ip), self.prefix, 128)
            }
            _ => false,
        }
    }
}

fn prefix_matches(net: u128, ip: u128, prefix: u8, bits: u32) -> bool {
    if prefix == 0 {
        return true;
    }
    let shift = bits - u32::from(prefix);
    (net >> shift) == (ip >> shift)
}

// =============================================================================
// Host lookup
// =============================================================================

pub(crate) type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Resolves a host name to addresses. The system resolver in production;
/// tests substitute a fixed mapping so fake names can resolve to loopback.
pub(crate) trait HostLookup: Send + Sync + 'static {
    fn lookup(&self, host: String) -> LookupFuture;
}

struct SystemLookup;

impl HostLookup for SystemLookup {
    fn lookup(&self, host: String) -> LookupFuture {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        })
    }
}

// =============================================================================
// Policy
// =============================================================================

/// Address policy applied to URLs the OAuth client takes from server
/// responses (SSRF protection).
///
/// On by default. Every OAuth client entry point uses
/// [`OAuthAddressPolicy::default()`] unless configured otherwise:
/// [`OAuthAuthorizationFlowBuilder::address_policy`](super::OAuthAuthorizationFlowBuilder::address_policy),
/// [`OAuthAuthCodeConfig::address_policy`](super::OAuthAuthCodeConfig::address_policy),
/// [`OAuthClientCredentials::discover_with_address_policy`](super::OAuthClientCredentials::discover_with_address_policy),
/// and [`discover_oauth_authorization_with_policy`](super::discover_oauth_authorization_with_policy).
///
/// # What is checked
///
/// URLs that come from the server side: the `resource_metadata` URL in a
/// `WWW-Authenticate` challenge, the authorization server metadata URLs built
/// from each advertised issuer, any redirect those requests follow, and the
/// token and registration endpoints taken from authorization server metadata.
///
/// # Blocked destinations
///
/// - loopback: `127.0.0.0/8`, `::1`, and the names `localhost` and
///   `*.localhost`
/// - link-local: `169.254.0.0/16` (including `169.254.169.254` cloud instance
///   metadata) and `fe80::/10`
/// - private networks: RFC 1918 (`10.0.0.0/8`, `172.16.0.0/12`,
///   `192.168.0.0/16`), CGNAT `100.64.0.0/10`, and IPv6 unique local
///   `fc00::/7`
/// - unspecified (`0.0.0.0/8`, `::`), multicast (`224.0.0.0/4`, `ff00::/8`),
///   and broadcast (`255.255.255.255`)
/// - documentation, benchmarking, and other reserved ranges (`192.0.0.0/24`,
///   `192.0.2.0/24`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`,
///   `240.0.0.0/4`, `2001:db8::/32`, `3fff::/20`, Teredo `2001::/32`, and
///   IPv6 outside `2000::/3`)
///
/// IPv6 forms that embed an IPv4 address (IPv4-mapped `::ffff:a.b.c.d`,
/// IPv4-compatible, NAT64 `64:ff9b::/96`, and 6to4 `2002::/16`) are judged
/// by the embedded IPv4 address.
///
/// # Exemptions
///
/// - **The configured server's own origin.** A URL with the same scheme,
///   host, and port as the MCP server URL the application configured is never
///   checked, whatever it resolves to. That URL came from the application,
///   not from a server response, so a private or local deployment keeps
///   working.
/// - **The configured server's address class.** When the configured MCP
///   server URL itself names a loopback host (`localhost`, `*.localhost`, or a
///   loopback IP literal), loopback destinations are allowed. When it is a
///   private IP literal (RFC 1918, CGNAT, or unique local), private
///   destinations are allowed. A server reachable only on such an address is
///   already inside that network, so this grants it nothing new, and it keeps
///   a local MCP server with its authorization server on another port
///   working. Link-local, unspecified, multicast, broadcast, and reserved
///   destinations are never allowed this way, and the DNS answers for a
///   configured host name never widen the policy.
/// - **The allowlist.** [`allow_host`](Self::allow_host) exempts a host name
///   or IP literal entirely; [`allow_network`](Self::allow_network) and
///   [`allow_cidr`](Self::allow_cidr) permit addresses in a network.
///
/// # Where it is enforced
///
/// - **Before each request**, the URL is checked. IP-literal hosts are
///   checked directly (reqwest connects to them without consulting a DNS
///   resolver). For the reqwest-based discovery functions, host names are
///   resolved and every resolved address must pass.
/// - **At connect time**, the reqwest clients tower-mcp builds by default
///   install [`OAuthAddressResolver`], which applies the policy to every
///   address DNS returns. This closes the gap between the check and the
///   connection (DNS rebinding) and covers host names reached by redirects.
///   The resolver exempts the configured server's host name and allowlisted
///   hosts by name, so at connect time that exemption is not port-specific.
/// - **On redirects**, the default client for [`OAuthAuthorizationCode`]
///   follows redirects and checks each IP-literal target before following
///   it; host-name targets go through the resolver. The other default
///   clients do not follow redirects at all.
///
/// # Custom HTTP clients
///
/// A custom [`OAuthHttpClient`](super::OAuthHttpClient) still gets the
/// pre-request check, but only for IP literals and `localhost` names: the
/// check cannot know how a custom transport resolves names, so it does not
/// resolve them itself. To enforce the policy on resolved addresses, build
/// the client's reqwest client with [`resolver`](Self::resolver) (or use
/// [`ReqwestOAuthHttpClient::with_address_policy`](super::ReqwestOAuthHttpClient::with_address_policy)),
/// and do not follow redirects to unchecked IP literals. A caller-supplied
/// `reqwest::Client` passed to the discovery functions or
/// [`OAuthAuthCodeConfig::http_client`](super::OAuthAuthCodeConfig::http_client)
/// gets the full pre-request check, including name resolution, but not the
/// connect-time resolver or redirect checks unless it was built with them.
///
/// The standalone [`resolve_oauth_client_registration`](super::resolve_oauth_client_registration)
/// helpers take caller-supplied metadata and client and do not apply this
/// policy.
///
/// # Proxies
///
/// The resolver also resolves an HTTP proxy's host name. A proxy on a private
/// address must be allowed with [`allow_host`](Self::allow_host) or
/// [`allow_network`](Self::allow_network).
///
/// # Example
///
/// ```rust
/// use tower_mcp::client::OAuthAddressPolicy;
///
/// # fn example() -> Result<(), tower_mcp::client::OAuthClientError> {
/// // An authorization server on a corporate network.
/// let policy = OAuthAddressPolicy::default()
///     .allow_host("sso.corp.example")
///     .allow_cidr("10.20.0.0/16")?;
///
/// // Opt out entirely (not recommended for server-side clients).
/// let unrestricted = OAuthAddressPolicy::disabled();
/// # let _ = (policy, unrestricted);
/// # Ok(())
/// # }
/// ```
///
/// [`OAuthAuthorizationCode`]: super::OAuthAuthorizationCode
#[derive(Clone)]
pub struct OAuthAddressPolicy {
    enabled: bool,
    allowed_hosts: Vec<String>,
    allowed_networks: Vec<IpNetwork>,
    lookup: Arc<dyn HostLookup>,
}

impl Default for OAuthAddressPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for OAuthAddressPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthAddressPolicy")
            .field("enabled", &self.enabled)
            .field("allowed_hosts", &self.allowed_hosts)
            .field(
                "allowed_networks",
                &self
                    .allowed_networks
                    .iter()
                    .map(|network| format!("{}/{}", network.addr, network.prefix))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl OAuthAddressPolicy {
    /// The default policy: enabled, with an empty allowlist.
    pub fn new() -> Self {
        Self {
            enabled: true,
            allowed_hosts: Vec::new(),
            allowed_networks: Vec::new(),
            lookup: Arc::new(SystemLookup),
        }
    }

    /// A policy that allows every destination.
    ///
    /// This restores the behavior from before the policy existed. Only use it
    /// when the client never talks to an MCP server it does not trust, or
    /// when outbound traffic is already restricted by other means.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::new()
        }
    }

    /// Whether the policy is enforced.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Exempt a host from the policy entirely.
    ///
    /// `host` is a host name (matched case-insensitively, exactly, with no
    /// wildcard) or an IP literal (IPv6 with or without brackets). Requests
    /// to it are allowed whatever it resolves to, on any port.
    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        let host = normalize_host(&host.into());
        if !host.is_empty() && !self.allowed_hosts.contains(&host) {
            self.allowed_hosts.push(host);
        }
        self
    }

    /// Allow destinations in the network `addr/prefix_len`.
    ///
    /// A prefix longer than the address family allows (32 for IPv4, 128 for
    /// IPv6) is treated as the maximum, so it matches the single address.
    /// An IPv4 network also matches that network's IPv4-mapped IPv6 forms.
    pub fn allow_network(mut self, addr: IpAddr, prefix_len: u8) -> Self {
        self.allowed_networks.push(IpNetwork::new(addr, prefix_len));
        self
    }

    /// Allow destinations in a network given in CIDR notation, such as
    /// `"10.20.0.0/16"` or `"fd00::/8"`. A bare address allows that single
    /// address.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthClientError::BuildError`] when `cidr` is not an IP
    /// address optionally followed by `/` and a prefix length that fits the
    /// address family.
    pub fn allow_cidr(self, cidr: &str) -> Result<Self, OAuthClientError> {
        let invalid = || {
            OAuthClientError::BuildError(format!(
                "invalid CIDR `{cidr}` for OAuthAddressPolicy::allow_cidr; expected an address such as `10.20.0.0/16` or `fd00::/8`"
            ))
        };
        let (addr, prefix) = match cidr.trim().split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (cidr.trim(), None),
        };
        let addr: IpAddr = addr
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .map_err(|_| invalid())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_| invalid())?,
            None => max,
        };
        if prefix > max {
            return Err(invalid());
        }
        Ok(self.allow_network(addr, prefix))
    }

    /// A reqwest DNS resolver that applies this policy to every resolved
    /// address, for a client that talks to the MCP server at `server_url`.
    ///
    /// Install it with [`reqwest::ClientBuilder::dns_resolver`]. Names equal
    /// to the server URL's host, and allowlisted hosts, resolve without a
    /// check. reqwest does not consult a resolver for IP-literal hosts, so
    /// pair it with the pre-request check (which the OAuth flows always run)
    /// and do not follow redirects to unchecked IP literals.
    pub fn resolver(&self, server_url: &str) -> OAuthAddressResolver {
        OAuthAddressResolver {
            policy: self.clone(),
            scope: Arc::new(ServerScope::new(server_url)),
        }
    }

    /// Replace the host lookup (tests map fake names to loopback).
    #[cfg(test)]
    pub(crate) fn with_lookup(mut self, lookup: impl HostLookup) -> Self {
        self.lookup = Arc::new(lookup);
        self
    }

    fn network_allows(&self, ip: IpAddr) -> bool {
        let embedded = match ip {
            IpAddr::V6(v6) => embedded_ipv4(&v6).map(IpAddr::V4),
            IpAddr::V4(_) => None,
        };
        self.allowed_networks.iter().any(|network| {
            network.contains(ip) || embedded.is_some_and(|embedded| network.contains(embedded))
        })
    }

    /// Whether `ip` may be reached, or the class that blocks it.
    fn permits(&self, ip: IpAddr, scope: &ServerScope) -> Result<(), AddressClass> {
        if self.network_allows(ip) {
            return Ok(());
        }
        match classify(ip) {
            AddressClass::Public => Ok(()),
            AddressClass::Loopback if scope.trusts_loopback => Ok(()),
            AddressClass::Private if scope.trusts_private => Ok(()),
            class => Err(class),
        }
    }

    /// Everything that can be decided without DNS. Returns the host name
    /// still to resolve, or `None` when the URL is already allowed.
    fn check_static(
        &self,
        url: &reqwest::Url,
        scope: &ServerScope,
    ) -> Result<Option<String>, OAuthClientError> {
        if !self.enabled {
            return Ok(None);
        }
        let Some(host) = url.host() else {
            // No host: the HTTP client rejects the URL on its own.
            return Ok(None);
        };
        let key = host_key(&host);
        let origin = (
            url.scheme().to_string(),
            key.clone(),
            url.port_or_known_default(),
        );
        if scope.origin.as_ref() == Some(&origin) || self.allowed_hosts.contains(&key) {
            return Ok(None);
        }
        let literal = match host {
            url::Host::Ipv4(ip) => IpAddr::V4(ip),
            url::Host::Ipv6(ip) => IpAddr::V6(ip),
            url::Host::Domain(name) if is_localhost_name(name) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            url::Host::Domain(_) => return Ok(Some(key)),
        };
        self.permits(literal, scope)
            .map(|()| None)
            .map_err(|class| blocked_error(&key, format!("it is {}", class.describe())))
    }
}

/// What the configured MCP server URL exempts.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerScope {
    /// `(scheme, host, port)` of the configured server URL.
    origin: Option<(String, String, Option<u16>)>,
    /// Normalized host of the configured server URL.
    host: Option<String>,
    /// The configured host is itself a loopback host.
    trusts_loopback: bool,
    /// The configured host is itself a private IP literal.
    trusts_private: bool,
}

impl ServerScope {
    /// The exemptions implied by `server_url`. An unparseable URL exempts
    /// nothing; discovery reports the malformed URL itself.
    pub(crate) fn new(server_url: &str) -> Self {
        let Ok(url) = reqwest::Url::parse(server_url) else {
            return Self::default();
        };
        let Some(host) = url.host() else {
            return Self::default();
        };
        let key = host_key(&host);
        let class = match host {
            url::Host::Ipv4(ip) => Some(classify(IpAddr::V4(ip))),
            url::Host::Ipv6(ip) => Some(classify(IpAddr::V6(ip))),
            url::Host::Domain(name) if is_localhost_name(name) => Some(AddressClass::Loopback),
            url::Host::Domain(_) => None,
        };
        Self {
            origin: Some((
                url.scheme().to_string(),
                key.clone(),
                url.port_or_known_default(),
            )),
            host: Some(key),
            trusts_loopback: class == Some(AddressClass::Loopback),
            trusts_private: class == Some(AddressClass::Private),
        }
    }
}

/// Pre-request check bound to one configured MCP server URL.
#[derive(Debug, Clone)]
pub(crate) struct AddressGuard {
    policy: OAuthAddressPolicy,
    scope: Arc<ServerScope>,
    resolve_names: bool,
}

impl AddressGuard {
    /// `resolve_names` controls whether host names are resolved and every
    /// address checked (reqwest-based paths), or left to the HTTP client
    /// (custom [`OAuthHttpClient`](super::OAuthHttpClient) implementations,
    /// and the default client whose resolver checks them at connect time).
    pub(crate) fn new(policy: &OAuthAddressPolicy, server_url: &str, resolve_names: bool) -> Self {
        Self {
            policy: policy.clone(),
            scope: Arc::new(ServerScope::new(server_url)),
            resolve_names,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.policy.enabled
    }

    /// Check one request URL against the policy.
    pub(crate) async fn check(&self, url: &str) -> Result<(), OAuthClientError> {
        let Ok(parsed) = reqwest::Url::parse(url) else {
            // The HTTP client reports a malformed URL itself.
            return Ok(());
        };
        let Some(host) = self.policy.check_static(&parsed, &self.scope)? else {
            return Ok(());
        };
        if !self.resolve_names {
            return Ok(());
        }
        let addrs = self
            .policy
            .lookup
            .lookup(host.clone())
            .await
            .map_err(|error| unresolvable_error(&host, &error.to_string()))?;
        if addrs.is_empty() {
            return Err(unresolvable_error(&host, "no addresses returned"));
        }
        for ip in addrs {
            self.policy.permits(ip, &self.scope).map_err(|class| {
                blocked_error(&host, format!("it resolves to {ip}, {}", class.describe()))
            })?;
        }
        Ok(())
    }

    /// A reqwest client builder carrying this guard's policy: the
    /// connect-time resolver and either no redirects or redirects checked
    /// against the policy.
    pub(crate) fn client_builder(&self, follow_redirects: bool) -> reqwest::ClientBuilder {
        let mut builder = reqwest::Client::builder();
        builder = if !follow_redirects {
            builder.redirect(reqwest::redirect::Policy::none())
        } else if self.policy.enabled {
            let policy = self.policy.clone();
            let scope = self.scope.clone();
            builder.redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                match policy.check_static(attempt.url(), &scope) {
                    Ok(_) => attempt.follow(),
                    Err(error) => attempt.error(error),
                }
            }))
        } else {
            builder
        };
        if self.policy.enabled {
            builder = builder.dns_resolver(OAuthAddressResolver {
                policy: self.policy.clone(),
                scope: self.scope.clone(),
            });
        }
        builder
    }
}

/// A [`reqwest::dns::Resolve`] implementation that applies an
/// [`OAuthAddressPolicy`] to every resolved address.
///
/// Create it with [`OAuthAddressPolicy::resolver`]. Resolution fails with an
/// [`OAuthClientError::AddressBlocked`] source error when any address a name
/// resolves to is blocked.
#[derive(Clone)]
pub struct OAuthAddressResolver {
    policy: OAuthAddressPolicy,
    scope: Arc<ServerScope>,
}

impl fmt::Debug for OAuthAddressResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthAddressResolver")
            .field("policy", &self.policy)
            .field("server_host", &self.scope.host)
            .finish()
    }
}

impl reqwest::dns::Resolve for OAuthAddressResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = normalize_host(name.as_str());
        let policy = self.policy.clone();
        let scope = self.scope.clone();
        Box::pin(async move {
            let addrs = policy.lookup.lookup(host.clone()).await?;
            let exempt = !policy.enabled
                || policy.allowed_hosts.contains(&host)
                || scope.host.as_deref() == Some(host.as_str());
            if !exempt {
                for ip in &addrs {
                    policy.permits(*ip, &scope).map_err(|class| {
                        Box::new(blocked_error(
                            &host,
                            format!("it resolves to {ip}, {}", class.describe()),
                        )) as Box<dyn std::error::Error + Send + Sync>
                    })?;
                }
            }
            let addrs: reqwest::dns::Addrs =
                Box::new(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn is_localhost_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost")
}

/// Canonical form of a URL host for comparisons.
fn host_key(host: &url::Host<&str>) -> String {
    match host {
        url::Host::Domain(name) => normalize_host(name),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    }
}

/// Canonical form of a host string: lowercase, no brackets, no trailing
/// dot, and IP literals in their standard text form.
fn normalize_host(host: &str) -> String {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => host,
    }
}

fn blocked_error(host: &str, detail: String) -> OAuthClientError {
    OAuthClientError::AddressBlocked(format!(
        "refusing to connect to `{host}` because {detail}, which server-supplied OAuth URLs may not reach; \
         if `{host}` is trusted, allow it with `OAuthAddressPolicy::allow_host(\"{host}\")` or `allow_cidr`, \
         or opt out with `OAuthAddressPolicy::disabled()`"
    ))
}

fn unresolvable_error(host: &str, reason: &str) -> OAuthClientError {
    OAuthClientError::AddressBlocked(format!(
        "could not resolve `{host}` to check it against the OAuth address policy ({reason}); \
         if `{host}` is trusted, allow it with `OAuthAddressPolicy::allow_host(\"{host}\")`"
    ))
}

/// Find an [`OAuthClientError::AddressBlocked`] in an error's source chain.
///
/// The resolver and the redirect policy report a blocked address through
/// reqwest, which wraps it in its own error; this recovers the actionable
/// message instead of reqwest's generic "error sending request".
pub(crate) fn address_blocked_in(
    error: &(dyn std::error::Error + 'static),
) -> Option<OAuthClientError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(OAuthClientError::AddressBlocked(message)) =
            error.downcast_ref::<OAuthClientError>()
        {
            return Some(OAuthClientError::AddressBlocked(message.clone()));
        }
        current = error.source();
    }
    None
}

#[cfg(test)]
mod tests;
