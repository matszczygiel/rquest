use std::borrow::Cow;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::pin::Pin;

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{collections::HashMap, convert::TryInto, net::SocketAddr};

use crate::connect::{
    BoxedConnectorLayer, BoxedConnectorService, Connector,
    sealed::{Conn, Unnameable},
};
#[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
use crate::cookie;
use crate::core::client::{
    Builder, Client as HyperClient, Http1Builder, Http2Builder, InnerRequest, NetworkScheme,
    NetworkSchemeBuilder, connect::HttpConnector,
};
use crate::core::rt::{TokioExecutor, tokio::TokioTimer};
#[cfg(feature = "hickory-dns")]
use crate::dns::hickory::{HickoryDnsResolver, LookupIpStrategy};
use crate::dns::{DnsResolverWithOverrides, DynResolver, Resolve, gai::GaiResolver};
use crate::error::{BoxError, Error};
use crate::into_url::try_uri;
use crate::proxy::IntoProxy;
use crate::tls::CertificateInput;
use crate::{CertStore, Http1Config, Http2Config, Identity, TlsConfig, conn, error};
use crate::{IntoUrl, Method, Proxy, StatusCode, Url};
use crate::{
    redirect,
    tls::{AlpnProtos, TlsConnector, TlsVersion},
};

use super::decoder::Accepts;
use super::request::{Request, RequestBuilder};
use super::response::Response;
#[cfg(feature = "websocket")]
use super::websocket::WebSocketRequestBuilder;
use super::{Body, EmulationProvider, EmulationProviderFactory};

use arc_swap::{ArcSwap, Guard};
use bytes::Bytes;
use http::{
    HeaderName, Uri, Version,
    header::{
        CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION,
        PROXY_AUTHORIZATION, REFERER, TRANSFER_ENCODING, USER_AGENT,
    },
    uri::Scheme,
};
use pin_project_lite::pin_project;

use tokio::time::Sleep;
use tower::util::BoxCloneSyncServiceLayer;
use tower::{Layer, Service};

macro_rules! impl_debug {
    ($type:ty, { $($field_name:ident),* }) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut debug_struct = f.debug_struct(stringify!($type));
                $(
                    debug_struct.field(stringify!($field_name), &self.$field_name);
                )*
                debug_struct.finish()
            }
        }
    }
}

type HyperResponseFuture = crate::core::client::ResponseFuture;

/// An asynchronous `Client` to make Requests with.
///
/// The Client has various configuration values to tweak, but the defaults
/// are set to what is usually the most commonly desired value. To configure a
/// `Client`, use `Client::builder()`.
///
/// The `Client` holds a connection pool internally, so it is advised that
/// you create one and **reuse** it.
///
/// You do **not** have to wrap the `Client` in an [`Rc`] or [`Arc`] to **reuse** it,
/// because it already uses an [`Arc`] internally.
///
/// [`Rc`]: std::rc::Rc
#[derive(Clone, Debug)]
pub struct Client {
    inner: Arc<ArcSwap<ClientRef>>,
}

/// A `ClientBuilder` can be used to create a `Client` with custom configuration.
#[must_use]
#[derive(Debug)]
pub struct ClientBuilder {
    config: Config,
}

struct Config {
    headers: HeaderMap,
    headers_order: Option<Cow<'static, [HeaderName]>>,
    accepts: Accepts,
    connect_timeout: Option<Duration>,
    connection_verbose: bool,
    pool_idle_timeout: Option<Duration>,
    pool_max_idle_per_host: usize,
    pool_max_size: Option<NonZeroUsize>,
    tcp_keepalive: Option<Duration>,
    proxies: Vec<Proxy>,
    auto_sys_proxy: bool,
    redirect_policy: redirect::Policy,
    referer: bool,
    timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    network_scheme: NetworkSchemeBuilder,
    nodelay: bool,
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    cookie_store: Option<Arc<dyn cookie::CookieStore>>,
    hickory_dns: bool,
    error: Option<Error>,
    dns_overrides: HashMap<String, Vec<SocketAddr>>,
    dns_resolver: Option<Arc<dyn Resolve>>,
    #[cfg(feature = "hickory-dns")]
    dns_strategy: Option<LookupIpStrategy>,
    https_only: bool,
    http2_max_retry_count: usize,
    connector_layers: Option<Vec<BoxedConnectorLayer>>,
    builder: Builder,
    alpn_protos: Option<AlpnProtos>,
    tls_keylog_file: Option<PathBuf>,
    tls_info: bool,
    tls_sni: Option<bool>,
    verify_hostname: Option<bool>,
    identity: Option<Identity>,
    cert_store: Option<CertStore>,
    cert_verification: Option<bool>,
    min_tls_version: Option<TlsVersion>,
    max_tls_version: Option<TlsVersion>,
    tls_config: Option<TlsConfig>,
}

