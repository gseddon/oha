use bytes::Bytes;
#[cfg(feature = "http3")]
use bytes::Buf;
use http_body_util::{BodyExt, Full};
use hyper::{Method, http};
use hyper_util::rt::{TokioExecutor, TokioIo};
use kanal::AsyncReceiver;
use rand::prelude::*;
use std::{
    borrow::Cow,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::Instant,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use url::{ParseError, Url};

use crate::{
    aws_auth::AwsSignatureConfig,
    pcg64si::Pcg64Si,
    url_generator::{UrlGenerator, UrlGeneratorError},
    ConnectToEntry,
};

#[cfg(feature = "http3")]
use crate::client_h3::{parallel_work_http3, spawn_http3_driver};


type SendRequestHttp1 = hyper::client::conn::http1::SendRequest<Full<Bytes>>;
type SendRequestHttp2 = hyper::client::conn::http2::SendRequest<Full<Bytes>>;
#[cfg(feature = "http3")]
pub type SendRequestHttp3 = (
    h3::client::Connection<h3_quinn::Connection, Bytes>,
    h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
);

#[derive(Debug, Clone, Copy)]
pub struct ConnectionTime {
    pub dns_lookup: std::time::Instant,
    pub dialup: std::time::Instant,
}

#[derive(Debug, Clone)]
/// a result for a request
pub struct RequestResult {
    pub rng: Pcg64Si,
    // When the query should started
    pub start_latency_correction: Option<std::time::Instant>,
    /// When the query started
    pub start: std::time::Instant,
    /// DNS + dialup
    /// None when reuse connection
    pub connection_time: Option<ConnectionTime>,
    /// First body byte received
    pub first_byte: Option<std::time::Instant>,
    /// When the query ends
    pub end: std::time::Instant,
    /// HTTP status
    pub status: http::StatusCode,
    /// Length of body
    pub len_bytes: usize,
}

impl RequestResult {
    /// Duration the request takes.
    pub fn duration(&self) -> std::time::Duration {
        self.end - self.start_latency_correction.unwrap_or(self.start)
    }
}

// encapsulates the HTTP generation of the work type. Used internally only for conditional logic.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HttpWorkType {
    H1,
    H2,
    #[cfg(feature = "http3")]
    H3,
}

pub struct Dns {
    pub connect_to: Vec<ConnectToEntry>,
    pub resolver:
        hickory_resolver::AsyncResolver<hickory_resolver::name_server::TokioConnectionProvider>,
}

