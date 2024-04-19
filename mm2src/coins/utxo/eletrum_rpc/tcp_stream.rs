/// The enum wrapping possible variants of underlying Streams
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::large_enum_variant)]
enum ElectrumStream {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

#[cfg(not(target_arch = "wasm32"))]
impl AsRef<TcpStream> for ElectrumStream {
    fn as_ref(&self) -> &TcpStream {
        match self {
            ElectrumStream::Tcp(stream) => stream,
            ElectrumStream::Tls(stream) => stream.get_ref().0,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl AsyncRead for ElectrumStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ElectrumStream::Tcp(stream) => AsyncRead::poll_read(Pin::new(stream), cx, buf),
            ElectrumStream::Tls(stream) => AsyncRead::poll_read(Pin::new(stream), cx, buf),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl AsyncWrite for ElectrumStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ElectrumStream::Tcp(stream) => AsyncWrite::poll_write(Pin::new(stream), cx, buf),
            ElectrumStream::Tls(stream) => AsyncWrite::poll_write(Pin::new(stream), cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.get_mut() {
            ElectrumStream::Tcp(stream) => AsyncWrite::poll_flush(Pin::new(stream), cx),
            ElectrumStream::Tls(stream) => AsyncWrite::poll_flush(Pin::new(stream), cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.get_mut() {
            ElectrumStream::Tcp(stream) => AsyncWrite::poll_shutdown(Pin::new(stream), cx),
            ElectrumStream::Tls(stream) => AsyncWrite::poll_shutdown(Pin::new(stream), cx),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn rustls_client_config(unsafe_conf: bool) -> Arc<ClientConfig> {
    let mut cert_store = RootCertStore::empty();

    cert_store.add_server_trust_anchors(
        TLS_SERVER_ROOTS
            .0
            .iter()
            .map(|ta| OwnedTrustAnchor::from_subject_spki_name_constraints(ta.subject, ta.spki, ta.name_constraints)),
    );

    let mut tls_config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(cert_store)
        .with_no_client_auth();

    if unsafe_conf {
        tls_config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoCertificateVerification {}));
    }
    Arc::new(tls_config)
}

#[cfg(not(target_arch = "wasm32"))]
lazy_static! {
    static ref SAFE_TLS_CONFIG: Arc<ClientConfig> = rustls_client_config(false);
    static ref UNSAFE_TLS_CONFIG: Arc<ClientConfig> = rustls_client_config(true);
}