impl_debug!(
    Config,
    {
        headers,
        headers_order,
        accepts,
        connect_timeout,
        connection_verbose,
        pool_idle_timeout,
        pool_max_idle_per_host,
        pool_max_size,
        tcp_keepalive,
        proxies,
        auto_sys_proxy,
        redirect_policy,
        referer,
        timeout,
        read_timeout,
        network_scheme,
        nodelay,
        hickory_dns,
        dns_overrides,
        https_only,
        http2_max_retry_count,
        builder,
        tls_keylog_file,
        tls_info,
        tls_sni,
        verify_hostname,
        cert_verification,
        cert_store,
        alpn_protos,
        min_tls_version,
        max_tls_version,
        tls_config
    }
);

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`.
    ///
    /// This is the same as `Client::builder()`.
    pub fn new() -> ClientBuilder {
        ClientBuilder {
            config: Config {
                error: None,
                headers: HeaderMap::new(),
                headers_order: None,
                accepts: Accepts::default(),
                connect_timeout: None,
                connection_verbose: false,
                pool_idle_timeout: Some(Duration::from_secs(90)),
                pool_max_idle_per_host: usize::MAX,
                pool_max_size: None,
                // TODO: Re-enable default duration once hyper's HttpConnector is fixed
                // to no longer error when an option fails.
                tcp_keepalive: None,
                proxies: Vec::new(),
                auto_sys_proxy: true,
                redirect_policy: redirect::Policy::none(),
                referer: true,
                timeout: None,
                read_timeout: None,
                network_scheme: NetworkScheme::builder(),
                nodelay: true,
                hickory_dns: cfg!(feature = "hickory-dns"),
                #[cfg(feature = "hickory-dns")]
                dns_strategy: None,
                #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
                cookie_store: None,
                dns_overrides: HashMap::new(),
                dns_resolver: None,
                builder: HyperClient::builder(TokioExecutor::new()),
                https_only: false,
                http2_max_retry_count: 2,
                connector_layers: None,
                alpn_protos: None,
                tls_keylog_file: None,
                tls_info: false,
                tls_sni: None,
                verify_hostname: None,
                identity: None,
                cert_store: None,
                cert_verification: None,
                min_tls_version: None,
                max_tls_version: None,
                tls_config: None,
            },
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    ///
    /// # Errors
    ///
    /// This method fails if a TLS backend cannot be initialized, or the resolver
    /// cannot load the system configuration.
    pub fn build(self) -> crate::Result<Client> {
        let mut config = self.config;

        if let Some(err) = config.error {
            return Err(err);
        }

        let mut proxies = config.proxies;
        if config.auto_sys_proxy {
            proxies.push(Proxy::system());
        }
        let proxies_maybe_http_auth = proxies.iter().any(Proxy::maybe_has_http_auth);

        config
            .builder
            .http2_only(matches!(config.alpn_protos, Some(AlpnProtos::HTTP2)))
            .http2_timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(config.pool_idle_timeout)
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .pool_max_size(config.pool_max_size);

        let connector = {
            let mut resolver: Arc<dyn Resolve> = match config.dns_resolver {
                Some(dns_resolver) => dns_resolver,
                #[cfg(feature = "hickory-dns")]
                None if config.hickory_dns => {
                    Arc::new(HickoryDnsResolver::new(config.dns_strategy)?)
                }
                None => Arc::new(GaiResolver::new()),
            };

            if !config.dns_overrides.is_empty() {
                resolver = Arc::new(DnsResolverWithOverrides::new(
                    resolver,
                    config.dns_overrides,
                ));
            }

            let mut http = HttpConnector::new_with_resolver(DynResolver::new(resolver));
            http.set_connect_timeout(config.connect_timeout);

            let tls = {
                let mut tls_config = config.tls_config.unwrap_or_default();

                if let Some(cert_verification) = config.cert_verification {
                    tls_config.cert_verification = cert_verification;
                }

                if let Some(ref tls_keylog_file) = config.tls_keylog_file {
                    tls_config.tls_keylog_file = Some(tls_keylog_file.clone());
                }

                if let Some(alpn_protos) = config.alpn_protos {
                    tls_config.alpn_protos = alpn_protos;
                }

                if let Some(tls_sni) = config.tls_sni {
                    tls_config.tls_sni = tls_sni;
                }

                if let Some(verify_hostname) = config.verify_hostname {
                    tls_config.verify_hostname = verify_hostname;
                }

                if config.cert_store.is_some() {
                    tls_config.cert_store = config.cert_store.clone();
                } else {
                    tls_config.cert_store = Some(CertStore::default());
                }

                if config.identity.is_some() {
                    tls_config.identity = config.identity.clone();
                }

                if config.min_tls_version.is_some() {
                    tls_config.min_tls_version = config.min_tls_version;
                }

                if config.max_tls_version.is_some() {
                    tls_config.max_tls_version = config.max_tls_version;
                }

                TlsConnector::new(tls_config)?
            };

            Connector::builder(http, tls, config.nodelay, config.tls_info)
                .timeout(config.connect_timeout)
                .keepalive(config.tcp_keepalive)
                .verbose(config.connection_verbose)
                .build(config.connector_layers)
        };

        Ok(Client {
            inner: Arc::new(ArcSwap::from_pointee(ClientRef {
                accepts: config.accepts,
                #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
                cookie_store: config.cookie_store,
                hyper: config.builder.build(connector),
                headers: config.headers,
                headers_order: config.headers_order,
                redirect: config.redirect_policy,
                referer: config.referer,
                request_timeout: config.timeout,
                read_timeout: config.read_timeout,
                https_only: config.https_only,
                http2_max_retry_count: config.http2_max_retry_count,
                proxies,
                proxies_maybe_http_auth,
                network_scheme: config.network_scheme,
                alpn_protos: config.alpn_protos,
                tls_keylog_file: config.tls_keylog_file,
                tls_sni: config.tls_sni,
                verify_hostname: config.verify_hostname,
                identity: config.identity,
                cert_store: config.cert_store,
                cert_verification: config.cert_verification,
                min_tls_version: config.min_tls_version,
                max_tls_version: config.max_tls_version,
            })),
        })
    }

    // Higher-level options

    /// Sets the `User-Agent` header to be used by this client.
    ///
    /// # Example
    ///
    /// ```rust
    /// # async fn doc() -> Result<(), rquest::Error> {
    /// // Name your user agent after your app?
    /// static APP_USER_AGENT: &str = concat!(
    ///     env!("CARGO_PKG_NAME"),
    ///     "/",
    ///     env!("CARGO_PKG_VERSION"),
    /// );
    ///
    /// let client = rquest::Client::builder()
    ///     .user_agent(APP_USER_AGENT)
    ///     .build()?;
    /// let res = client.get("https://www.rust-lang.org").send().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn user_agent<V>(mut self, value: V) -> ClientBuilder
    where
        V: TryInto<HeaderValue>,
        V::Error: Into<http::Error>,
    {
        match value.try_into() {
            Ok(value) => {
                self.config.headers.insert(USER_AGENT, value);
            }
            Err(e) => {
                self.config.error = Some(crate::error::builder(e.into()));
            }
        };
        self
    }

    /// Sets the default headers for every request.
    ///
    /// # Example
    ///
    /// ```rust
    /// use rquest::header;
    /// # async fn doc() -> Result<(), rquest::Error> {
    /// let mut headers = header::HeaderMap::new();
    /// headers.insert("X-MY-HEADER", header::HeaderValue::from_static("value"));
    ///
    /// // Consider marking security-sensitive headers with `set_sensitive`.
    /// let mut auth_value = header::HeaderValue::from_static("secret");
    /// auth_value.set_sensitive(true);
    /// headers.insert(header::AUTHORIZATION, auth_value);
    ///
    /// // get a client builder
    /// let client = rquest::Client::builder()
    ///     .default_headers(headers)
    ///     .build()?;
    /// let res = client.get("https://www.rust-lang.org").send().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Override the default headers:
    ///
    /// ```rust
    /// use rquest::header;
    /// # async fn doc() -> Result<(), rquest::Error> {
    /// let mut headers = header::HeaderMap::new();
    /// headers.insert("X-MY-HEADER", header::HeaderValue::from_static("value"));
    ///
    /// // get a client builder
    /// let client = rquest::Client::builder()
    ///     .default_headers(headers)
    ///     .build()?;
    /// let res = client
    ///     .get("https://www.rust-lang.org")
    ///     .header("X-MY-HEADER", "new_value")
    ///     .send()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn default_headers(mut self, headers: HeaderMap) -> ClientBuilder {
        crate::util::replace_headers(&mut self.config.headers, headers);
        self
    }

    /// Change the order in which headers will be sent
    ///
    /// Warning
    ///
    /// The host header needs to be manually inserted if you want to modify its order.
    /// Otherwise it will be inserted by hyper after sorting.
    pub fn headers_order(mut self, order: impl Into<Cow<'static, [HeaderName]>>) -> ClientBuilder {
        std::mem::swap(&mut self.config.headers_order, &mut Some(order.into()));
        self
    }

    /// Enable a persistent cookie store for the client.
    ///
    /// Cookies received in responses will be preserved and included in
    /// additional requests.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cookies")))]
    pub fn cookie_store(mut self, enable: bool) -> ClientBuilder {
        if enable {
            self.cookie_provider(Arc::new(cookie::Jar::default()))
        } else {
            self.config.cookie_store = None;
            self
        }
    }

    /// Set the persistent cookie store for the client.
    ///
    /// Cookies received in responses will be passed to this store, and
    /// additional requests will query this store for cookies.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(feature = "cookies", feature = "cookies-abstract")))
    )]
    pub fn cookie_provider<C: cookie::CookieStore + 'static>(
        mut self,
        cookie_store: Arc<C>,
    ) -> ClientBuilder {
        self.config.cookie_store = Some(cookie_store as _);
        self
    }

    /// Enable auto gzip decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto gzip decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `gzip`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of
    ///   `gzip`, both `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `gzip` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `gzip` feature to be enabled
    #[cfg(feature = "gzip")]
    #[cfg_attr(docsrs, doc(cfg(feature = "gzip")))]
    pub fn gzip(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.gzip = enable;
        self
    }

    /// Enable auto brotli decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto brotli decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `br`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of
    ///   `br`, both `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `brotli` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `brotli` feature to be enabled
    #[cfg(feature = "brotli")]
    #[cfg_attr(docsrs, doc(cfg(feature = "brotli")))]
    pub fn brotli(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.brotli = enable;
        self
    }

    /// Enable auto zstd decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto zstd decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `zstd`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of
    ///   `zstd`, both `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `zstd` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `zstd` feature to be enabled
    #[cfg(feature = "zstd")]
    #[cfg_attr(docsrs, doc(cfg(feature = "zstd")))]
    pub fn zstd(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.zstd = enable;
        self
    }

    /// Enable auto deflate decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto deflate decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `deflate`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if it's headers contain a `Content-Encoding` value that
    ///   equals to `deflate`, both values `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `deflate` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `deflate` feature to be enabled
    #[cfg(feature = "deflate")]
    #[cfg_attr(docsrs, doc(cfg(feature = "deflate")))]
    pub fn deflate(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.deflate = enable;
        self
    }

    /// Disable auto response body zstd decompression.
    ///
    /// This method exists even if the optional `zstd` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use zstd decompression
    /// even if another dependency were to enable the optional `zstd` feature.
    pub fn no_zstd(self) -> ClientBuilder {
        #[cfg(feature = "zstd")]
        {
            self.zstd(false)
        }

        #[cfg(not(feature = "zstd"))]
        {
            self
        }
    }

    /// Disable auto response body gzip decompression.
    ///
    /// This method exists even if the optional `gzip` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use gzip decompression
    /// even if another dependency were to enable the optional `gzip` feature.
    pub fn no_gzip(self) -> ClientBuilder {
        #[cfg(feature = "gzip")]
        {
            self.gzip(false)
        }

        #[cfg(not(feature = "gzip"))]
        {
            self
        }
    }

    /// Disable auto response body brotli decompression.
    ///
    /// This method exists even if the optional `brotli` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use brotli decompression
    /// even if another dependency were to enable the optional `brotli` feature.
    pub fn no_brotli(self) -> ClientBuilder {
        #[cfg(feature = "brotli")]
        {
            self.brotli(false)
        }

        #[cfg(not(feature = "brotli"))]
        {
            self
        }
    }

    /// Disable auto response body deflate decompression.
    ///
    /// This method exists even if the optional `deflate` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use deflate decompression
    /// even if another dependency were to enable the optional `deflate` feature.
    pub fn no_deflate(self) -> ClientBuilder {
        #[cfg(feature = "deflate")]
        {
            self.deflate(false)
        }

        #[cfg(not(feature = "deflate"))]
        {
            self
        }
    }

    // Redirect options

    /// Set a `RedirectPolicy` for this client.
    ///
    /// Default will follow redirects up to a maximum of 10.
    pub fn redirect(mut self, policy: redirect::Policy) -> ClientBuilder {
        self.config.redirect_policy = policy;
        self
    }

    /// Enable or disable automatic setting of the `Referer` header.
    ///
    /// Default is `true`.
    pub fn referer(mut self, enable: bool) -> ClientBuilder {
        self.config.referer = enable;
        self
    }

    // Proxy options

    /// Add a `Proxy` to the list of proxies the `Client` will use.
    ///
    /// # Note
    ///
    /// Adding a proxy will disable the automatic usage of the "system" proxy.
    ///
    /// # Example
    /// ```
    /// use rquest::Client;
    /// use rquest::Proxy;
    ///
    /// let proxy = Proxy::http("http://proxy:8080").unwrap();
    /// let client = Client::builder().proxy(proxy).build().unwrap();
    ///
    /// let client = Client::builder().proxy("http://proxy2:8080").build().unwrap();
    /// ```
    pub fn proxy<P>(mut self, proxy: P) -> ClientBuilder
    where
        P: IntoProxy,
    {
        match proxy.into_proxy() {
            Ok(proxy) => {
                self.config.proxies.push(proxy);
                self.config.auto_sys_proxy = false;
            }
            Err(e) => {
                self.config.error = Some(e);
            }
        }
        self
    }

    /// Clear all `Proxies`, so `Client` will use no proxy anymore.
    ///
    /// # Note
    /// To add a proxy exclusion list, use [crate::proxy::Proxy::no_proxy()]
    /// on all desired proxies instead.
    ///
    /// This also disables the automatic usage of the "system" proxy.
    pub fn no_proxy(mut self) -> ClientBuilder {
        self.config.proxies.clear();
        self.config.auto_sys_proxy = false;
        self
    }

    // Timeout options

    /// Enables a request timeout.
    ///
    /// The timeout is applied from when the request starts connecting until the
    /// response body has finished.
    ///
    /// Default is no timeout.
    pub fn timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.timeout = Some(timeout);
        self
    }

    /// Set a timeout for only the read phase of a `Client`.
    ///
    /// Default is `None`.
    pub fn read_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.read_timeout = Some(timeout);
        self
    }

    /// Set a timeout for only the connect phase of a `Client`.
    ///
    /// Default is `None`.
    ///
    /// # Note
    ///
    /// This **requires** the futures be executed in a tokio runtime with
    /// a tokio timer enabled.
    pub fn connect_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.connect_timeout = Some(timeout);
        self
    }

    /// Set whether connections should emit verbose logs.
    ///
    /// Enabling this option will emit [log][] messages at the `TRACE` level
    /// for read and write operations on connections.
    ///
    /// [log]: https://crates.io/crates/log
    pub fn connection_verbose(mut self, verbose: bool) -> ClientBuilder {
        self.config.connection_verbose = verbose;
        self
    }

    // HTTP options

    /// Set an optional timeout for idle sockets being kept-alive.
    ///
    /// Pass `None` to disable timeout.
    ///
    /// Default is 90 seconds.
    pub fn pool_idle_timeout<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.pool_idle_timeout = val.into();
        self
    }

    /// Sets the maximum idle connection per host allowed in the pool.
    pub fn pool_max_idle_per_host(mut self, max: usize) -> ClientBuilder {
        self.config.pool_max_idle_per_host = max;
        self
    }

    /// Sets the maximum number of connections in the pool.
    pub fn pool_max_size(mut self, max: usize) -> ClientBuilder {
        self.config.pool_max_size = NonZeroUsize::new(max);
        self
    }

    /// Disable keep-alive for the client.
    pub fn no_keepalive(mut self) -> ClientBuilder {
        self.config.pool_max_idle_per_host = 0;
        self.config.tcp_keepalive = None;
        self
    }

    /// Only use HTTP/1.
    pub fn http1_only(mut self) -> ClientBuilder {
        self.config.alpn_protos = Some(AlpnProtos::HTTP1);
        self
    }

    /// Only use HTTP/2.
    pub fn http2_only(mut self) -> ClientBuilder {
        self.config.alpn_protos = Some(AlpnProtos::HTTP2);
        self
    }

    /// Sets the maximum number of safe retries for HTTP/2 connections.
    pub fn http2_max_retry_count(mut self, max: usize) -> ClientBuilder {
        self.config.http2_max_retry_count = max;
        self
    }

    /// Configures the HTTP/1 builder with the provided closure.
    ///
    /// This method allows you to customize the HTTP/1 builder by passing a closure
    /// that modifies the builder. The closure receives a mutable reference to the
    /// HTTP/1 builder, allowing you to set various options.
    ///
    /// # Example
    /// ```
    /// let client = rquest::Client::builder()
    ///     .http1(|http1| {
    ///         http1.http09_responses(true);
    ///     })
    ///     .build()?;
    /// ```
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a mutable reference to an `Http1Builder` and modifies it.
    ///
    /// # Returns
    ///
    /// * `ClientBuilder` - The modified client builder.
    pub fn http1<F>(mut self, f: F) -> ClientBuilder
    where
        F: FnOnce(Http1Builder<'_>),
    {
        f(self.config.builder.http1());
        self
    }

    /// Configures the HTTP/2 builder with the provided closure.
    ///
    /// This method allows you to customize the HTTP/2 builder by passing a closure
    /// that modifies the builder. The closure receives a mutable reference to the
    /// HTTP/2 builder, allowing you to set various options.
    ///
    /// # Example
    /// ```
    /// let client = rquest::Client::builder()
    ///     .http2(|http2| {
    ///         http2.initial_stream_id(3);
    ///     })
    ///     .build()?;
    /// ```
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a mutable reference to an `Http2Builder` and modifies it.
    ///
    /// # Returns
    ///
    /// * `ClientBuilder` - The modified client builder.
    pub fn http2<F>(mut self, f: F) -> ClientBuilder
    where
        F: FnOnce(Http2Builder<'_>),
    {
        f(self.config.builder.http2());
        self
    }

    // TCP options

    /// Set whether sockets have `TCP_NODELAY` enabled.
    ///
    /// Default is `true`.
    pub fn tcp_nodelay(mut self, enabled: bool) -> ClientBuilder {
        self.config.nodelay = enabled;
        self
    }

    /// Bind to a local IP Address.
    ///
    /// # Example
    ///
    /// ```
    /// use std::net::IpAddr;
    /// let local_addr = IpAddr::from([12, 4, 1, 8]);
    /// let client = rquest::Client::builder()
    ///     .local_address(local_addr)
    ///     .build().unwrap();
    /// ```
    pub fn local_address<T>(mut self, addr: T) -> ClientBuilder
    where
        T: Into<Option<IpAddr>>,
    {
        self.config.network_scheme.address(addr);
        self
    }

    /// Set that all sockets are bound to the configured IPv4 or IPv6 address (depending on host's
    /// preferences) before connection.
    pub fn local_addresses<V4, V6>(mut self, ipv4: V4, ipv6: V6) -> ClientBuilder
    where
        V4: Into<Option<Ipv4Addr>>,
        V6: Into<Option<Ipv6Addr>>,
    {
        self.config.network_scheme.addresses(ipv4, ipv6);
        self
    }

    /// Bind to an interface by `SO_BINDTODEVICE`.
    ///
    /// # Example
    ///
    /// ```
    /// let interface = "lo";
    /// let client = rquest::Client::builder()
    ///     .interface(interface)
    ///     .build().unwrap();
    /// ```
    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            target_os = "android",
            target_os = "fuchsia",
            target_os = "illumos",
            target_os = "ios",
            target_os = "linux",
            target_os = "macos",
            target_os = "solaris",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos",
        )))
    )]
    pub fn interface<T>(mut self, interface: T) -> ClientBuilder
    where
        T: Into<Cow<'static, str>>,
    {
        self.config.network_scheme.interface(interface);
        self
    }

    /// Set that all sockets have `SO_KEEPALIVE` set with the supplied duration.
    ///
    /// If `None`, the option will not be set.
    pub fn tcp_keepalive<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.tcp_keepalive = val.into();
        self
    }

    // TLS/HTTP2 emulation options

    /// Configures the client builder to emulation the specified HTTP context.
    ///
    /// This method sets the necessary headers, HTTP/1 and HTTP/2 configurations, and TLS config
    /// to use the specified HTTP context. It allows the client to mimic the behavior of different
    /// versions or setups, which can be useful for testing or ensuring compatibility with various environments.
    ///
    /// # Note
    /// This will overwrite the existing configuration.
    /// You must set emulation before you can perform subsequent HTTP1/HTTP2/TLS fine-tuning.
    ///
    /// # Example
    ///
    /// ```rust
    /// use rquest::{Client, Emulation};
    /// use rquest_util::Emulation;
    ///
    /// let client = Client::builder()
    ///     .emulation(Emulation::Firefox128)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn emulation<P>(mut self, factory: P) -> ClientBuilder
    where
        P: EmulationProviderFactory,
    {
        let mut emulation = factory.emulation();

        if let Some(mut headers) = emulation.default_headers {
            std::mem::swap(&mut self.config.headers, &mut headers);
        }

        if let Some(headers_order) = emulation.headers_order {
            std::mem::swap(&mut self.config.headers_order, &mut Some(headers_order));
        }

        if let Some(http1_config) = emulation.http1_config.take() {
            let builder = self.config.builder.http1();
            apply_http1_config(builder, http1_config);
        }

        if let Some(http2_config) = emulation.http2_config.take() {
            let builder = self.config.builder.http2();
            apply_http2_config(builder, http2_config)
        }

        std::mem::swap(&mut self.config.tls_config, &mut emulation.tls_config);
        self
    }

    /// Configures SSL/TLS certificate pinning for the client.
    ///
    /// This method allows you to specify a set of PEM-encoded certificates that the client
    /// will pin to, ensuring that only these certificates are trusted during SSL/TLS connections.
    /// This provides an additional layer of security by preventing man-in-the-middle (MITM) attacks,
    /// even if a malicious certificate is issued by a trusted Certificate Authority (CA).
    ///
    /// # Parameters
    ///
    /// - `certs`: An iterator of DER-encoded certificates. Each certificate should be provided
    ///   as a byte slice (`&[u8]`).
    pub fn ssl_pinning<'c, I>(mut self, certs: I) -> ClientBuilder
    where
        I: IntoIterator,
        I::Item: Into<CertificateInput<'c>>,
    {
        match CertStore::from_der_certs(certs) {
            Ok(store) => {
                self.config.cert_store = Some(store);
            }
            Err(err) => self.config.error = Some(err),
        }
        self
    }

    /// Sets the identity to be used for client certificate authentication.
    pub fn identity(mut self, identity: Identity) -> ClientBuilder {
        self.config.identity = Some(identity);
        self
    }

    /// Controls the use of certificate validation.
    ///
    /// Defaults to `true`.
    ///
    /// # Warning
    ///
    /// You should think very carefully before using this method. If
    /// invalid certificates are trusted, *any* certificate for *any* site
    /// will be trusted for use. This includes expired certificates. This
    /// introduces significant vulnerabilities, and should only be used
    /// as a last resort.
    pub fn cert_verification(mut self, cert_verification: bool) -> ClientBuilder {
        self.config.cert_verification = Some(cert_verification);
        self
    }

    /// Sets the verify certificate store for the client.
    ///
    /// This method allows you to specify a custom verify certificate store to be used
    /// for TLS connections. By default, the system's verify certificate store is used.
    ///
    /// # Parameters
    ///
    /// - `store`: The verify certificate store to use. This can be a custom implementation
    ///   of the `IntoCertStore` trait or one of the predefined options.
    ///
    /// # Notes
    ///
    /// - Using a custom verify certificate store can be useful in scenarios where you need
    ///   to trust specific certificates that are not included in the system's default store.
    /// - Ensure that the provided verify certificate store is properly configured to avoid
    ///   potential security risks.
    pub fn cert_store(mut self, store: CertStore) -> ClientBuilder {
        self.config.cert_store = Some(store);
        self
    }

    /// Configures the use of Server Name Indication (SNI) when connecting.
    ///
    /// Defaults to `true`.
    pub fn tls_sni(mut self, tls_sni: bool) -> ClientBuilder {
        self.config.tls_sni = Some(tls_sni);
        self
    }

    /// Configures TLS key logging to a file for protocol analysis and debugging
    ///
    /// This method specifies a file where TLS session keys will be written in NSS key log format.
    /// The logged keys can be used by various network analysis tools to decrypt TLS traffic:
    ///
    /// * Wireshark - Import via Preferences > Protocols > TLS > (Pre)-Master-Secret log filename
    /// * mitmproxy - Use with the --ssl-keylog-file option
    /// * tcpdump/tshark - Use with SSLKEYLOGFILE environment variable
    pub fn tls_keylog_file<P>(mut self, tls_keylog_file: P) -> ClientBuilder
    where
        P: Into<PathBuf>,
    {
        self.config.tls_keylog_file = Some(tls_keylog_file.into());
        self
    }

    /// Configures the use of hostname verification when connecting.
    ///
    /// Defaults to `true`.
    ///
    /// # Warning
    ///
    /// You should think very carefully before you use this method. If hostname verification is not
    /// used, *any* valid certificate for *any* site will be trusted for use from any other. This
    /// introduces a significant vulnerability to man-in-the-middle attacks.
    pub fn verify_hostname(mut self, verify_hostname: bool) -> ClientBuilder {
        self.config.verify_hostname = Some(verify_hostname);
        self
    }

    /// Set the minimum required TLS version for connections.
    ///
    /// By default the TLS backend's own default is used.
    pub fn min_tls_version(mut self, version: TlsVersion) -> ClientBuilder {
        self.config.min_tls_version = Some(version);
        self
    }

    /// Set the maximum allowed TLS version for connections.
    ///
    /// By default there's no maximum.
    pub fn max_tls_version(mut self, version: TlsVersion) -> ClientBuilder {
        self.config.max_tls_version = Some(version);
        self
    }

    /// Add TLS information as `TlsInfo` extension to responses.
    ///
    /// # Optional
    ///
    /// feature to be enabled.
    pub fn tls_info(mut self, tls_info: bool) -> ClientBuilder {
        self.config.tls_info = tls_info;
        self
    }

    /// Restrict the Client to be used with HTTPS only requests.
    ///
    /// Defaults to false.
    pub fn https_only(mut self, enabled: bool) -> ClientBuilder {
        self.config.https_only = enabled;
        self
    }

    // DNS options

    /// Enables the `hickory-dns` asynchronous resolver instead of the default threadpool-based `getaddrinfo`.
    ///
    /// By default, if the `hickory-dns` feature is enabled, this option is used.
    ///
    /// # Optional
    ///
    /// Requires the `hickory-dns` feature to be enabled.
    #[cfg(feature = "hickory-dns")]
    #[cfg_attr(docsrs, doc(cfg(feature = "hickory-dns")))]
    pub fn hickory_dns_strategy(mut self, strategy: LookupIpStrategy) -> ClientBuilder {
        self.config.dns_strategy = Some(strategy);
        self
    }

    /// Disables the hickory-dns async resolver.
    ///
    /// This method exists even if the optional `hickory-dns` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use the hickory-dns async resolver
    /// even if another dependency were to enable the optional `hickory-dns` feature.
    #[cfg(feature = "hickory-dns")]
    #[cfg_attr(docsrs, doc(cfg(feature = "hickory-dns")))]
    pub fn no_hickory_dns(mut self) -> ClientBuilder {
        self.config.hickory_dns = false;
        self
    }

    /// Override DNS resolution for specific domains to a particular IP address.
    ///
    /// Warning
    ///
    /// Since the DNS protocol has no notion of ports, if you wish to send
    /// traffic to a particular port you must include this port in the URL
    /// itself, any port in the overridden addr will be ignored and traffic sent
    /// to the conventional port for the given scheme (e.g. 80 for http).
    pub fn resolve(self, domain: &str, addr: SocketAddr) -> ClientBuilder {
        self.resolve_to_addrs(domain, &[addr])
    }

    /// Override DNS resolution for specific domains to particular IP addresses.
    ///
    /// Warning
    ///
    /// Since the DNS protocol has no notion of ports, if you wish to send
    /// traffic to a particular port you must include this port in the URL
    /// itself, any port in the overridden addresses will be ignored and traffic sent
    /// to the conventional port for the given scheme (e.g. 80 for http).
    pub fn resolve_to_addrs(mut self, domain: &str, addrs: &[SocketAddr]) -> ClientBuilder {
        self.config
            .dns_overrides
            .insert(domain.to_string(), addrs.to_vec());
        self
    }

    /// Override the DNS resolver implementation.
    ///
    /// Pass an `Arc` wrapping a trait object implementing `Resolve`.
    /// Overrides for specific names passed to `resolve` and `resolve_to_addrs` will
    /// still be applied on top of this resolver.
    pub fn dns_resolver<R: Resolve + 'static>(mut self, resolver: Arc<R>) -> ClientBuilder {
        self.config.dns_resolver = Some(resolver as _);
        self
    }

    /// Adds a new Tower [`Layer`](https://docs.rs/tower/latest/tower/trait.Layer.html) to the
    /// base connector [`Service`](https://docs.rs/tower/latest/tower/trait.Service.html) which
    /// is responsible for connection establishment.a
    ///
    /// Each subsequent invocation of this function will wrap previous layers.
    ///
    /// If configured, the `connect_timeout` will be the outermost layer.
    ///
    /// Example usage:
    /// ```
    /// use std::time::Duration;
    ///
    /// let client = rquest::Client::builder()
    ///                      // resolved to outermost layer, meaning while we are waiting on concurrency limit
    ///                      .connect_timeout(Duration::from_millis(200))
    ///                      // underneath the concurrency check, so only after concurrency limit lets us through
    ///                      .connector_layer(tower::timeout::TimeoutLayer::new(Duration::from_millis(50)))
    ///                      .connector_layer(tower::limit::concurrency::ConcurrencyLimitLayer::new(2))
    ///                      .build()
    ///                      .unwrap();
    /// ```
    ///
    pub fn connector_layer<L>(mut self, layer: L) -> ClientBuilder
    where
        L: Layer<BoxedConnectorService> + Clone + Send + Sync + 'static,
        L::Service:
            Service<Unnameable, Response = Conn, Error = BoxError> + Clone + Send + Sync + 'static,
        <L::Service as Service<Unnameable>>::Future: Send + 'static,
    {
        let layer = BoxCloneSyncServiceLayer::new(layer);
        self.config
            .connector_layers
            .get_or_insert_default()
            .push(layer);
        self
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Constructs a new `Client`.
    ///
    /// # Panics
    ///
    /// This method panics if a TLS backend cannot be initialized, or the resolver
    /// cannot load the system configuration.
    ///
    /// Use `Client::builder()` if you wish to handle the failure as an `Error`
    /// instead of panicking.
    pub fn new() -> Client {
        ClientBuilder::new().build().expect("Client::new()")
    }

    /// Create a `ClientBuilder` specifically configured for WebSocket connections.
    ///
    /// This method configures the `ClientBuilder` to use HTTP/1.0 only, which is required for certain WebSocket connections.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Convenience method to make a `GET` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// Upgrades the [`RequestBuilder`] to perform a
    /// websocket handshake. This returns a wrapped type, so you must do
    /// this after you set up your request, and just before you send the
    /// request.
    #[cfg(feature = "websocket")]
    pub fn websocket<U: IntoUrl>(&self, url: U) -> WebSocketRequestBuilder {
        WebSocketRequestBuilder::new(self.request(Method::GET, url))
    }

    /// Convenience method to make a `POST` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Convenience method to make a `PUT` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// Convenience method to make a `PATCH` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// Convenience method to make a `DELETE` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    /// Convenience method to make a `HEAD` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    /// Start building a `Request` with the `Method` and `Url`.
    ///
    /// Returns a `RequestBuilder`, which will allow setting headers and
    /// the request body before sending.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        let req = url.into_url().map(move |url| Request::new(method, url));
        RequestBuilder::new(self.clone(), req)
    }

    /// Executes a `Request`.
    ///
    /// A `Request` can be built manually with `Request::new()` or obtained
    /// from a RequestBuilder with `RequestBuilder::build()`.
    ///
    /// You should prefer to use the `RequestBuilder` and
    /// `RequestBuilder::send()`.
    ///
    /// # Errors
    ///
    /// This method fails if there was an error while sending request,
    /// redirect loop was detected or redirect limit was exhausted.
    pub fn execute(&self, request: Request) -> impl Future<Output = Result<Response, Error>> {
        self.execute_request(request)
    }

    pub(super) fn execute_request(&self, req: Request) -> Pending {
        let (
            method,
            url,
            mut headers,
            headers_order,
            body,
            timeout,
            read_timeout,
            version,
            redirect,
            _allow_compression,
            network_scheme,
            protocol,
        ) = req.pieces();

        // get the scheme of the URL
        let scheme = url.scheme();

        // check if the scheme is supported
        if scheme != "http" && scheme != "https" {
            return Pending::new_err(error::url_bad_scheme(url));
        }

        let client = self.inner.load();

        // check if we're in https_only mode and check the scheme of the current URL
        if client.https_only && scheme != "https" {
            return Pending::new_err(error::url_bad_scheme(url));
        }

        // insert default headers in the request headers
        // without overwriting already appended headers.
        for name in client.headers.keys() {
            if !headers.contains_key(name) {
                for value in client.headers.get_all(name) {
                    headers.append(name, value.clone());
                }
            }
        }

        // add cookies from the cookie store.
        #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
        if let Some(cookie_store) = client.cookie_store.as_ref() {
            if !headers.contains_key(crate::header::COOKIE) {
                add_cookie_header(cookie_store, &mut headers, &url);
            }
        }

        // add accept-encoding header
        #[cfg(any(
            feature = "gzip",
            feature = "brotli",
            feature = "zstd",
            feature = "deflate"
        ))]
        if _allow_compression {
            add_accpet_encoding_header(&client.accepts, &mut headers);
        }

        // parse Uri from the Url
        let uri = match try_uri(&url) {
            Some(uri) => uri,
            None => return Pending::new_err(error::url_bad_uri(url)),
        };

        // reuse the body if possible
        let (reusable, body) = match body {
            Some(body) => {
                let (reusable, body) = body.try_reuse();
                (Some(reusable), body)
            }
            None => (None, Body::empty()),
        };

        client.proxy_auth(&uri, &mut headers);

        let headers_order = headers_order.or_else(|| client.headers_order.clone());

        let network_scheme = client.network_scheme(&uri, network_scheme);

        let in_flight = {
            let res = InnerRequest::builder()
                .uri(uri)
                .method(method.clone())
                .headers(headers.clone())
                .headers_order(headers_order.as_deref())
                .version(version)
                .extension(protocol.clone())
                .network_scheme(network_scheme.clone())
                .body(body);

            match res {
                Ok(req) => client.hyper.request(req),
                Err(err) => return Pending::new_err(error::builder(err)),
            }
        };

        let total_timeout = timeout
            .or(client.request_timeout)
            .map(tokio::time::sleep)
            .map(Box::pin);
        let read_timeout = read_timeout.or(client.read_timeout);
        let read_timeout_fut = read_timeout.map(tokio::time::sleep).map(Box::pin);

        Pending {
            inner: PendingInner::Request(PendingRequest {
                method,
                url,
                headers,
                headers_order,
                body: reusable,
                version,
                protocol,
                urls: Vec::new(),
                http2_retry_count: 0,
                http2_max_retry_count: client.http2_max_retry_count,
                redirect,
                network_scheme,
                client,
                in_flight,
                total_timeout,
                read_timeout_fut,
                read_timeout,
            }),
        }
    }
}

