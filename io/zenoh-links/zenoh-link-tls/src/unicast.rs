//
// Copyright (c) 2023 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use crate::{
    base64_decode, config::*, get_tls_addr, get_tls_host, get_tls_server_name,
    verify::WebPkiVerifierAnyServerName, TLS_ACCEPT_THROTTLE_TIME, TLS_DEFAULT_MTU,
    TLS_LINGER_TIMEOUT, TLS_LOCATOR_PREFIX,
};
use async_rustls::{
    rustls::{
        server::AllowAnyAuthenticatedClient, version::TLS13, Certificate, ClientConfig,
        OwnedTrustAnchor, PrivateKey, RootCertStore, ServerConfig,
    },
    TlsAcceptor, TlsConnector, TlsStream,
};
use async_std::fs;
use async_std::net::{SocketAddr, TcpListener, TcpStream};
use async_std::prelude::FutureExt;
use async_std::sync::Mutex as AsyncMutex;
use async_std::task;
use async_trait::async_trait;
use futures::io::AsyncReadExt;
use futures::io::AsyncWriteExt;
use std::convert::TryInto;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{cell::UnsafeCell, io};
use webpki::{
    anchor_from_trusted_cert,
    types::{CertificateDer, TrustAnchor},
};
use zenoh_core::zasynclock;
use zenoh_link_commons::{
    get_ip_interface_names, LinkManagerUnicastTrait, LinkUnicast, LinkUnicastTrait,
    ListenersUnicastIP, NewLinkChannelSender,
};
use zenoh_protocol::core::endpoint::Config;
use zenoh_protocol::core::{EndPoint, Locator};
use zenoh_result::{bail, zerror, ZError, ZResult};
use zenoh_sync::Signal;

pub struct LinkUnicastTls {
    // The underlying socket as returned from the async-rustls library
    // NOTE: TlsStream requires &mut for read and write operations. This means
    //       that concurrent reads and writes are not possible. To achieve that,
    //       we use an UnsafeCell for interior mutability. Using an UnsafeCell
    //       is safe in our case since the transmission and reception logic
    //       already ensures that no concurrent reads or writes can happen on
    //       the same stream: there is only one task at the time that writes on
    //       the stream and only one task at the time that reads from the stream.
    inner: UnsafeCell<TlsStream<TcpStream>>,
    // The source socket address of this link (address used on the local host)
    src_addr: SocketAddr,
    src_locator: Locator,
    // The destination socket address of this link (address used on the local host)
    dst_addr: SocketAddr,
    dst_locator: Locator,
    // Make sure there are no concurrent read or writes
    write_mtx: AsyncMutex<()>,
    read_mtx: AsyncMutex<()>,
}

unsafe impl Send for LinkUnicastTls {}
unsafe impl Sync for LinkUnicastTls {}

impl LinkUnicastTls {
    fn new(
        socket: TlsStream<TcpStream>,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
    ) -> LinkUnicastTls {
        let (tcp_stream, _) = socket.get_ref();
        // Set the TLS nodelay option
        if let Err(err) = tcp_stream.set_nodelay(true) {
            log::warn!(
                "Unable to set NODEALY option on TLS link {} => {}: {}",
                src_addr,
                dst_addr,
                err
            );
        }

        // Set the TLS linger option
        if let Err(err) = zenoh_util::net::set_linger(
            tcp_stream,
            Some(Duration::from_secs(
                (*TLS_LINGER_TIMEOUT).try_into().unwrap(),
            )),
        ) {
            log::warn!(
                "Unable to set LINGER option on TLS link {} => {}: {}",
                src_addr,
                dst_addr,
                err
            );
        }

        // Build the Tls object
        LinkUnicastTls {
            inner: UnsafeCell::new(socket),
            src_addr,
            src_locator: Locator::new(TLS_LOCATOR_PREFIX, src_addr.to_string(), "").unwrap(),
            dst_addr,
            dst_locator: Locator::new(TLS_LOCATOR_PREFIX, dst_addr.to_string(), "").unwrap(),
            write_mtx: AsyncMutex::new(()),
            read_mtx: AsyncMutex::new(()),
        }
    }