impl Dns {
    /// Perform a DNS lookup for a given url and returns (ip_addr, port)
    async fn lookup<R: Rng>(
        &self,
        url: &Url,
        rng: &mut R,
    ) -> Result<(std::net::IpAddr, u16), ClientError> {
        let host = url.host_str().ok_or(ClientError::HostNotFound)?;
        let port = url
            .port_or_known_default()
            .ok_or(ClientError::PortNotFound)?;

        // Try to find an override (passed via `--connect-to`) that applies to this (host, port),
        // choosing one randomly if several match.
        let (host, port) = if let Some(entry) = self
            .connect_to
            .iter()
            .filter(|entry| entry.requested_port == port && entry.requested_host == host)
            .collect::<Vec<_>>()
            .choose(rng)
        {
            (entry.target_host.as_str(), entry.target_port)
        } else {
            (host, port)
        };

        let host = if host.starts_with('[') && host.ends_with(']') {
            // host is [ipv6] format
            // remove first [ and last ]
            &host[1..host.len() - 1]
        } else {
            host
        };

        // Perform actual DNS lookup, either on the original (host, port), or
        // on the (host, port) specified with `--connect-to`.
        let addrs = self
            .resolver
            .lookup_ip(host)
            .await
            .map_err(Box::new)?
            .iter()
            .collect::<Vec<_>>();

        let addr = *addrs.choose(rng).ok_or(ClientError::DNSNoRecord)?;

        Ok((addr, port))
    }
}

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("failed to get port from URL")]
    PortNotFound,
    #[error("failed to get host from URL")]
    HostNotFound,
    #[error("No record returned from DNS")]
    DNSNoRecord,
    #[error("Redirection limit has reached")]
    TooManyRedirect,
    #[error(transparent)]
    // Use Box here because ResolveError is big.
    ResolveError(#[from] Box<hickory_resolver::error::ResolveError>),

    #[cfg(feature = "native-tls")]
    #[error(transparent)]
    NativeTlsError(#[from] native_tls::Error),

    #[cfg(feature = "rustls")]
    #[error(transparent)]
    RustlsError(#[from] rustls::Error),

    #[cfg(feature = "rustls")]
    #[error(transparent)]
    InvalidDnsName(#[from] rustls_pki_types::InvalidDnsNameError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error(transparent)]
    HttpError(#[from] http::Error),
    #[error(transparent)]
    HyperError(#[from] hyper::Error),
    #[error(transparent)]
    InvalidUriParts(#[from] http::uri::InvalidUriParts),
    #[error(transparent)]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("Failed to get header from builder")]
    GetHeaderFromBuilderError,
    #[error(transparent)]
    HeaderToStrError(#[from] http::header::ToStrError),
    #[error(transparent)]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("timeout")]
    Timeout,
    #[error("aborted due to deadline")]
    Deadline,
    #[error(transparent)]
    UrlGeneratorError(#[from] UrlGeneratorError),
    #[error(transparent)]
    UrlParseError(#[from] ParseError),
    #[error("AWS SigV4 signature error: {0}")]
    SigV4Error(&'static str),
    #[cfg(feature = "http3")]
    #[error("QUIC Client: {0}")]
    QuicClientConfigError(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[cfg(feature = "http3")]
    #[error("QUIC connect: {0}")]
    QuicConnectError(#[from] quinn::ConnectError),
    #[cfg(feature = "http3")]
    #[error("QUIC connection: {0}")]
    QuicConnectionError(#[from] quinn::ConnectionError),
    #[cfg(feature = "http3")]
    #[error("HTTP3: {0}")]
    H3Error(#[from] h3::Error),
    #[cfg(feature = "http3")]
    #[error("Quic connection closed earlier than expected")]
    QuicDriverClosedEarlyError(#[from] tokio::sync::oneshot::error::RecvError),
}

pub struct Client {
    pub http_version: http::Version,
    pub proxy_http_version: http::Version,
    pub url_generator: UrlGenerator,
    pub method: http::Method,
    pub headers: http::header::HeaderMap,
    pub proxy_headers: http::header::HeaderMap,
    pub body: Option<&'static [u8]>,
    pub dns: Dns,
    pub timeout: Option<std::time::Duration>,
    pub redirect_limit: usize,
    pub disable_keepalive: bool,
    pub proxy_url: Option<Url>,
    pub aws_config: Option<AwsSignatureConfig>,
    #[cfg(unix)]
    pub unix_socket: Option<std::path::PathBuf>,
    #[cfg(feature = "vsock")]
    pub vsock_addr: Option<tokio_vsock::VsockAddr>,
    #[cfg(feature = "rustls")]
    pub rustls_configs: crate::tls_config::RuslsConfigs,
    #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
    pub native_tls_connectors: crate::tls_config::NativeTlsConnectors,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            http_version: http::Version::HTTP_11,
            proxy_http_version: http::Version::HTTP_11,
            url_generator: UrlGenerator::new_static("http://example.com".parse().unwrap()),
            method: http::Method::GET,
            headers: http::header::HeaderMap::new(),
            proxy_headers: http::header::HeaderMap::new(),
            body: None,
            dns: Dns {
                resolver: hickory_resolver::AsyncResolver::tokio_from_system_conf().unwrap(),
                connect_to: Vec::new(),
            },
            timeout: None,
            redirect_limit: 0,
            disable_keepalive: false,
            proxy_url: None,
            aws_config: None,
            #[cfg(unix)]
            unix_socket: None,
            #[cfg(feature = "vsock")]
            vsock_addr: None,
            #[cfg(feature = "rustls")]
            rustls_configs: crate::tls_config::RuslsConfigs::new(false, None, None),
            #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
            native_tls_connectors: crate::tls_config::NativeTlsConnectors::new(false, None, None),
        }
    }
}

struct ClientStateHttp1 {
    rng: Pcg64Si,
    send_request: Option<SendRequestHttp1>,
}

impl Default for ClientStateHttp1 {
    fn default() -> Self {
        Self {
            rng: SeedableRng::from_os_rng(),
            send_request: None,
        }
    }
}

struct ClientStateHttp2 {
    rng: Pcg64Si,
    send_request: SendRequestHttp2,
}

pub enum QueryLimit {
    Qps(f64),
    Burst(std::time::Duration, usize),
}

// To avoid dynamic dispatch
// I'm not sure how much this is effective
pub (crate) enum Stream {
    Tcp(TcpStream),
    #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
    Tls(tokio_native_tls::TlsStream<TcpStream>),
    #[cfg(feature = "rustls")]
    // Box for large variant
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(feature = "vsock")]
    Vsock(tokio_vsock::VsockStream),
    #[cfg(feature = "http3")]
    Quic(quinn::Connection)
}

impl Stream {
    async fn handshake_http1(self, with_upgrade: bool) -> Result<SendRequestHttp1, ClientError> {
        match self {
            Stream::Tcp(stream) => {
                let (send_request, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
                if with_upgrade {
                    tokio::spawn(conn.with_upgrades());
                } else {
                    tokio::spawn(conn);
                }
                Ok(send_request)
            }
            Stream::Tls(stream) => {
                let (send_request, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
                if with_upgrade {
                    tokio::spawn(conn.with_upgrades());
                } else {
                    tokio::spawn(conn);
                }
                Ok(send_request)
            }
            #[cfg(unix)]
            Stream::Unix(stream) => {
                let (send_request, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
                if with_upgrade {
                    tokio::spawn(conn.with_upgrades());
                } else {
                    tokio::spawn(conn);
                }
                Ok(send_request)
            }
            #[cfg(feature = "vsock")]
            Stream::Vsock(stream) => {
                let (send_request, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
                if with_upgrade {
                    tokio::spawn(conn.with_upgrades());
                } else {
                    tokio::spawn(conn);
                }
                Ok(send_request)
            }
            #[cfg(feature = "http3")]
            Stream::Quic(_) => {
                panic!("quic is not supported in http1")
            }
        }
    }
    async fn handshake_http2(self) -> Result<SendRequestHttp2, ClientError> {
        let mut builder = hyper::client::conn::http2::Builder::new(TokioExecutor::new());
        builder
            // from nghttp2's default
            .initial_stream_window_size((1 << 30) - 1)
            .initial_connection_window_size((1 << 30) - 1);

        match self {
            Stream::Tcp(stream) => {
                let (send_request, conn) = builder.handshake(TokioIo::new(stream)).await?;
                tokio::spawn(conn);
                Ok(send_request)
            }
            Stream::Tls(stream) => {
                let (send_request, conn) = builder.handshake(TokioIo::new(stream)).await?;
                tokio::spawn(conn);
                Ok(send_request)
            }
            #[cfg(unix)]
            Stream::Unix(stream) => {
                let (send_request, conn) = builder.handshake(TokioIo::new(stream)).await?;
                tokio::spawn(conn);
                Ok(send_request)
            }
            #[cfg(feature = "vsock")]
            Stream::Vsock(stream) => {
                let (send_request, conn) = builder.handshake(TokioIo::new(stream)).await?;
                tokio::spawn(conn);
                Ok(send_request)
            }
            #[cfg(feature = "http3")]
            Stream::Quic(_) => {
                panic!("quic is not supported in http2")
            }
        }
    }
}

impl Client {
    #[inline]
    fn is_http2(&self) -> bool {
        self.http_version == http::Version::HTTP_2
    }

    #[inline]
    pub fn is_http1(&self) -> bool {
        self.http_version <= http::Version::HTTP_11
    }

    #[inline]
    fn is_proxy_http2(&self) -> bool {
        self.proxy_http_version == http::Version::HTTP_2
    }

    pub fn is_work_http2(&self) -> bool {
        if self.proxy_url.is_some() {
            let url = self
                .url_generator
                .generate(&mut Pcg64Si::from_seed([0, 0, 0, 0, 0, 0, 0, 0]))
                .unwrap();
            if url.scheme() == "https" {
                self.is_http2()
            } else {
                self.is_proxy_http2()
            }
        } else {
            self.is_http2()
        }
    }

    // slightly naughty reusing the HTTP version (there are different versions of 1)
    fn work_type(&self) -> HttpWorkType {
        #[cfg(feature = "http3")]
        if self.http_version == http::Version::HTTP_3 {
            return HttpWorkType::H3;
        }
        if self.is_work_http2() {
            HttpWorkType::H2
        } else {
            HttpWorkType::H1
        }
    }

    /// Perform a DNS lookup to cache it
    /// This is useful to avoid DNS lookup latency at the first concurrent requests
    pub async fn pre_lookup(&self) -> Result<(), ClientError> {
        // If the client is using a unix socket, we don't need to do a DNS lookup
        #[cfg(unix)]
        if self.unix_socket.is_some() {
            return Ok(());
        }
        // If the client is using a vsock address, we don't need to do a DNS lookup
        #[cfg(feature = "vsock")]
        if self.vsock_addr.is_some() {
            return Ok(());
        }

        let mut rng = StdRng::from_os_rng();
        let url = self.url_generator.generate(&mut rng)?;

        // It automatically caches the result
        self.dns.lookup(&url, &mut rng).await?;
        Ok(())
    }

    pub fn generate_url(&self, rng: &mut Pcg64Si) -> Result<(Cow<Url>, Pcg64Si), ClientError> {
        let snapshot = *rng;
        Ok((self.url_generator.generate(rng)?, snapshot))
    }

    /**
     * Returns a stream of the underlying transport. NOT a HTTP client
     */
    pub (crate) async fn client<R: Rng>(
        &self,
        url: &Url,
        rng: &mut R,
        http_version: http::Version
    ) -> Result<(Instant, Stream), ClientError> {
        // TODO: Allow the connect timeout to be configured
        let timeout_duration = tokio::time::Duration::from_secs(5);

        #[cfg(feature = "http3")]
        if http_version == http::Version::HTTP_3 {
            let addr = self.dns.lookup(url, rng).await?;
            let dns_lookup = Instant::now();
            let stream = tokio::time::timeout(timeout_duration, self.quic_client(addr, url)).await;
            return match stream {
                Ok(Ok(stream)) => Ok((dns_lookup, stream)),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(ClientError::Timeout),
            };
        }
        if url.scheme() == "https" {
            let addr = self.dns.lookup(url, rng).await?;
            let dns_lookup = Instant::now();
            // If we do not put a timeout here then the connections attempts will
            // linger long past the configured timeout
            let stream =
                tokio::time::timeout(timeout_duration, self.tls_client(addr, url, http_version)).await;
            return match stream {
                Ok(Ok(stream)) => Ok((dns_lookup, stream)),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(ClientError::Timeout),
            };
        }
        #[cfg(unix)]
        if let Some(socket_path) = &self.unix_socket {
            let dns_lookup = Instant::now();
            let stream = tokio::time::timeout(
                timeout_duration,
                tokio::net::UnixStream::connect(socket_path),
            )
            .await;
            return match stream {
                Ok(Ok(stream)) => Ok((dns_lookup, Stream::Unix(stream))),
                Ok(Err(err)) => Err(ClientError::IoError(err)),
                Err(_) => Err(ClientError::Timeout),
            };
        }
        #[cfg(feature = "vsock")]
        if let Some(addr) = self.vsock_addr {
            let dns_lookup = Instant::now();
            let stream =
                tokio::time::timeout(timeout_duration, tokio_vsock::VsockStream::connect(addr))
                    .await;
            return match stream {
                Ok(Ok(stream)) => Ok((dns_lookup, Stream::Vsock(stream))),
                Ok(Err(err)) => Err(ClientError::IoError(err)),
                Err(_) => Err(ClientError::Timeout),
            };
        }
        // HTTP
        let addr = self.dns.lookup(url, rng).await?;
        let dns_lookup = Instant::now();
        let stream =
            tokio::time::timeout(timeout_duration, tokio::net::TcpStream::connect(addr)).await;
        match stream {
            Ok(Ok(stream)) => {
                stream.set_nodelay(true)?;
                Ok((dns_lookup, Stream::Tcp(stream)))
            }
            Ok(Err(err)) => Err(ClientError::IoError(err)),
            Err(_) => Err(ClientError::Timeout),
        }
    }

    async fn tls_client(
        &self,
        addr: (std::net::IpAddr, u16),
        url: &Url,
        http_version: http::Version
    ) -> Result<Stream, ClientError> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;

        let stream = self.connect_tls(stream, url, http_version).await?;

        Ok(Stream::Tls(stream))
    }

    #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
    async fn connect_tls<S>(
        &self,
        stream: S,
        url: &Url,
        http_version: http::Version
    ) -> Result<tokio_native_tls::TlsStream<S>, ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let connector = self.native_tls_connectors.connector(is_http2);
        let stream = connector
            .connect(url.host_str().ok_or(ClientError::HostNotFound)?, stream)
            .await?;

        Ok(stream)
    }

    #[cfg(feature = "rustls")]
    async fn connect_tls<S>(
        &self,
        stream: S,
        url: &Url,
        http_version: http::Version
    ) -> Result<Box<tokio_rustls::client::TlsStream<S>>, ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let connector =
            tokio_rustls::TlsConnector::from(self.rustls_configs.config(http_version).clone());
        let domain = rustls_pki_types::ServerName::try_from(
            url.host_str().ok_or(ClientError::HostNotFound)?,
        )?;
        let stream = connector.connect(domain.to_owned(), stream).await?;

        Ok(Box::new(stream))
    }

    async fn client_http1<R: Rng>(
        &self,
        url: &Url,
        rng: &mut R,
    ) -> Result<(Instant, SendRequestHttp1), ClientError> {
        if let Some(proxy_url) = &self.proxy_url {
            let http_proxy_version = if self.is_proxy_http2() { http::Version::HTTP_2 } else { http::Version:: HTTP_11 };
            let (dns_lookup, stream) = self.client(proxy_url, rng, http_proxy_version).await?;
            if url.scheme() == "https" {
                // Do CONNECT request to proxy
                let req = {
                    let mut builder =
                        http::Request::builder()
                            .method(Method::CONNECT)
                            .uri(format!(
                                "{}:{}",
                                url.host_str().unwrap(),
                                url.port_or_known_default().unwrap()
                            ));
                    *builder
                        .headers_mut()
                        .ok_or(ClientError::GetHeaderFromBuilderError)? =
                        self.proxy_headers.clone();
                    builder.body(http_body_util::Full::default())?
                };
                let res = if self.proxy_http_version == http::Version::HTTP_2 {
                    let mut send_request = stream.handshake_http2().await?;
                    send_request.send_request(req).await?
                } else {
                    let mut send_request = stream.handshake_http1(true).await?;
                    send_request.send_request(req).await?
                };
                let stream = hyper::upgrade::on(res).await?;
                let stream = self.connect_tls(TokioIo::new(stream), url, self.http_version).await?;
                let (send_request, conn) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
                tokio::spawn(conn);
                Ok((dns_lookup, send_request))
            } else {
                // Send full URL in request() for HTTP proxy
                Ok((dns_lookup, stream.handshake_http1(false).await?))
            }
        } else {
            let (dns_lookup, stream) = self.client(url, rng, http::Version::HTTP_11).await?;
            Ok((dns_lookup, stream.handshake_http1(false).await?))
        }
    }

    #[inline]
    pub (crate) fn request(&self, url: &Url) -> Result<http::Request<Full<Bytes>>, ClientError> {
        let use_proxy = self.proxy_url.is_some() && url.scheme() == "http";

        let mut builder = http::Request::builder()
            .uri(if !(self.is_http1()) || use_proxy {
                &url[..]
            } else {
                &url[url::Position::BeforePath..]
            })
            .method(self.method.clone())
            .version(if use_proxy {
                self.proxy_http_version
            } else {
                self.http_version
            });

        let bytes = self.body.map(Bytes::from_static);

        let body = if let Some(body) = &bytes {
            Full::new(body.clone())
        } else {
            Full::default()
        };

        let mut headers = self.headers.clone();

        // Apply AWS SigV4 if configured
        if let Some(aws_config) = &self.aws_config {
            aws_config.sign_request(self.method.as_str(), &mut headers, url, bytes)?
        }

        if use_proxy {
            for (key, value) in self.proxy_headers.iter() {
                headers.insert(key, value.clone());
            }
        }

        *builder
            .headers_mut()
            .ok_or(ClientError::GetHeaderFromBuilderError)? = headers;

        let request = builder.body(body)?;

        Ok(request)
    }

    async fn work_http1(
        &self,
        client_state: &mut ClientStateHttp1,
    ) -> Result<RequestResult, ClientError> {
        let do_req = async {
            let (url, rng) = self.generate_url(&mut client_state.rng)?;
            let mut start = std::time::Instant::now();
            let mut first_byte: Option<std::time::Instant> = None;
            let mut connection_time: Option<ConnectionTime> = None;

            let mut send_request = if let Some(send_request) = client_state.send_request.take() {
                send_request
            } else {
                let (dns_lookup, send_request) =
                    self.client_http1(&url, &mut client_state.rng).await?;
                let dialup = std::time::Instant::now();

                connection_time = Some(ConnectionTime { dns_lookup, dialup });
                send_request
            };
            while send_request.ready().await.is_err() {
                // This gets hit when the connection for HTTP/1.1 faults
                // This re-connects
                start = std::time::Instant::now();
                let (dns_lookup, send_request_) =
                    self.client_http1(&url, &mut client_state.rng).await?;
                send_request = send_request_;
                let dialup = std::time::Instant::now();
                connection_time = Some(ConnectionTime { dns_lookup, dialup });
            }
            let request = self.request(&url)?;
            match send_request.send_request(request).await {
                Ok(res) => {
                    let (parts, mut stream) = res.into_parts();
                    let mut status = parts.status;

                    let mut len_bytes = 0;
                    while let Some(chunk) = stream.frame().await {
                        if first_byte.is_none() {
                            first_byte = Some(std::time::Instant::now())
                        }
                        len_bytes += chunk?.data_ref().map(|d| d.len()).unwrap_or_default();
                    }

                    if self.redirect_limit != 0 {
                        if let Some(location) = parts.headers.get("Location") {
                            let (send_request_redirect, new_status, len) = self
                                .redirect(
                                    send_request,
                                    &url,
                                    location,
                                    self.redirect_limit,
                                    &mut client_state.rng,
                                )
                                .await?;

                            send_request = send_request_redirect;
                            status = new_status;
                            len_bytes = len;
                        }
                    }

                    let end = std::time::Instant::now();

                    let result = RequestResult {
                        rng,
                        start_latency_correction: None,
                        start,
                        first_byte,
                        end,
                        status,
                        len_bytes,
                        connection_time,
                    };

                    if !self.disable_keepalive {
                        client_state.send_request = Some(send_request);
                    }

                    Ok::<_, ClientError>(result)
                }
                Err(e) => {
                    client_state.send_request = Some(send_request);
                    Err(e.into())
                }
            }
        };

        if let Some(timeout) = self.timeout {
            tokio::select! {
                res = do_req => {
                    res
                }
                _ = tokio::time::sleep(timeout) => {
                    Err(ClientError::Timeout)
                }
            }
        } else {
            do_req.await
        }
    }

    async fn connect_http2<R: Rng>(
        &self,
        url: &Url,
        rng: &mut R,
    ) -> Result<(ConnectionTime, SendRequestHttp2), ClientError> {
        if let Some(proxy_url) = &self.proxy_url {
            let http_proxy_version = if self.is_proxy_http2() { http::Version::HTTP_2 } else { http::Version:: HTTP_11 };
            let (dns_lookup, stream) = self.client(proxy_url, rng, http_proxy_version).await?;
            if url.scheme() == "https" {
                let req = {
                    let mut builder =
                        http::Request::builder()
                            .method(Method::CONNECT)
                            .uri(format!(
                                "{}:{}",
                                url.host_str().unwrap(),
                                url.port_or_known_default().unwrap()
                            ));
                    *builder
                        .headers_mut()
                        .ok_or(ClientError::GetHeaderFromBuilderError)? =
                        self.proxy_headers.clone();
                    builder.body(http_body_util::Full::default())?
                };
                let res = if self.proxy_http_version == http::Version::HTTP_2 {
                    let mut send_request = stream.handshake_http2().await?;
                    send_request.send_request(req).await?
                } else {
                    let mut send_request = stream.handshake_http1(true).await?;
                    send_request.send_request(req).await?
                };
                let stream = hyper::upgrade::on(res).await?;
                let stream = self.connect_tls(TokioIo::new(stream), url, http::Version::HTTP_2).await?;
                let (send_request, conn) =
                    hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                        // from nghttp2's default
                        .initial_stream_window_size((1 << 30) - 1)
                        .initial_connection_window_size((1 << 30) - 1)
                        .handshake(TokioIo::new(stream))
                        .await?;
                tokio::spawn(conn);
                let dialup = std::time::Instant::now();

                Ok((ConnectionTime { dns_lookup, dialup }, send_request))
            } else {
                let send_request = stream.handshake_http2().await?;
                let dialup = std::time::Instant::now();
                Ok((ConnectionTime { dns_lookup, dialup }, send_request))
            }
        } else {
            let (dns_lookup, stream) = self.client(url, rng, self.http_version).await?;
            let send_request = stream.handshake_http2().await?;
            let dialup = std::time::Instant::now();
            Ok((ConnectionTime { dns_lookup, dialup }, send_request))
        }
    }

    async fn work_http2(
        &self,
        client_state: &mut ClientStateHttp2,
    ) -> Result<RequestResult, ClientError> {
        let do_req = async {
            let (url, rng) = self.generate_url(&mut client_state.rng)?;
            let start = std::time::Instant::now();
            let mut first_byte: Option<std::time::Instant> = None;
            let connection_time: Option<ConnectionTime> = None;

            let request = self.request(&url)?;
            match client_state.send_request.send_request(request).await {
                Ok(res) => {
                    let (parts, mut stream) = res.into_parts();
                    let status = parts.status;

                    let mut len_bytes = 0;
                    while let Some(chunk) = stream.frame().await {
                        if first_byte.is_none() {
                            first_byte = Some(std::time::Instant::now())
                        }
                        len_bytes += chunk?.data_ref().map(|d| d.len()).unwrap_or_default();
                    }

                    let end = std::time::Instant::now();

                    let result = RequestResult {
                        rng,
                        start_latency_correction: None,
                        start,
                        first_byte,
                        end,
                        status,
                        len_bytes,
                        connection_time,
                    };

                    Ok::<_, ClientError>(result)
                }
                Err(e) => Err(e.into()),
            }
        };

        if let Some(timeout) = self.timeout {
            tokio::select! {
                res = do_req => {
                    res
                }
                _ = tokio::time::sleep(timeout) => {
                    Err(ClientError::Timeout)
                }
            }
        } else {
            do_req.await
        }
    }

    #[allow(clippy::type_complexity)]
    async fn redirect<R: Rng + Send>(
        &self,
        send_request: SendRequestHttp1,
        base_url: &Url,
        location: &http::header::HeaderValue,
        limit: usize,
        rng: &mut R,
    ) -> Result<(SendRequestHttp1, http::StatusCode, usize), ClientError> {
        if limit == 0 {
            return Err(ClientError::TooManyRedirect);
        }
        let url = match Url::parse(location.to_str()?) {
            Ok(url) => url,
            Err(ParseError::RelativeUrlWithoutBase) => Url::options()
                .base_url(Some(base_url))
                .parse(location.to_str()?)?,
            Err(err) => Err(err)?,
        };

        let (mut send_request, send_request_base) =
            if base_url.authority() == url.authority() && !self.disable_keepalive {
                // reuse connection
                (send_request, None)
            } else {
                let (_dns_lookup, stream) = self.client_http1(&url, rng).await?;
                (stream, Some(send_request))
            };

        while send_request.ready().await.is_err() {
            let (_dns_lookup, stream) = self.client_http1(&url, rng).await?;
            send_request = stream;
        }

        let mut request = self.request(&url)?;
        if url.authority() != base_url.authority() {
            request.headers_mut().insert(
                http::header::HOST,
                http::HeaderValue::from_str(url.authority())?,
            );
        }
        let res = send_request.send_request(request).await?;
        let (parts, mut stream) = res.into_parts();
        let mut status = parts.status;

        let mut len_bytes = 0;
        while let Some(chunk) = stream.frame().await {
            len_bytes += chunk?.data_ref().map(|d| d.len()).unwrap_or_default();
        }

        if let Some(location) = parts.headers.get("Location") {
            let (send_request_redirect, new_status, len) =
                Box::pin(self.redirect(send_request, &url, location, limit - 1, rng)).await?;
            send_request = send_request_redirect;
            status = new_status;
            len_bytes = len;
        }

        if let Some(send_request_base) = send_request_base {
            Ok((send_request_base, status, len_bytes))
        } else {
            Ok((send_request, status, len_bytes))
        }
    }
}

/// Check error and decide whether to cancel the connection
pub (crate) fn is_cancel_error(res: &Result<RequestResult, ClientError>) -> bool {
    matches!(res, Err(ClientError::Deadline)) || is_too_many_open_files(res)
}

/// Check error was "Too many open file"
fn is_too_many_open_files(res: &Result<RequestResult, ClientError>) -> bool {
    res.as_ref()
        .err()
        .map(|err| match err {
            ClientError::IoError(io_error) => io_error.raw_os_error() == Some(libc::EMFILE),
            _ => false,
        })
        .unwrap_or(false)
}

/// Check error was any Hyper error (primarily for HTTP2 connection errors)
fn is_hyper_error(res: &Result<RequestResult, ClientError>) -> bool {
    res.as_ref()
        .err()
        .map(|err| match err {
            // REVIEW: IoErrors, if indicating the underlying connection has failed,
            // should also cause a stop of HTTP2 requests
            ClientError::IoError(_) => true,
            ClientError::HyperError(_) => true,
            _ => false,
        })
        .unwrap_or(false)
}

async fn setup_http2(client: &Client) -> Result<(ConnectionTime, SendRequestHttp2), ClientError> {
    // Whatever rng state, all urls should have the same authority
    let mut rng: Pcg64Si = SeedableRng::from_seed([0, 0, 0, 0, 0, 0, 0, 0]);
    let url = client.url_generator.generate(&mut rng)?;
    let (connection_time, send_request) = client.connect_http2(&url, &mut rng).await?;

    Ok((connection_time, send_request))
}

async fn work_http2_once(
    client: &Client,
    client_state: &mut ClientStateHttp2,
    report_tx: &kanal::Sender<Result<RequestResult, ClientError>>,
    connection_time: ConnectionTime,
    start_latency_correction: Option<Instant>,
) -> (bool, bool) {
    let mut res = client.work_http2(client_state).await;
    let is_cancel = is_cancel_error(&res);
    let is_reconnect = is_hyper_error(&res);
    set_connection_time(&mut res, connection_time);
    if let Some(start_latency_correction) = start_latency_correction {
        set_start_latency_correction(&mut res, start_latency_correction);
    }
    report_tx.send(res).unwrap();
    (is_cancel, is_reconnect)
}

pub (crate) fn set_connection_time<E>(res: &mut Result<RequestResult, E>, connection_time: ConnectionTime) {
    if let Ok(res) = res {
        res.connection_time = Some(connection_time);
    }
}

pub (crate) fn set_start_latency_correction<E>(
    res: &mut Result<RequestResult, E>,
    start_latency_correction: std::time::Instant,
) {
    if let Ok(res) = res {
        res.start_latency_correction = Some(start_latency_correction);
    }
}

pub async fn work_debug<W: Write>(w: &mut W, client: Arc<Client>) -> Result<(), ClientError> {
    let mut rng = StdRng::from_os_rng();
    let url = client.url_generator.generate(&mut rng)?;
    writeln!(w, "URL: {}", url)?;

    let request = client.request(&url)?;

    writeln!(w, "{:#?}", request)?;

    let response = match client.work_type() {
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => {
            let(_, (h3_connection, mut client_state)) = client.connect_http3(&url, &mut rng).await?;

            // Prepare a channel to stop the driver thread
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            // Run the driver
            let http3_driver = spawn_http3_driver(h3_connection, shutdown_rx).await;


            let (head, mut req_body) = request.into_parts();
            let request = http::request::Request::from_parts(head, ());

            let mut stream = client_state.send_request(request).await?;
            match req_body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        stream.send_data(data).await?;
                    }
                }
                _ => {}
            }

            stream.finish().await?;

            let response = stream.recv_response().await.unwrap_or_else(|err| {
                panic!("{}", err);
            });
            let mut body_bytes = bytes::BytesMut::new();

            while let Some(mut chunk) = stream.recv_data().await? {
                let bytes = chunk.copy_to_bytes(chunk.remaining());
                body_bytes.extend_from_slice(&bytes);
            }
            let body = body_bytes.freeze();
            let _ = shutdown_tx.send(0);
            let _ = http3_driver.await.unwrap();
            let (parts, _) = response.into_parts();
            http::Response::from_parts(parts, body)

        }
        HttpWorkType::H2 => {
            let (_, mut client_state) = client.connect_http2(&url, &mut rng).await?;
            let response = client_state.send_request(request).await?;
            let (parts, body) = response.into_parts();
            let body = body.collect().await.unwrap().to_bytes();

            http::Response::from_parts(parts, body)
        }
        HttpWorkType::H1 => {
            let (_dns_lookup, mut send_request) = client.client_http1(&url, &mut rng).await?;

            let response = send_request.send_request(request).await?;
            let (parts, body) = response.into_parts();
            let body = body.collect().await.unwrap().to_bytes();

            http::Response::from_parts(parts, body)
        }
    };


    writeln!(w, "{:#?}", response)?;

    Ok(())
}

/// Run n tasks by m workers
pub async fn work(
    client: Arc<Client>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    n_tasks: usize,
    n_connections: usize,
    n_http2_parallel: usize,
) {
    let (tx, rx) = kanal::unbounded::<Option<Instant>>();
    let rx = rx.to_async();

    let n_tasks_emitter = async move {
        for _ in 0..n_tasks {
            tx.send(None)?
        }
        drop(tx);
        Ok::<(), kanal::SendError>(())
    };

    let futures = match client.work_type()  {
        HttpWorkType::H1 => parallel_work_http1(n_connections, rx, report_tx, client, None).await,
        HttpWorkType::H2 => parallel_work_http2(n_connections, n_http2_parallel, rx, report_tx, client, None).await,
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => parallel_work_http3(n_connections, n_http2_parallel, rx, report_tx, client, None).await,
    };
    n_tasks_emitter.await.unwrap();
    for f in futures {
        let _ = f.await;
    };
}

/// n tasks by m workers limit to qps works in a second
pub async fn work_with_qps(
    client: Arc<Client>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    query_limit: QueryLimit,
    n_tasks: usize,
    n_connections: usize,
    n_http2_parallel: usize,
) {
    let (tx, rx) = kanal::unbounded();

    let work_queue = async move {
        match query_limit {
            QueryLimit::Qps(qps) => {
                let start = std::time::Instant::now();
                for i in 0..n_tasks {
                    tokio::time::sleep_until(
                        (start + std::time::Duration::from_secs_f64(i as f64 * 1f64 / qps)).into(),
                    )
                    .await;
                    tx.send(None)?;
                }
            }
            QueryLimit::Burst(duration, rate) => {
                let mut n = 0;
                // Handle via rate till n_tasks out of bound
                while n + rate < n_tasks {
                    tokio::time::sleep(duration).await;
                    for _ in 0..rate {
                        tx.send(None)?;
                    }
                    n += rate;
                }
                // Handle the remaining tasks
                if n_tasks > n {
                    tokio::time::sleep(duration).await;
                    for _ in 0..n_tasks - n {
                        tx.send(None)?;
                    }
                }
            }
        }
        // tx gone
        drop(tx);
        Ok::<(), kanal::SendError>(())
    };

    let rx = rx.to_async();
    let futures = match client.work_type()  {
        HttpWorkType::H1 => parallel_work_http1(n_connections, rx, report_tx, client, None).await,
        HttpWorkType::H2 => parallel_work_http2(n_connections, n_http2_parallel, rx, report_tx, client, None).await,
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => parallel_work_http3(n_connections, n_http2_parallel, rx, report_tx, client, None).await,
    };
    work_queue.await.unwrap();
    for f in futures {
        let _ = f.await;
    };
}

async fn parallel_work_http1(
    n_connections: usize,
    rx: AsyncReceiver<Option<Instant>>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    client: Arc<Client>,
    deadline: Option<std::time::Instant>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let is_end = Arc::new(AtomicBool::new(false));
    let has_deadline = deadline.is_some();

    let futures = (0..n_connections)
        .map(|_| {
            let report_tx = report_tx.clone();
            let rx = rx.clone();
            let client = client.clone();
            let is_end = is_end.clone();
            tokio::spawn(async move {
                let mut client_state = ClientStateHttp1::default();
                while let Ok(rx_value) = rx.recv().await {
                    let mut res = client.work_http1(&mut client_state).await;
                    if let Some(start_latency_correction) = rx_value {
                        set_start_latency_correction(&mut res, start_latency_correction);
                    }
                    let is_cancel = is_cancel_error(&res);
                    report_tx.send(res).unwrap();
                    if is_cancel || has_deadline || is_end.load(Relaxed) {
                        break;
                    }
                }
            })
        })
        .collect::<Vec<_>>();

    if has_deadline {
        tokio::time::sleep_until(deadline.unwrap().into()).await;
        is_end.store(true, Relaxed);
    };

    return futures;
}

async fn parallel_work_http2(
    n_connections: usize,
    n_http2_parallel: usize,
    rx: AsyncReceiver<Option<Instant>>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    client: Arc<Client>,
    deadline: Option<std::time::Instant>
) -> Vec<tokio::task::JoinHandle<()>> {

    // Using semaphore to control the deadline
    // Maybe there is a better concurrent primitive to do this
    let s = Arc::new(tokio::sync::Semaphore::new(0));
    let has_deadline = deadline.is_some();

    let futures = (0..n_connections)
    .map(|_| {
        let report_tx = report_tx.clone();
        let rx = rx.clone();
        let client = client.clone();
        let s = s.clone();
        tokio::spawn(async move {
            let s = s.clone();
            loop {
                match setup_http2(&client).await {
                    Ok((connection_time, send_request)) => {
                        let futures = (0..n_http2_parallel)
                            .map(|_| {
                                let report_tx = report_tx.clone();
                                let rx = rx.clone();
                                let client = client.clone();
                                let mut client_state = ClientStateHttp2 {
                                    rng: SeedableRng::from_os_rng(),
                                    send_request: send_request.clone(),
                                };
                                let s = s.clone();
                                tokio::spawn(async move {
                                    // This is where HTTP2 loops to make all the requests for a given client and worker
                                    while let Ok(start_time_option) = rx.recv().await {
                                        let (is_cancel, is_reconnect) = work_http2_once(
                                            &client,
                                            &mut client_state,
                                            &report_tx,
                                            connection_time,
                                            start_time_option,
                                        )
                                        .await;

                                        let is_cancel = is_cancel || s.is_closed();
                                        if is_cancel || is_reconnect {
                                            return is_cancel;
                                        }
                                    }
                                    true
                                })
                            })
                            .collect::<Vec<_>>();
                        let mut connection_gone = false;
                        for f in futures {
                            tokio::select! {
                                r = f => {
                                    match r {
                                        Ok(true) => {
                                            // All works done
                                            connection_gone = true;
                                        }
                                        Err(_) => {
                                            // Unexpected
                                            connection_gone = true;
                                        }
                                        _ => {}
                                    }
                                }
                                _ = s.acquire() => {
                                    report_tx.send(Err(ClientError::Deadline)).unwrap();
                                    connection_gone = true;
                                }
                            }
                        }
                        if connection_gone {
                            return;
                        }
                    }
                    Err(err) => {
                        if s.is_closed() {
                            break;
                            // Consume a task 
                        } else if rx.recv().await.is_ok() {
                            report_tx.send(Err(err)).unwrap();
                        } else {
                            return;
                        }
                    }
                }
            }
        })
    })
    .collect::<Vec<_>>();

    if has_deadline {
        tokio::time::sleep_until(deadline.unwrap().into()).await;
        s.close();
    }

    return futures;

}

/// n tasks by m workers limit to qps works in a second with latency correction
pub async fn work_with_qps_latency_correction(
    client: Arc<Client>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    query_limit: QueryLimit,
    n_tasks: usize,
    n_connections: usize,
    n_http2_parallel: usize,
) {
    let (tx, rx) = kanal::unbounded();
    let rx = rx.to_async();

    let work_queue = async move {
        match query_limit {
            QueryLimit::Qps(qps) => {
                let start = std::time::Instant::now();
                for i in 0..n_tasks {
                    tokio::time::sleep_until(
                        (start + std::time::Duration::from_secs_f64(i as f64 * 1f64 / qps)).into(),
                    )
                    .await;
                    tx.send(Some(std::time::Instant::now()))?;
                }
            }
            QueryLimit::Burst(duration, rate) => {
                let mut n = 0;
                // Handle via rate till n_tasks out of bound
                while n + rate < n_tasks {
                    tokio::time::sleep(duration).await;
                    let now = std::time::Instant::now();
                    for _ in 0..rate {
                        tx.send(Some(now))?;
                    }
                    n += rate;
                }
                // Handle the remaining tasks
                if n_tasks > n {
                    tokio::time::sleep(duration).await;
                    let now = std::time::Instant::now();
                    for _ in 0..n_tasks - n {
                        tx.send(Some(now))?;
                    }
                }
            }
        }

        // tx gone
        drop(tx);
        Ok::<(), kanal::SendError>(())
    };

    let futures = match client.work_type()  {
        HttpWorkType::H1 => parallel_work_http1(n_connections, rx, report_tx, client, None).await,
        HttpWorkType::H2 => parallel_work_http2(n_connections, n_http2_parallel, rx, report_tx, client, None).await,
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => parallel_work_http3(n_connections, n_http2_parallel, rx, report_tx, client, None).await,
    };
    work_queue.await.unwrap();
    for f in futures {
        let _ = f.await;
    };
}

/// Run until dead_line by n workers
pub async fn work_until(
    client: Arc<Client>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    dead_line: std::time::Instant,
    n_connections: usize,
    n_http2_parallel: usize,
    wait_ongoing_requests_after_deadline: bool,
) {
    let (tx, rx) = kanal::bounded_async::<Option<Instant>>(5000);
    // These emitters are used for H2 and H3 to give it unlimited tokens to emit work.
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let emitter_handle = endless_emitter(cancel_token.clone(), tx).await;
    let futures = match client.work_type() {
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => parallel_work_http3(n_connections, n_http2_parallel, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
        HttpWorkType::H2 => parallel_work_http2(n_connections, n_http2_parallel, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
        HttpWorkType::H1 => parallel_work_http1(n_connections, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
    };
    if client.work_type() == HttpWorkType::H1 && !wait_ongoing_requests_after_deadline {
        for f in futures {
            f.abort();
            if let Err(e) = f.await {
                if e.is_cancelled() {
                    report_tx.send(Err(ClientError::Deadline)).unwrap();
                }
            }
        }
    } else {
        for f in futures {
            let _ = f.await;
        }
    }
    // Cancel the emitter when we're done with the futures
    cancel_token.cancel();
    // Wait for the emitter to exit cleanly
    let _ = emitter_handle.await;

}

async fn endless_emitter(
    cancellation_token: tokio_util::sync::CancellationToken,
    tx: kanal::AsyncSender<Option<Instant>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    break;
                }
                _ = async {
                    // As we our `work_http2_once` function is limited by the number of `tx` we send, but we only
                    // want to stop when our semaphore is closed, just dump unlimited `Nones` into the tx to un-constrain it
                    let _ = tx.send(None).await;
                } => {}
            }
        }
    })
}

/// Run until dead_line by n workers limit to qps works in a second
#[allow(clippy::too_many_arguments)]
pub async fn work_until_with_qps(
    client: Arc<Client>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    query_limit: QueryLimit,
    start: std::time::Instant,
    dead_line: std::time::Instant,
    n_connections: usize,
    n_http2_parallel: usize,
    wait_ongoing_requests_after_deadline: bool,
) {
    let rx = match query_limit {
        QueryLimit::Qps(qps) => {
            let (tx, rx) = kanal::unbounded();
            tokio::spawn(async move {
                for i in 0.. {
                    if std::time::Instant::now() > dead_line {
                        break;
                    }
                    tokio::time::sleep_until(
                        (start + std::time::Duration::from_secs_f64(i as f64 * 1f64 / qps)).into(),
                    )
                    .await;
                    let _ = tx.send(None);
                }
                // tx gone
            });
            rx
        }
        QueryLimit::Burst(duration, rate) => {
            let (tx, rx) = kanal::unbounded();
            tokio::spawn(async move {
                // Handle via rate till deadline is reached
                for _ in 0.. {
                    if std::time::Instant::now() > dead_line {
                        break;
                    }

                    tokio::time::sleep(duration).await;
                    for _ in 0..rate {
                        let _ = tx.send(None);
                    }
                }
                // tx gone
            });
            rx
        }
    };

    let rx = rx.to_async();
    let futures = match client.work_type() {
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => parallel_work_http3(n_connections, n_http2_parallel, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
        HttpWorkType::H2 => parallel_work_http2(n_connections, n_http2_parallel, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
        HttpWorkType::H1 => parallel_work_http1(n_connections, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
    };
    if client.work_type() == HttpWorkType::H1 && !wait_ongoing_requests_after_deadline {
        for f in futures {
            f.abort();
            if let Err(e) = f.await {
                if e.is_cancelled() {
                    report_tx.send(Err(ClientError::Deadline)).unwrap();
                }
            }
        }
    } else {
        for f in futures {
            let _ = f.await;
        }
    }
}

/// Run until dead_line by n workers limit to qps works in a second with latency correction
#[allow(clippy::too_many_arguments)]
pub async fn work_until_with_qps_latency_correction(
    client: Arc<Client>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    query_limit: QueryLimit,
    start: std::time::Instant,
    dead_line: std::time::Instant,
    n_connections: usize,
    n_http2_parallel: usize,
    wait_ongoing_requests_after_deadline: bool,
) {
    let (tx, rx) = kanal::unbounded();
    match query_limit {
        QueryLimit::Qps(qps) => {
            tokio::spawn(async move {
                for i in 0.. {
                    tokio::time::sleep_until(
                        (start + std::time::Duration::from_secs_f64(i as f64 * 1f64 / qps)).into(),
                    )
                    .await;
                    let now = std::time::Instant::now();
                    if now > dead_line {
                        break;
                    }
                    let _ = tx.send(Some(now));
                }
                // tx gone
            });
        }
        QueryLimit::Burst(duration, rate) => {
            tokio::spawn(async move {
                // Handle via rate till deadline is reached
                loop {
                    tokio::time::sleep(duration).await;
                    let now = std::time::Instant::now();
                    if now > dead_line {
                        break;
                    }

                    for _ in 0..rate {
                        let _ = tx.send(Some(now));
                    }
                }
                // tx gone
            });
        }
    };

    let rx = rx.to_async();
    let futures = match client.work_type() {
        #[cfg(feature = "http3")]
        HttpWorkType::H3 => parallel_work_http3(n_connections, n_http2_parallel, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
        HttpWorkType::H2 => parallel_work_http2(n_connections, n_http2_parallel, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
        HttpWorkType::H1 => parallel_work_http1(n_connections, rx, report_tx.clone(), client.clone(), Some(dead_line)).await,
    };
    if client.work_type() == HttpWorkType::H1 && !wait_ongoing_requests_after_deadline {
        for f in futures {
            f.abort();
            if let Err(e) = f.await {
                if e.is_cancelled() {
                    report_tx.send(Err(ClientError::Deadline)).unwrap();
                }
            }
        }
    } else {
        for f in futures {
            let _ = f.await;
        }
    }
}

/**
 * Optimized workers for `--no-tui` mode
 * These workers will all run on a single thread and are not `async`. 
 * Reduces tokio synchronisation overhead.
 */
pub mod fast {
    use std::sync::{atomic::{AtomicBool, AtomicIsize, Ordering}, Arc};

    use rand::SeedableRng;

    use crate::{
        client::{
            is_cancel_error,
            is_hyper_error,
            set_connection_time,
            setup_http2,
            ClientError,
            ClientStateHttp1,
            ClientStateHttp2,
            HttpWorkType
        }, result_data::ResultData
    };

    #[cfg(feature = "http3")]
    use crate::client_h3::http3_connection_fast_work_until;

    use super::Client;

    /// Run n tasks by m workers
    pub async fn work(
        client: Arc<Client>,
        report_tx: kanal::Sender<ResultData>,
        n_tasks: usize,
        n_connections: usize,
        n_http_parallel: usize,
    ) {
        let counter = Arc::new(AtomicIsize::new(n_tasks as isize));
        let num_threads = num_cpus::get_physical();
        let connections = (0..num_threads).filter_map(|i| {
            let num_connection = n_connections / num_threads
                + (if (n_connections % num_threads) > i {
                    1
                } else {
                    0
                });
            if num_connection > 0 {
                Some(num_connection)
            } else {
                None
            }
        });
        let token = tokio_util::sync::CancellationToken::new();
        let handles = connections
        .map(|num_connections| {
            let report_tx = report_tx.clone();
            let client = client.clone();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let token = token.clone();
            let counter = counter.clone();
            // will let is_end just stay false permanently
            let is_end = Arc::new(AtomicBool::new(false));
            std::thread::spawn(move || match client.work_type() {
                #[cfg(feature = "http3")]
                HttpWorkType::H3 => http3_connection_fast_work_until(num_connections, n_http_parallel, report_tx, client, token, Some(counter), is_end, rt),
                HttpWorkType::H2 => http2_connection_fast_work_until(num_connections, n_http_parallel, report_tx, client, token, Some(counter), is_end, rt),
                HttpWorkType::H1 => http1_connection_fast_work_until(num_connections, report_tx, client, token, Some(counter), is_end, rt)
            })
        })
        .collect::<Vec<_>>();

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
            token.cancel();
        });

        tokio::task::block_in_place(|| {
            for handle in handles {
                let _ = handle.join();
            }
        });
    }

    /// Run until dead_line by n workers
    pub async fn work_until(
        client: Arc<Client>,
        report_tx: kanal::Sender<ResultData>,
        dead_line: std::time::Instant,
        n_connections: usize,
        n_http_parallel: usize,
        wait_ongoing_requests_after_deadline: bool,
    ) {
        let num_threads = num_cpus::get_physical();

        let is_end = Arc::new(AtomicBool::new(false));
        let connections = (0..num_threads).filter_map(|i| {
            let num_connection = n_connections / num_threads
                + (if (n_connections % num_threads) > i {
                    1
                } else {
                    0
                });
            if num_connection > 0 {
                Some(num_connection)
            } else {
                None
            }
        });
        let token = tokio_util::sync::CancellationToken::new();
        let handles = connections
            .map(|num_connections| {
                let report_tx = report_tx.clone();
                let client = client.clone();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let token = token.clone();
                let is_end = is_end.clone();
                std::thread::spawn(move || match client.work_type() {
                    #[cfg(feature = "http3")]
                    HttpWorkType::H3 => http3_connection_fast_work_until(num_connections, n_http_parallel, report_tx, client, token, None, is_end, rt),
                    HttpWorkType::H2 => http2_connection_fast_work_until(num_connections, n_http_parallel, report_tx, client, token, None, is_end, rt),
                    HttpWorkType::H1 => http1_connection_fast_work_until(num_connections, report_tx, client, token, None, is_end, rt)
                })
            })
            .collect::<Vec<_>>();
        tokio::select! {
            _ = tokio::time::sleep_until(dead_line.into()) => {
            }
            _ = tokio::signal::ctrl_c() => {
            }
        }

        is_end.store(true, Ordering::Relaxed);

        if !wait_ongoing_requests_after_deadline {
            token.cancel();
        }
        tokio::task::block_in_place(|| {
            for handle in handles {
                let _ = handle.join();
            }
        });
    }

    /**
     * Generalised HTTP2 work function. Can be terminated in the following ways:
     *  * Set `is_end` to true. This happens when we reach a 'deadline', or are counting for a fixed period of time.
     *  * The cancellation token can also be triggered for a similar effect. These can likely be combined into one in a future CR.
     *  * pass a value for n_tasks. it will use the counter to count up to that number of requests, and terminate.
     */
    fn http2_connection_fast_work_until(
        num_connections: usize,
        n_http_parallel: usize,
        report_tx: kanal::Sender<ResultData>,
        client: Arc<Client>,
        token: tokio_util::sync::CancellationToken,
        counter: Option<Arc<AtomicIsize>>,
        is_end: Arc<AtomicBool>,
        rt: tokio::runtime::Runtime,
    ) {
        let is_counting_tasks = counter.is_some();
        let client = client.clone();
        let local = tokio::task::LocalSet::new();
        for _ in 0..num_connections {
            let report_tx = report_tx.clone();
            let client = client.clone();
            let token = token.clone();
            let is_end = is_end.clone();
            let counter = counter.clone();
            local.spawn_local(Box::pin(async move {
                let mut has_err = false;
                let mut result_data_err = ResultData::default();
                loop {
                    let client = client.clone();
                    match setup_http2(&client).await {
                        Ok((connection_time, send_request)) => {
                            let futures = (0..n_http_parallel)
                                .map(|_| {
                                    let mut client_state = ClientStateHttp2 {
                                        rng: SeedableRng::from_os_rng(),
                                        send_request: send_request.clone(),
                                    };
                                    let client = client.clone();
                                    let report_tx = report_tx.clone();
                                    let token = token.clone();
                                    let is_end = is_end.clone();
                                    let counter = counter.clone();
                                    tokio::task::spawn_local(async move {
                                        let mut result_data = ResultData::default();

                                        let work = async {
                                            loop {
                                                if is_counting_tasks {
                                                    if counter.as_ref().unwrap().fetch_sub(1, Ordering::Relaxed) <= 0  {
                                                        return true;
                                                    }
                                                }
                                                let mut res = client
                                                    .work_http2(&mut client_state)
                                                    .await;
                                                let is_cancel = is_cancel_error(&res) || is_end.load(Ordering::Relaxed);
                                                let is_reconnect = is_hyper_error(&res);
                                                set_connection_time(
                                                    &mut res,
                                                    connection_time,
                                                );

                                                result_data.push(res);

                                                if is_cancel || is_reconnect {
                                                    return is_cancel;
                                                }
                                            }
                                        };

                                        let is_cancel = tokio::select! {
                                            is_cancel = work => {
                                                is_cancel
                                            }
                                            _ = token.cancelled() => {
                                                result_data.push(Err(ClientError::Deadline));
                                                true
                                            }
                                        };

                                        report_tx.send(result_data).unwrap();
                                        is_cancel
                                    })
                                })
                                .collect::<Vec<_>>();

                            let mut connection_gone = false;
                            for f in futures {
                                match f.await {
                                    Ok(true) => {
                                        // All works done
                                        connection_gone = true;
                                    }
                                    Err(_) => {
                                        // Unexpected
                                        connection_gone = true;
                                    }
                                    _ => {}
                                }
                            }

                            if connection_gone {
                                break;
                            }
                        }
                        Err(err) => {
                            has_err = true;
                            result_data_err.push(Err(err));
                            if is_end.load(Ordering::Relaxed) ||
                             (is_counting_tasks && counter.as_ref().unwrap().fetch_sub(1, Ordering::Relaxed) <= 0)  {
                                break;
                            }
                        }
                    }
                }
                if has_err {
                    report_tx.send(result_data_err).unwrap();
                }
            }));
        }

        rt.block_on(local);
    }

    fn http1_connection_fast_work_until(
        num_connections: usize,
        report_tx: kanal::Sender<ResultData>,
        client: Arc<Client>,
        token: tokio_util::sync::CancellationToken,
        counter: Option<Arc<AtomicIsize>>,
        is_end: Arc<AtomicBool>,
        rt: tokio::runtime::Runtime,
    ) {
        let is_counting_tasks = counter.is_some();
        let local = tokio::task::LocalSet::new();

        for _ in 0..num_connections {
            let report_tx = report_tx.clone();
            let is_end = is_end.clone();
            let counter = counter.clone();
            let client = client.clone();
            let token = token.clone();
            local.spawn_local(Box::pin(async move {
                let mut result_data = ResultData::default();

                let work = async {
                    let mut client_state = ClientStateHttp1::default();
                    loop {
                        if is_counting_tasks {
                            if counter.as_ref().unwrap().fetch_sub(1, Ordering::Relaxed) <= 0 {
                                break;
                            }
                        }
                        let res = client.work_http1(&mut client_state).await;
                        let is_cancel = is_cancel_error(&res);
                        result_data.push(res);
                        if is_cancel || is_end.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                };

                tokio::select! {
                    _ = work => {
                    }
                    _ = token.cancelled() => {
                        result_data.push(Err(ClientError::Deadline));
                    }
                }
                report_tx.send(result_data).unwrap();
            }));
        }
        rt.block_on(local);
    }

}