impl Client {
    /// Retrieves the `User-Agent` header for this client.
    ///
    /// This method returns the `User-Agent` header value if it is set for this client.
    ///
    /// # Returns
    ///
    /// An `Option<HeaderValue>` containing the `User-Agent` header value if it is set, or `None` if it is not.
    #[inline]
    pub fn user_agent(&self) -> Option<HeaderValue> {
        self.inner.load().headers.get(USER_AGENT).cloned()
    }

    /// Retrieves a headers for this client.
    ///
    /// This method returns a `HeaderMap` containing the headers for this client.
    /// Note that this operation involves cloning the headers, which can be
    /// expensive if the header map is large.
    ///
    /// # Returns
    ///
    /// A `HeaderMap` containing the headers for this client.
    #[inline]
    pub fn headers(&self) -> HeaderMap {
        self.inner.load().headers.clone()
    }

    /// Returns a `String` of the header-value of all `Cookie` in a `Url`.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    pub fn get_cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.inner
            .load()
            .cookie_store
            .as_ref()
            .and_then(|cookie_store| cookie_store.cookies(url))
    }

    /// Injects a 'Cookie' into the 'CookieStore' for the specified URL.
    ///
    /// This method accepts a collection of cookies, which can be either an owned
    /// vector (`Vec<HeaderValue>`) or a reference to a slice (`&[HeaderValue]`).
    /// It will convert the collection into an iterator and pass it to the
    /// `cookie_store` for processing.
    ///
    /// # Parameters
    /// - `url`: The URL associated with the cookies to be set.
    /// - `cookies`: A collection of `HeaderValue` items, either by reference or owned.
    ///
    /// This method ensures that cookies are only set if at least one cookie
    /// exists in the collection.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    pub fn set_cookies<C>(&self, url: &Url, cookies: C)
    where
        C: AsRef<[HeaderValue]>,
    {
        if let Some(ref cookie_store) = self.inner.load().cookie_store {
            let mut cookies = cookies.as_ref().iter().peekable();
            if cookies.peek().is_some() {
                cookie_store.set_cookies(url, &mut cookies);
            }
        }
    }

    /// Inserts a cookie into the `CookieStore` for the specified URL.
    ///
    /// # Note
    ///
    /// This method requires the `cookies` feature to be enabled.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    pub fn set_cookie<'c, C>(&self, url: &Url, cookie: C)
    where
        C: cookie::IntoCookie + 'c,
    {
        if let Some(ref cookie_store) = self.inner.load().cookie_store {
            cookie_store.set_cookie(url, &cookie);
        }
    }

    /// Removes a cookie from the `CookieStore` for the specified URL.
    ///
    /// This method deletes a cookie with the given name from the client's `CookieStore`
    /// for the specified URL. It can be useful in scenarios where you want to remove
    /// specific cookies to reset the client's state or ensure that certain cookies are
    /// not sent with subsequent requests.
    ///
    /// # Parameters
    ///
    /// - `url`: The URL associated with the cookie to be removed.
    /// - `name`: The name of the cookie to be removed.
    ///
    /// # Note
    ///
    /// This method requires the `cookies` feature to be enabled.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    pub fn remove_cookie(&self, url: &Url, name: &str) {
        if let Some(ref cookie_store) = self.inner.load().cookie_store {
            cookie_store.remove(url, name);
        }
    }

    /// Clears all cookies from the `CookieStore`.
    ///
    /// This method removes all cookies stored in the client's `CookieStore`.
    /// It can be useful in scenarios where you want to reset the client's state
    /// or ensure that no cookies are sent with subsequent requests.
    ///
    /// # Note
    ///
    /// This method requires the `cookies` feature to be enabled.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    pub fn clear_cookies(&self) {
        if let Some(ref cookie_store) = self.inner.load().cookie_store {
            cookie_store.clear();
        }
    }

    /// Returns a `ClientUpdate` instance to modify the internal state of the `Client`.
    ///
    /// This method allows you to obtain a `ClientUpdate` instance, which provides methods
    /// to mutate the internal state of the `Client`. This is useful when you need to modify
    /// the client's configuration or state after it has been created.
    ///
    /// # Returns
    ///
    /// A `ClientUpdate<'_>` instance that can be used to modify the internal state of the `Client`.
    ///
    /// # Example
    ///
    /// ```rust
    /// let client = rquest::Client::new();
    /// client.update()
    ///     .headers(|headers| {
    ///         headers.insert("X-Custom-Header", HeaderValue::from_static("value"));
    ///     })
    ///     .apply()
    ///     .unwrap();
    /// ```
    #[inline]
    pub fn update(&self) -> ClientUpdate<'_> {
        ClientUpdate {
            inner: self.inner.as_ref(),
            current: (**self.inner.load()).clone(),
            emulation: None,
        }
    }

    /// Clones the `Client` into a new instance.
    ///
    /// This method creates a new instance of the `Client` by cloning its internal state.
    /// The cloned client will have the same configuration and state as the original client,
    /// but it will be a separate instance that can be used independently.
    /// Note that this will still share the connection pool with the original `Client`.
    ///
    /// # Example
    ///
    /// ```rust
    /// let client = rquest::Client::new();
    /// let cloned_client = client.cloned();
    /// // Use the cloned client independently
    /// ```
    #[inline]
    pub fn cloned(&self) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee((**self.inner.load()).clone())),
        }
    }
}