    // NOTE: It is safe to suppress Clippy warning since no concurrent reads
    //       or concurrent writes will ever happen. The read_mtx and write_mtx
    //       are respectively acquired in any read and write operation.
    #[allow(clippy::mut_from_ref)]
    fn get_sock_mut(&self) -> &mut TlsStream<TcpStream> {
        unsafe { &mut *self.inner.get() }
    }
}

#[async_trait]
impl LinkUnicastTrait for LinkUnicastTls {
    async fn close(&self) -> ZResult<()> {
        log::trace!("Closing TLS link: {}", self);
        // Flush the TLS stream
        let _guard = zasynclock!(self.write_mtx);
        let tls_stream = self.get_sock_mut();
        let res = tls_stream.flush().await;
        log::trace!("TLS link flush {}: {:?}", self, res);
        // Close the underlying TCP stream
        let (tcp_stream, _) = tls_stream.get_ref();
        let res = tcp_stream.shutdown(Shutdown::Both);
        log::trace!("TLS link shutdown {}: {:?}", self, res);
        res.map_err(|e| zerror!(e).into())
    }

    async fn write(&self, buffer: &[u8]) -> ZResult<usize> {
        let _guard = zasynclock!(self.write_mtx);
        self.get_sock_mut().write(buffer).await.map_err(|e| {
            log::trace!("Write error on TLS link {}: {}", self, e);
            zerror!(e).into()
        })
    }

    async fn write_all(&self, buffer: &[u8]) -> ZResult<()> {
        let _guard = zasynclock!(self.write_mtx);
        self.get_sock_mut().write_all(buffer).await.map_err(|e| {
            log::trace!("Write error on TLS link {}: {}", self, e);
            zerror!(e).into()
        })
    }

    async fn read(&self, buffer: &mut [u8]) -> ZResult<usize> {
        let _guard = zasynclock!(self.read_mtx);
        self.get_sock_mut().read(buffer).await.map_err(|e| {
            log::trace!("Read error on TLS link {}: {}", self, e);
            zerror!(e).into()
        })
    }

    async fn read_exact(&self, buffer: &mut [u8]) -> ZResult<()> {
        let _guard = zasynclock!(self.read_mtx);
        self.get_sock_mut().read_exact(buffer).await.map_err(|e| {
            log::trace!("Read error on TLS link {}: {}", self, e);
            zerror!(e).into()
        })
    }

    #[inline(always)]
    fn get_src(&self) -> &Locator {
        &self.src_locator
    }

    #[inline(always)]
    fn get_dst(&self) -> &Locator {
        &self.dst_locator
    }

    #[inline(always)]
    fn get_mtu(&self) -> u16 {
        *TLS_DEFAULT_MTU
    }

    #[inline(always)]
    fn get_interface_names(&self) -> Vec<String> {
        get_ip_interface_names(&self.src_addr)
    }

    #[inline(always)]
    fn is_reliable(&self) -> bool {
        true
    }

    #[inline(always)]
    fn is_streamed(&self) -> bool {
        true
    }
}

impl Drop for LinkUnicastTls {
    fn drop(&mut self) {
        // Close the underlying TCP stream
        let (tcp_stream, _) = self.get_sock_mut().get_ref();
        let _ = tcp_stream.shutdown(Shutdown::Both);
    }
}

impl fmt::Display for LinkUnicastTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} => {}", self.src_addr, self.dst_addr)?;
        Ok(())
    }
}

impl fmt::Debug for LinkUnicastTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tls")
            .field("src", &self.src_addr)
            .field("dst", &self.dst_addr)
            .finish()
    }
}

pub struct LinkManagerUnicastTls {
    manager: NewLinkChannelSender,
    listeners: ListenersUnicastIP,
}

impl LinkManagerUnicastTls {
    pub fn new(manager: NewLinkChannelSender) -> Self {
        Self {
            manager,
            listeners: ListenersUnicastIP::new(),
        }
    }
}

