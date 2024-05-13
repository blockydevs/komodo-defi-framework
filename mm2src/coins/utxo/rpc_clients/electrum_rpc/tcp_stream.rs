use futures::future::Either;
use futures::io::Error;
use http::header::AUTHORIZATION;
use http::{Request, StatusCode};
use rustls::client::ServerCertVerified;
use rustls::{Certificate, ClientConfig, OwnedTrustAnchor, RootCertStore, ServerName};
use serde_json::{self as json, Value as Json};
use std::convert::TryFrom;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::webpki::DnsNameRef;
use tokio_rustls::{client::TlsStream, TlsConnector};
use webpki_roots::TLS_SERVER_ROOTS;

/// The enum wrapping possible variants of underlying Streams
#[allow(clippy::large_enum_variant)]
pub enum ElectrumStream {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl AsRef<TcpStream> for ElectrumStream {
    fn as_ref(&self) -> &TcpStream {
        match self {
            ElectrumStream::Tcp(stream) => stream,
            ElectrumStream::Tls(stream) => stream.get_ref().0,
        }
    }
}

impl AsyncRead for ElectrumStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ElectrumStream::Tcp(stream) => AsyncRead::poll_read(Pin::new(stream), cx, buf),
            ElectrumStream::Tls(stream) => AsyncRead::poll_read(Pin::new(stream), cx, buf),
        }
    }
}

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

/// Skips the server certificate verification on TLS connection
pub struct NoCertificateVerification {}

impl rustls::client::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _: &Certificate,
        _: &[Certificate],
        _: &ServerName,
        _: &mut dyn Iterator<Item = &[u8]>,
        _: &[u8],
        _: SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

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

lazy_static! {
    pub static ref SAFE_TLS_CONFIG: Arc<ClientConfig> = rustls_client_config(false);
    pub static ref UNSAFE_TLS_CONFIG: Arc<ClientConfig> = rustls_client_config(true);
}