impl tower_service::Service<Request> for Client {
    type Response = Response;
    type Error = Error;
    type Future = Pending;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.execute_request(req)
    }
}

impl tower_service::Service<Request> for &'_ Client {
    type Response = Response;
    type Error = Error;
    type Future = Pending;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.execute_request(req)
    }
}

#[derive(Clone)]
struct ClientRef {
    accepts: Accepts,
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    cookie_store: Option<Arc<dyn cookie::CookieStore>>,
    headers: HeaderMap,
    headers_order: Option<Cow<'static, [HeaderName]>>,
    hyper: HyperClient<Connector, super::Body>,
    redirect: redirect::Policy,
    referer: bool,
    request_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    https_only: bool,
    http2_max_retry_count: usize,
    proxies: Vec<Proxy>,
    proxies_maybe_http_auth: bool,
    network_scheme: NetworkSchemeBuilder,
    alpn_protos: Option<AlpnProtos>,
    tls_keylog_file: Option<PathBuf>,
    tls_sni: Option<bool>,
    verify_hostname: Option<bool>,
    identity: Option<Identity>,
    cert_store: Option<CertStore>,
    cert_verification: Option<bool>,
    min_tls_version: Option<TlsVersion>,
    max_tls_version: Option<TlsVersion>,
}

impl ClientRef {
    #[inline]
    fn proxy_auth(&self, dst: &Uri, headers: &mut HeaderMap) {
        if !self.proxies_maybe_http_auth {
            return;
        }

        // Only set the header here if the destination scheme is 'http',
        // since otherwise, the header will be included in the CONNECT tunnel
        // request instead.
        if dst.scheme() != Some(&Scheme::HTTP) {
            return;
        }

        if headers.contains_key(PROXY_AUTHORIZATION) {
            return;
        }

        // Find the first proxy that matches the destination URI
        // If a matching proxy provides an HTTP basic auth header, insert it into the headers
        for proxy in self.proxies.iter() {
            if proxy.is_match(dst) {
                if let Some(header) = proxy.http_basic_auth(dst) {
                    headers.insert(PROXY_AUTHORIZATION, header);
                }

                if let Some(http_headers) = proxy.http_headers() {
                    headers.extend(http_headers);
                }

                break;
            }
        }
    }