#[async_trait]
impl LinkManagerUnicastTrait for LinkManagerUnicastTls {
    async fn new_link(&self, endpoint: EndPoint) -> ZResult<LinkUnicast> {
        let epaddr = endpoint.address();
        let epconf = endpoint.config();

        let server_name = get_tls_server_name(&epaddr)?;
        let addr = get_tls_addr(&epaddr).await?;

        // Initialize the TLS Config
        let client_config = TlsClientConfig::new(&epconf)
            .await
            .map_err(|e| zerror!("Cannot create a new TLS listener to {endpoint}: {e}"))?;
        let config = Arc::new(client_config.client_config);
        let connector = TlsConnector::from(config);

        // Initialize the TcpStream
        let tcp_stream = TcpStream::connect(addr).await.map_err(|e| {
            zerror!(
                "Can not create a new TLS link bound to {:?}: {}",
                server_name,
                e
            )
        })?;

        let src_addr = tcp_stream.local_addr().map_err(|e| {
            zerror!(
                "Can not create a new TLS link bound to {:?}: {}",
                server_name,
                e
            )
        })?;

        let dst_addr = tcp_stream.peer_addr().map_err(|e| {
            zerror!(
                "Can not create a new TLS link bound to {:?}: {}",
                server_name,
                e
            )
        })?;

        // Initialize the TlsStream
        let tls_stream = connector
            .connect(server_name.to_owned(), tcp_stream)
            .await
            .map_err(|e| {
                zerror!(
                    "Can not create a new TLS link bound to {:?}: {}",
                    server_name,
                    e
                )
            })?;
        let tls_stream = TlsStream::Client(tls_stream);

        let link = Arc::new(LinkUnicastTls::new(tls_stream, src_addr, dst_addr));

        Ok(LinkUnicast(link))
    }

    async fn new_listener(&self, endpoint: EndPoint) -> ZResult<Locator> {
        let epaddr = endpoint.address();
        let epconf = endpoint.config();

        let addr = get_tls_addr(&epaddr).await?;
        let host = get_tls_host(&epaddr)?;

        // Initialize TlsConfig
        let tls_server_config = TlsServerConfig::new(&epconf)
            .await
            .map_err(|e| zerror!("Cannot create a new TLS listener on {addr}. {e}"))?;

        // Initialize the TcpListener
        let socket = TcpListener::bind(addr)
            .await
            .map_err(|e| zerror!("Can not create a new TLS listener on {}: {}", addr, e))?;

        let local_addr = socket
            .local_addr()
            .map_err(|e| zerror!("Can not create a new TLS listener on {}: {}", addr, e))?;
        let local_port = local_addr.port();

        // Initialize the TlsAcceptor
        let acceptor = TlsAcceptor::from(Arc::new(tls_server_config.server_config));
        let active = Arc::new(AtomicBool::new(true));
        let signal = Signal::new();

        let c_active = active.clone();
        let c_signal = signal.clone();
        let c_manager = self.manager.clone();

        let handle = task::spawn(async move {
            accept_task(socket, acceptor, c_active, c_signal, c_manager).await
        });

        // Update the endpoint locator address
        let locator = Locator::new(
            endpoint.protocol(),
            format!("{host}:{local_port}"),
            endpoint.metadata(),
        )?;

        self.listeners
            .add_listener(endpoint, local_addr, active, signal, handle)
            .await?;

        Ok(locator)
    }

    async fn del_listener(&self, endpoint: &EndPoint) -> ZResult<()> {
        let epaddr = endpoint.address();
        let addr = get_tls_addr(&epaddr).await?;
        self.listeners.del_listener(addr).await
    }

    fn get_listeners(&self) -> Vec<EndPoint> {
        self.listeners.get_endpoints()
    }

    fn get_locators(&self) -> Vec<Locator> {
        self.listeners.get_locators()
    }
}