    #[inline]
    fn network_scheme(&self, uri: &Uri, default: NetworkScheme) -> NetworkScheme {
        if matches!(default, NetworkScheme::Default) {
            let mut builder = self.network_scheme.clone();

            // iterate over the client's proxies and use the first valid one
            for proxy in self.proxies.iter() {
                if let Some(proxy_scheme) = proxy.intercept(uri) {
                    builder.proxy_scheme(proxy_scheme);
                }
            }

            return builder.build();
        }

        default
    }
}

impl_debug!(ClientRef,{
    accepts,
    headers,
    headers_order,
    hyper,
    redirect,
    referer,
    request_timeout,
    read_timeout,
    https_only,
    http2_max_retry_count,
    proxies,
    network_scheme,
    cert_verification
});

/// A mutable reference to a `ClientRef`.
///
/// This struct provides methods to mutate the state of a `ClientRef`.
#[derive(Debug)]
pub struct ClientUpdate<'c> {
    inner: &'c ArcSwap<ClientRef>,
    current: ClientRef,
    emulation: Option<EmulationProvider>,
}

impl<'c> ClientUpdate<'c> {
    /// Modifies the headers for this client using the provided closure.
    #[inline]
    pub fn headers<F>(mut self, f: F) -> ClientUpdate<'c>
    where
        F: FnOnce(&mut HeaderMap),
    {
        f(&mut self.current.headers);
        self
    }

    /// Sets the headers order for this client.
    #[inline]
    pub fn headers_order<T>(mut self, order: T) -> ClientUpdate<'c>
    where
        T: Into<Cow<'static, [HeaderName]>>,
    {
        std::mem::swap(&mut self.current.headers_order, &mut Some(order.into()));
        self
    }

    /// Set the cookie provider for this client.
    #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
    #[inline]
    pub fn cookie_provider<C>(mut self, cookie_store: Arc<C>) -> ClientUpdate<'c>
    where
        C: cookie::CookieStore + 'static,
    {
        std::mem::swap(&mut self.current.cookie_store, &mut Some(cookie_store as _));
        self
    }

    /// Set that all sockets are bound to the configured address before connection.
    #[inline]
    pub fn local_address<T>(mut self, addr: T) -> ClientUpdate<'c>
    where
        T: Into<Option<IpAddr>>,
    {
        self.current.network_scheme.address(addr.into());
        self
    }

    /// Set that all sockets are bound to the configured IPv4 or IPv6 address
    /// (depending on host's preferences) before connection.
    #[inline]
    pub fn local_addresses<V4, V6>(mut self, ipv4: V4, ipv6: V6) -> ClientUpdate<'c>
    where
        V4: Into<Option<Ipv4Addr>>,
        V6: Into<Option<Ipv6Addr>>,
    {
        self.current.network_scheme.addresses(ipv4, ipv6);
        self
    }

    /// Bind to an interface by `SO_BINDTODEVICE`.
    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[inline]
    pub fn interface<T>(mut self, interface: T) -> ClientUpdate<'c>
    where
        T: Into<Cow<'static, str>>,
    {
        self.current.network_scheme.interface(interface);
        self
    }

    /// Sets the proxies for this client in a thread-safe manner and returns the old proxies.
    ///
    /// This method allows you to set the proxies for the client, ensuring thread safety. It will
    /// replace the current proxies with the provided ones and return the old proxies, if any.
    #[inline]
    pub fn proxies<P>(mut self, proxies: P) -> ClientUpdate<'c>
    where
        P: IntoIterator,
        P::Item: Into<Proxy>,
    {
        let proxies = proxies.into_iter().map(Into::into).collect::<Vec<Proxy>>();
        self.current.proxies_maybe_http_auth = proxies.iter().any(Proxy::maybe_has_http_auth);
        self.current.proxies = proxies;
        self
    }