async fn accept_task(
    socket: TcpListener,
    acceptor: TlsAcceptor,
    active: Arc<AtomicBool>,
    signal: Signal,
    manager: NewLinkChannelSender,
) -> ZResult<()> {
    enum Action {
        Accept((TcpStream, SocketAddr)),
        Stop,
    }

    async fn accept(socket: &TcpListener) -> ZResult<Action> {
        let res = socket.accept().await.map_err(|e| zerror!(e))?;
        Ok(Action::Accept(res))
    }

    async fn stop(signal: Signal) -> ZResult<Action> {
        signal.wait().await;
        Ok(Action::Stop)
    }

    let src_addr = socket.local_addr().map_err(|e| {
        let e = zerror!("Can not accept TLS connections: {}", e);
        log::warn!("{}", e);
        e
    })?;

    log::trace!("Ready to accept TLS connections on: {:?}", src_addr);
    while active.load(Ordering::Acquire) {
        // Wait for incoming connections
        let (tcp_stream, dst_addr) = match accept(&socket).race(stop(signal.clone())).await {
            Ok(action) => match action {
                Action::Accept((tcp_stream, dst_addr)) => (tcp_stream, dst_addr),
                Action::Stop => break,
            },
            Err(e) => {
                log::warn!("{}. Hint: increase the system open file limit.", e);
                // Throttle the accept loop upon an error
                // NOTE: This might be due to various factors. However, the most common case is that
                //       the process has reached the maximum number of open files in the system. On
                //       Linux systems this limit can be changed by using the "ulimit" command line
                //       tool. In case of systemd-based systems, this can be changed by using the
                //       "sysctl" command line tool.
                task::sleep(Duration::from_micros(*TLS_ACCEPT_THROTTLE_TIME)).await;
                continue;
            }
        };
        // Accept the TLS connection
        let tls_stream = match acceptor.accept(tcp_stream).await {
            Ok(stream) => TlsStream::Server(stream),
            Err(e) => {
                let e = format!("Can not accept TLS connection: {e}");
                log::warn!("{}", e);
                continue;
            }
        };

        log::debug!("Accepted TLS connection on {:?}: {:?}", src_addr, dst_addr);
        // Create the new link object
        let link = Arc::new(LinkUnicastTls::new(tls_stream, src_addr, dst_addr));

        // Communicate the new link to the initial transport manager
        if let Err(e) = manager.send_async(LinkUnicast(link)).await {
            log::error!("{}-{}: {}", file!(), line!(), e)
        }
    }

    Ok(())
}

struct TlsServerConfig {
    server_config: ServerConfig,
}