    /// Clears the proxies for this client in a thread-safe manner and returns the old proxies.
    ///
    /// This method allows you to clear the proxies for the client, ensuring thread safety. It will
    /// remove the current proxies and return the old proxies, if any.
    #[inline]
    pub fn unset_proxies(mut self) -> ClientUpdate<'c> {
        self.current.proxies.clear();
        self.current.proxies_maybe_http_auth = false;
        self
    }

    /// Configures the client to emulation the specified HTTP context.
    ///
    /// This method sets the necessary headers, HTTP/1 and HTTP/2 configurations, and TLS config
    /// to use the specified HTTP context. It allows the client to mimic the behavior of different
    /// versions or setups, which can be useful for testing or ensuring compatibility with various environments.
    ///
    /// The configuration set by this method will have the highest priority, overriding any other
    /// config that may have been previously set.
    #[inline]
    pub fn emulation<P>(mut self, factory: P) -> ClientUpdate<'c>
    where
        P: EmulationProviderFactory,
    {
        let emulation = factory.emulation();
        self.emulation = Some(emulation);
        self
    }

    /// Applies the changes made to the `ClientUpdate` to the `Client`.
    #[inline]
    pub fn apply(self) -> Result<(), Error> {
        let mut current = self.current;

        if let Some(emulation) = self.emulation {
            if let Some(mut headers) = emulation.default_headers {
                std::mem::swap(&mut current.headers, &mut headers);
            }

            if let Some(headers_order) = emulation.headers_order {
                std::mem::swap(&mut current.headers_order, &mut Some(headers_order));
            }

            if let Some(http1_config) = emulation.http1_config {
                apply_http1_config(current.hyper.http1(), http1_config);
            }

            if let Some(http2_config) = emulation.http2_config {
                apply_http2_config(current.hyper.http2(), http2_config);
            }

            if let Some(mut tls_config) = emulation.tls_config {
                if let Some(alpn_protos) = current.alpn_protos {
                    tls_config.alpn_protos = alpn_protos;
                }

                if let Some(tls_sni) = current.tls_sni {
                    tls_config.tls_sni = tls_sni;
                }

                if let Some(verify_hostname) = current.verify_hostname {
                    tls_config.verify_hostname = verify_hostname;
                }

                if let Some(cert_verification) = current.cert_verification {
                    tls_config.cert_verification = cert_verification;
                }

                if current.tls_keylog_file.is_some() {
                    tls_config.tls_keylog_file = current.tls_keylog_file.clone();
                }

                if current.identity.is_some() {
                    tls_config.identity = current.identity.clone();
                }

                if current.cert_store.is_some() {
                    tls_config.cert_store = current.cert_store.clone();
                }

                if current.min_tls_version.is_some() {
                    tls_config.min_tls_version = current.min_tls_version;
                }

                if current.max_tls_version.is_some() {
                    tls_config.max_tls_version = current.max_tls_version;
                }

                let connector = TlsConnector::new(tls_config)?;
                current.hyper.connector_mut().set_connector(connector);
            }
        }

        self.inner.store(Arc::new(current));
        Ok(())
    }
}

pin_project! {
    pub struct Pending {
        #[pin]
        inner: PendingInner,
    }
}

enum PendingInner {
    Request(PendingRequest),
    Error(Option<Error>),
}

pin_project! {
    struct PendingRequest {
        method: Method,
        url: Url,
        headers: HeaderMap,
        headers_order: Option<Cow<'static, [HeaderName]>>,
        body: Option<Option<Bytes>>,
        version: Option<Version>,
        protocol: Option<crate::core::ext::Protocol>,
        urls: Vec<Url>,
        http2_retry_count: usize,
        http2_max_retry_count: usize,
        redirect: Option<redirect::Policy>,
        network_scheme: NetworkScheme,
        client: Guard<Arc<ClientRef>>,
        #[pin]
        in_flight: HyperResponseFuture,
        #[pin]
        total_timeout: Option<Pin<Box<Sleep>>>,
        #[pin]
        read_timeout_fut: Option<Pin<Box<Sleep>>>,
        read_timeout: Option<Duration>,
    }
}

impl PendingRequest {
    #[inline]
    fn in_flight(self: Pin<&mut Self>) -> Pin<&mut HyperResponseFuture> {
        self.project().in_flight
    }

    #[inline]
    fn total_timeout(self: Pin<&mut Self>) -> Pin<&mut Option<Pin<Box<Sleep>>>> {
        self.project().total_timeout
    }

    #[inline]
    fn read_timeout(self: Pin<&mut Self>) -> Pin<&mut Option<Pin<Box<Sleep>>>> {
        self.project().read_timeout_fut
    }

    #[inline]
    fn urls(self: Pin<&mut Self>) -> &mut Vec<Url> {
        self.project().urls
    }

    #[inline]
    fn headers(self: Pin<&mut Self>) -> &mut HeaderMap {
        self.project().headers
    }

    fn retry_error(mut self: Pin<&mut Self>, err: &(dyn std::error::Error + 'static)) -> bool {
        if !is_retryable_error(err) {
            return false;
        }

        trace!("can retry {:?}", err);

        let body = match self.body {
            Some(Some(ref body)) => Body::reusable(body.clone()),
            Some(None) => {
                debug!("error was retryable, but body not reusable");
                return false;
            }
            None => Body::empty(),
        };

        if self.http2_retry_count >= self.http2_max_retry_count {
            trace!("retry count too high");
            return false;
        }
        self.http2_retry_count += 1;

        let uri = match try_uri(&self.url) {
            Some(uri) => uri,
            None => {
                debug!("a parsed Url should always be a valid Uri: {}", self.url);
                return false;
            }
        };

        *self.as_mut().in_flight().get_mut() = {
            let res = InnerRequest::builder()
                .uri(uri)
                .method(self.method.clone())
                .headers(self.headers.clone())
                .headers_order(self.headers_order.as_deref())
                .version(self.version)
                .extension(self.protocol.clone())
                .network_scheme(self.network_scheme.clone())
                .body(body);

            if let Ok(req) = res {
                self.client.hyper.request(req)
            } else {
                trace!("error request build");
                return false;
            }
        };

        true
    }
}

impl Pending {
    pub(super) fn new_err(err: Error) -> Pending {
        Pending {
            inner: PendingInner::Error(Some(err)),
        }
    }

    fn inner(self: Pin<&mut Self>) -> Pin<&mut PendingInner> {
        self.project().inner
    }
}

impl Future for Pending {
    type Output = Result<Response, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.inner();
        match inner.get_mut() {
            PendingInner::Request(req) => Pin::new(req).poll(cx),
            PendingInner::Error(err) => Poll::Ready(Err(err
                .take()
                .unwrap_or_else(|| error::request("Pending error polled more than once")))),
        }
    }
}

impl Future for PendingRequest {
    type Output = Result<Response, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(delay) = self.as_mut().total_timeout().as_mut().as_pin_mut() {
            if let Poll::Ready(()) = delay.poll(cx) {
                return Poll::Ready(Err(
                    error::request(error::TimedOut).with_url(self.url.clone())
                ));
            }
        }

        if let Some(delay) = self.as_mut().read_timeout().as_mut().as_pin_mut() {
            if let Poll::Ready(()) = delay.poll(cx) {
                return Poll::Ready(Err(
                    error::request(error::TimedOut).with_url(self.url.clone())
                ));
            }
        }

        loop {
            let res = {
                let r = self.as_mut().in_flight().get_mut();
                match Pin::new(r).poll(cx) {
                    Poll::Ready(Err(e)) => {
                        if self.as_mut().retry_error(&e) {
                            continue;
                        }
                        return Poll::Ready(Err(error::request(e).with_url(self.url.clone())));
                    }
                    Poll::Ready(Ok(res)) => res.map(super::body::boxed),
                    Poll::Pending => return Poll::Pending,
                }
            };

            #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
            {
                if let Some(cookie_store) = self.client.cookie_store.as_ref() {
                    let mut cookies =
                        cookie::extract_response_cookie_headers(res.headers()).peekable();
                    if cookies.peek().is_some() {
                        cookie_store.set_cookies(&self.url, &mut cookies);
                    }
                }
            }

            let previous_method = self.method.clone();

            let should_redirect = match res.status() {
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => {
                    self.body = None;
                    for header in &[
                        TRANSFER_ENCODING,
                        CONTENT_ENCODING,
                        CONTENT_TYPE,
                        CONTENT_LENGTH,
                    ] {
                        self.headers.remove(header);
                    }

                    match self.method {
                        Method::GET | Method::HEAD => {}
                        _ => {
                            self.method = Method::GET;
                        }
                    }
                    true
                }
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                    match self.body {
                        Some(Some(_)) | None => true,
                        Some(None) => false,
                    }
                }
                _ => false,
            };