impl TlsServerConfig {
    pub async fn new(config: &Config<'_>) -> ZResult<TlsServerConfig> {
        let tls_server_client_auth: bool = match config.get(TLS_CLIENT_AUTH) {
            Some(s) => s
                .parse()
                .map_err(|_| zerror!("Unknown client auth argument: {}", s))?,
            None => false,
        };
        let tls_server_private_key = TlsServerConfig::load_tls_private_key(config).await?;
        let tls_server_certificate = TlsServerConfig::load_tls_certificate(config).await?;

        let certs: Vec<Certificate> =
            rustls_pemfile::certs(&mut Cursor::new(&tls_server_certificate))
                .map(|result| {
                    result
                        .map_err(|err| zerror!("Error processing server certificate: {err}."))
                        .map(|der| Certificate(der.to_vec()))
                })
                .collect::<Result<Vec<Certificate>, ZError>>()?;

        let mut keys: Vec<PrivateKey> =
            rustls_pemfile::rsa_private_keys(&mut Cursor::new(&tls_server_private_key))
                .map(|result| {
                    result
                        .map_err(|err| zerror!("Error processing server key: {err}."))
                        .map(|key| PrivateKey(key.secret_pkcs1_der().to_vec()))
                })
                .collect::<Result<Vec<PrivateKey>, ZError>>()?;

        if keys.is_empty() {
            keys = rustls_pemfile::pkcs8_private_keys(&mut Cursor::new(&tls_server_private_key))
                .map(|result| {
                    result
                        .map_err(|err| zerror!("Error processing server key: {err}."))
                        .map(|key| PrivateKey(key.secret_pkcs8_der().to_vec()))
                })
                .collect::<Result<Vec<PrivateKey>, ZError>>()?;
        }

        if keys.is_empty() {
            keys = rustls_pemfile::ec_private_keys(&mut Cursor::new(&tls_server_private_key))
                .map(|result| {
                    result
                        .map_err(|err| zerror!("Error processing server key: {err}."))
                        .map(|key| PrivateKey(key.secret_sec1_der().to_vec()))
                })
                .collect::<Result<Vec<PrivateKey>, ZError>>()?;
        }

        if keys.is_empty() {
            bail!("No private key found for TLS server.");
        }

        let sc = if tls_server_client_auth {
            let root_cert_store = load_trust_anchors(config)?.map_or_else(
                || {
                    Err(zerror!(
                        "Missing root certificates while client authentication is enabled."
                    ))
                },
                Ok,
            )?;
            ServerConfig::builder()
                .with_safe_default_cipher_suites()
                .with_safe_default_kx_groups()
                .with_protocol_versions(&[&TLS13]) // Force TLS 1.3
                .map_err(|e| zerror!(e))?
                .with_client_cert_verifier(Arc::new(AllowAnyAuthenticatedClient::new(root_cert_store)))
                .with_single_cert(certs, keys.remove(0))
                .map_err(|e| zerror!(e))?
        } else {
            ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(certs, keys.remove(0))
                .map_err(|e| zerror!(e))?
        };
        Ok(TlsServerConfig { server_config: sc })
    }

    async fn load_tls_private_key(config: &Config<'_>) -> ZResult<Vec<u8>> {
        load_tls_key(
            config,
            TLS_SERVER_PRIVATE_KEY_RAW,
            TLS_SERVER_PRIVATE_KEY_FILE,
            TLS_SERVER_PRIVATE_KEY_BASE_64,
        )
        .await
    }

    async fn load_tls_certificate(config: &Config<'_>) -> ZResult<Vec<u8>> {
        load_tls_certificate(
            config,
            TLS_SERVER_CERTIFICATE_RAW,
            TLS_SERVER_CERTIFICATE_FILE,
            TLS_SERVER_CERTIFICATE_BASE64,
        )
        .await
    }
}

struct TlsClientConfig {
    client_config: ClientConfig,
}