            if should_redirect {
                let loc = res.headers().get(LOCATION).and_then(|val| {
                    let loc = (|| -> Option<Url> {
                        // Some sites may send a utf-8 Location header,
                        // even though we're supposed to treat those bytes
                        // as opaque, we'll check specifically for utf8.
                        self.url
                            .join(std::str::from_utf8(val.as_bytes()).ok()?)
                            .ok()
                    })();

                    // Check that the `url` is also a valid `http::Uri`.
                    //
                    // If not, just log it and skip the redirect.
                    let loc = loc.and_then(|url| {
                        if try_uri(&url).is_some() {
                            Some(url)
                        } else {
                            None
                        }
                    });

                    if loc.is_none() {
                        debug!("Location header had invalid URI: {:?}", val);
                    }
                    loc
                });
                if let Some(loc) = loc {
                    if self.client.referer {
                        if let Some(referer) = make_referer(&loc, &self.url) {
                            self.headers.insert(REFERER, referer);
                        }
                    }
                    let url = self.url.clone();
                    self.as_mut().urls().push(url);

                    let action = self
                        .redirect
                        .as_ref()
                        .unwrap_or(&self.client.redirect)
                        .check(
                            res.status(),
                            &self.method,
                            &loc,
                            &previous_method,
                            &self.urls,
                        );

                    match action {
                        redirect::ActionKind::Follow => {
                            debug!("redirecting '{}' to '{}'", self.url, loc);

                            if loc.scheme() != "http" && loc.scheme() != "https" {
                                return Poll::Ready(Err(error::url_bad_scheme(loc)));
                            }

                            if self.client.https_only && loc.scheme() != "https" {
                                return Poll::Ready(Err(error::redirect(
                                    error::url_bad_scheme(loc.clone()),
                                    loc,
                                )));
                            }

                            self.url = loc;
                            let mut headers =
                                std::mem::replace(self.as_mut().headers(), HeaderMap::new());

                            redirect::Policy::remove_sensitive_headers(
                                &mut headers,
                                &self.url,
                                &self.urls,
                            );

                            let uri = match try_uri(&self.url) {
                                Some(uri) => uri,
                                None => {
                                    return Poll::Ready(Err(error::url_bad_uri(self.url.clone())));
                                }
                            };

                            let body = match self.body {
                                Some(Some(ref body)) => Body::reusable(body.clone()),
                                _ => Body::empty(),
                            };

                            // Add cookies from the cookie store.
                            #[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
                            if let Some(cookie_store) = self.client.cookie_store.as_ref() {
                                add_cookie_header(cookie_store, &mut headers, &self.url);
                            }

                            *self.as_mut().in_flight().get_mut() = {
                                let req = InnerRequest::builder()
                                    .uri(uri)
                                    .method(self.method.clone())
                                    .headers(headers.clone())
                                    .headers_order(self.headers_order.as_deref())
                                    .version(self.version)
                                    .extension(self.protocol.clone())
                                    .network_scheme(self.network_scheme.clone())
                                    .body(body)?;

                                std::mem::swap(self.as_mut().headers(), &mut headers);
                                self.client.hyper.request(req)
                            };

                            continue;
                        }
                        redirect::ActionKind::Stop => {
                            debug!("redirect policy disallowed redirection to '{}'", loc);
                        }
                        redirect::ActionKind::Error(err) => {
                            return Poll::Ready(Err(error::redirect(err, self.url.clone())));
                        }
                    }
                }
            }

            let res = Response::new(
                res,
                self.url.clone(),
                self.client.accepts,
                self.total_timeout.take(),
                self.read_timeout,
            );
            return Poll::Ready(Ok(res));
        }
    }
}

fn is_retryable_error(err: &(dyn std::error::Error + 'static)) -> bool {
    // pop the legacy::Error
    let err = if let Some(err) = err.source() {
        err
    } else {
        return false;
    };

    if let Some(cause) = err.source() {
        if let Some(err) = cause.downcast_ref::<http2::Error>() {
            // They sent us a graceful shutdown, try with a new connection!
            if err.is_go_away() && err.is_remote() && err.reason() == Some(http2::Reason::NO_ERROR)
            {
                return true;
            }

            // REFUSED_STREAM was sent from the server, which is safe to retry.
            // https://www.rfc-editor.org/rfc/rfc9113.html#section-8.7-3.2
            if err.is_reset()
                && err.is_remote()
                && err.reason() == Some(http2::Reason::REFUSED_STREAM)
            {
                return true;
            }
        }
    }
    false
}

fn make_referer(next: &Url, previous: &Url) -> Option<HeaderValue> {
    if next.scheme() == "http" && previous.scheme() == "https" {
        return None;
    }

    let mut referer = previous.clone();
    let _ = referer.set_username("");
    let _ = referer.set_password(None);
    referer.set_fragment(None);
    referer.as_str().parse().ok()
}

#[cfg(any(feature = "cookies", feature = "cookies-abstract"))]
fn add_cookie_header(
    cookie_store: &Arc<dyn cookie::CookieStore>,
    headers: &mut HeaderMap,
    url: &Url,
) {
    if let Some(header) = cookie_store.cookies(url) {
        #[cfg(feature = "cookies-multiple")]
        header
            .as_bytes()
            .split(|b| *b == b';')
            .filter_map(|s| {
                let s = s.strip_prefix(b" ").unwrap_or(s);
                HeaderValue::from_bytes(s).ok()
            })
            .for_each(|header| {
                headers.append(crate::header::COOKIE, header);
            });

        #[cfg(not(feature = "cookies-multiple"))]
        headers.insert(crate::header::COOKIE, header);
    }
}

#[cfg(any(
    feature = "gzip",
    feature = "brotli",
    feature = "zstd",
    feature = "deflate"
))]
fn add_accpet_encoding_header(accepts: &Accepts, headers: &mut HeaderMap) {
    if let Some(accept_encoding) = accepts.as_str() {
        if !headers.contains_key(crate::header::ACCEPT_ENCODING)
            && !headers.contains_key(crate::header::RANGE)
        {
            headers.insert(
                crate::header::ACCEPT_ENCODING,
                HeaderValue::from_static(accept_encoding),
            );
        }
    }
}

pub fn apply_http1_config(mut builder: Http1Builder<'_>, http1: Http1Config) {
    apply_http1_config2(&mut builder, http1);
}

/// Apply http1 config
pub fn apply_http1_config2(builder: &mut conn::http1::Builder, http1: Http1Config) {
    builder
        .http09_responses(http1.http09_responses)
        .max_headers(http1.max_headers)
        .max_buf_size(http1.max_buf_size)
        .read_buf_exact_size(http1.read_buf_exact_size)
        .preserve_header_case(http1.preserve_header_case)
        .title_case_headers(http1.title_case_headers)
        .ignore_invalid_headers_in_responses(http1.ignore_invalid_headers_in_responses)
        .allow_spaces_after_header_name_in_responses(
            http1.allow_spaces_after_header_name_in_responses,
        )
        .allow_obsolete_multiline_headers_in_responses(
            http1.allow_obsolete_multiline_headers_in_responses,
        );

    if let Some(writev) = http1.writev {
        builder.writev(writev);
    }
}

fn apply_http2_config(mut builder: Http2Builder<'_>, http2: Http2Config) {
    apply_http2_config2(&mut builder, http2);
}

/// Apply http2 config
pub fn apply_http2_config2<T: Clone>(builder: &mut conn::http2::Builder<T>, http2: Http2Config) {
    builder
        .initial_stream_id(http2.initial_stream_id)
        .initial_stream_window_size(http2.initial_stream_window_size)
        .initial_connection_window_size(http2.initial_connection_window_size)
        .max_concurrent_streams(http2.max_concurrent_streams)
        .header_table_size(http2.header_table_size)
        .max_frame_size(http2.max_frame_size)
        .headers_priority(http2.headers_priority)
        .headers_pseudo_order(http2.headers_pseudo_order)
        .settings_order(http2.settings_order)
        .priority(http2.priority);

    if let Some(max_header_list_size) = http2.max_header_list_size {
        builder.max_header_list_size(max_header_list_size);
    }

    if let Some(enable_push) = http2.enable_push {
        builder.enable_push(enable_push);
    }

    if let Some(enable_connect_protocol) = http2.unknown_setting8 {
        builder.unknown_setting8(enable_connect_protocol);
    }

    if let Some(unknown_setting9) = http2.unknown_setting9 {
        builder.unknown_setting9(unknown_setting9);
    }
}