impl TlsClientConfig {
    pub async fn new(config: &Config<'_>) -> ZResult<TlsClientConfig> {
        let tls_client_server_auth: bool = match config.get(TLS_CLIENT_AUTH) {
            Some(s) => s
                .parse()
                .map_err(|_| zerror!("Unknown client auth argument: {}", s))?,
            None => false,
        };

        let tls_server_name_verification: bool = match config.get(TLS_SERVER_NAME_VERIFICATION) {
            Some(s) => {
                let s: bool = s
                    .parse()
                    .map_err(|_| zerror!("Unknown server name verification argument: {}", s))?;
                if s {
                    log::warn!("Skipping name verification of servers");
                }
                s
            }
            None => false,
        };

        // Allows mixed user-generated CA and webPKI CA
        log::debug!("Loading default Web PKI certificates.");
        let mut root_cert_store: RootCertStore = RootCertStore {
            roots: load_default_webpki_certs().roots,
        };

        if let Some(custom_root_cert) = load_trust_anchors(config)? {
            log::debug!("Loading user-generated certificates.");
            root_cert_store.add_trust_anchors(custom_root_cert.roots.into_iter());
        }

        let cc = if tls_client_server_auth {
            log::debug!("Loading client authentication key and certificate...");
            let tls_client_private_key = TlsClientConfig::load_tls_private_key(config).await?;
            let tls_client_certificate = TlsClientConfig::load_tls_certificate(config).await?;

            let certs: Vec<Certificate> =
                rustls_pemfile::certs(&mut Cursor::new(&tls_client_certificate))
                    .map(|result| {
                        result
                            .map_err(|err| zerror!("Error processing client certificate: {err}."))
                            .map(|der| Certificate(der.to_vec()))
                    })
                    .collect::<Result<Vec<Certificate>, ZError>>()?;

            let mut keys: Vec<PrivateKey> =
                rustls_pemfile::rsa_private_keys(&mut Cursor::new(&tls_client_private_key))
                    .map(|result| {
                        result
                            .map_err(|err| zerror!("Error processing client key: {err}."))
                            .map(|key| PrivateKey(key.secret_pkcs1_der().to_vec()))
                    })
                    .collect::<Result<Vec<PrivateKey>, ZError>>()?;

            if keys.is_empty() {
                keys =
                    rustls_pemfile::pkcs8_private_keys(&mut Cursor::new(&tls_client_private_key))
                        .map(|result| {
                            result
                                .map_err(|err| zerror!("Error processing client key: {err}."))
                                .map(|key| PrivateKey(key.secret_pkcs8_der().to_vec()))
                        })
                        .collect::<Result<Vec<PrivateKey>, ZError>>()?;
            }

            if keys.is_empty() {
                keys = rustls_pemfile::ec_private_keys(&mut Cursor::new(&tls_client_private_key))
                    .map(|result| {
                        result
                            .map_err(|err| zerror!("Error processing client key: {err}."))
                            .map(|key| PrivateKey(key.secret_sec1_der().to_vec()))
                    })
                    .collect::<Result<Vec<PrivateKey>, ZError>>()?;
            }

            if keys.is_empty() {
                bail!("No private key found for TLS client.");
            }

            let builder = ClientConfig::builder()
                .with_safe_default_cipher_suites()
                .with_safe_default_kx_groups()
                .with_protocol_versions(&[&TLS13])
                .map_err(|e| zerror!("Config parameters should be valid: {}", e))?;

            if tls_server_name_verification {
                builder
                    .with_root_certificates(root_cert_store)
                    .with_client_auth_cert(certs, keys.remove(0))
            } else {
                builder
                    .with_custom_certificate_verifier(Arc::new(WebPkiVerifierAnyServerName::new(
                        root_cert_store,
                    )))
                    .with_client_auth_cert(certs, keys.remove(0))
            }
            .map_err(|e| zerror!("Bad certificate/key: {}", e))?
        } else {
            let builder = ClientConfig::builder().with_safe_defaults();
            if tls_server_name_verification {
                builder
                    .with_root_certificates(root_cert_store)
                    .with_no_client_auth()
            } else {
                builder
                    .with_custom_certificate_verifier(Arc::new(WebPkiVerifierAnyServerName::new(
                        root_cert_store,
                    )))
                    .with_no_client_auth()
            }
        };
        Ok(TlsClientConfig { client_config: cc })
    }

    async fn load_tls_private_key(config: &Config<'_>) -> ZResult<Vec<u8>> {
        load_tls_key(
            config,
            TLS_CLIENT_PRIVATE_KEY_RAW,
            TLS_CLIENT_PRIVATE_KEY_FILE,
            TLS_CLIENT_PRIVATE_KEY_BASE64,
        )
        .await
    }

    async fn load_tls_certificate(config: &Config<'_>) -> ZResult<Vec<u8>> {
        load_tls_certificate(
            config,
            TLS_CLIENT_CERTIFICATE_RAW,
            TLS_CLIENT_CERTIFICATE_FILE,
            TLS_CLIENT_CERTIFICATE_BASE64,
        )
        .await
    }
}

async fn load_tls_key(
    config: &Config<'_>,
    tls_private_key_raw_config_key: &str,
    tls_private_key_file_config_key: &str,
    tls_private_key_base64_config_key: &str,
) -> ZResult<Vec<u8>> {
    if let Some(value) = config.get(tls_private_key_raw_config_key) {
        return Ok(value.as_bytes().to_vec());
    } else if let Some(b64_key) = config.get(tls_private_key_base64_config_key) {
        return base64_decode(b64_key);
    } else if let Some(value) = config.get(tls_private_key_file_config_key) {
        return Ok(fs::read(value)
            .await
            .map_err(|e| zerror!("Invalid TLS private key file: {}", e))?)
        .and_then(|result| {
            if result.is_empty() {
                Err(zerror!("Empty TLS key.").into())
            } else {
                Ok(result)
            }
        });
    }
    Err(zerror!("Missing TLS private key.").into())
}

async fn load_tls_certificate(
    config: &Config<'_>,
    tls_certificate_raw_config_key: &str,
    tls_certificate_file_config_key: &str,
    tls_certificate_base64_config_key: &str,
) -> ZResult<Vec<u8>> {
    if let Some(value) = config.get(tls_certificate_raw_config_key) {
        return Ok(value.as_bytes().to_vec());
    } else if let Some(b64_certificate) = config.get(tls_certificate_base64_config_key) {
        return base64_decode(b64_certificate);
    } else if let Some(value) = config.get(tls_certificate_file_config_key) {
        return Ok(fs::read(value)
            .await
            .map_err(|e| zerror!("Invalid TLS certificate file: {}", e))?);
    }
    Err(zerror!("Missing tls certificates.").into())
}

fn load_trust_anchors(config: &Config<'_>) -> ZResult<Option<RootCertStore>> {
    let mut root_cert_store = RootCertStore::empty();
    if let Some(value) = config.get(TLS_ROOT_CA_CERTIFICATE_RAW) {
        let mut pem = BufReader::new(value.as_bytes());
        let trust_anchors = process_pem(&mut pem)?;
        root_cert_store.add_trust_anchors(trust_anchors.into_iter());
        return Ok(Some(root_cert_store));
    }

    if let Some(b64_certificate) = config.get(TLS_ROOT_CA_CERTIFICATE_BASE64) {
        let certificate_pem = base64_decode(b64_certificate)?;
        let mut pem = BufReader::new(certificate_pem.as_slice());
        let trust_anchors = process_pem(&mut pem)?;
        root_cert_store.add_trust_anchors(trust_anchors.into_iter());
        return Ok(Some(root_cert_store));
    }

    if let Some(filename) = config.get(TLS_ROOT_CA_CERTIFICATE_FILE) {
        let mut pem = BufReader::new(File::open(filename)?);
        let trust_anchors = process_pem(&mut pem)?;
        root_cert_store.add_trust_anchors(trust_anchors.into_iter());
        return Ok(Some(root_cert_store));
    }
    Ok(None)
}

fn process_pem(pem: &mut dyn io::BufRead) -> ZResult<Vec<OwnedTrustAnchor>> {
    let certs: Vec<CertificateDer> = rustls_pemfile::certs(pem)
        .map(|result| result.map_err(|err| zerror!("Error processing PEM certificates: {err}.")))
        .collect::<Result<Vec<CertificateDer>, ZError>>()?;

    let trust_anchors: Vec<TrustAnchor> = certs
        .into_iter()
        .map(|cert| {
            anchor_from_trusted_cert(&cert)
                .map_err(|err| zerror!("Error processing trust anchor: {err}."))
                .map(|trust_anchor| trust_anchor.to_owned())
        })
        .collect::<Result<Vec<TrustAnchor>, ZError>>()?;

    let owned_trust_anchors: Vec<OwnedTrustAnchor> = trust_anchors
        .into_iter()
        .map(|ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject.to_vec(),
                ta.subject_public_key_info.to_vec(),
                ta.name_constraints.map(|x| x.to_vec()),
            )
        })
        .collect();

    Ok(owned_trust_anchors)
}

fn load_default_webpki_certs() -> RootCertStore {
    let mut root_cert_store = RootCertStore::empty();
    root_cert_store.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject.to_vec(),
            ta.subject_public_key_info.to_vec(),
            ta.name_constraints.clone().map(|x| x.to_vec()),
        )
    }));
    root_cert_store
}
