use rcgen::{CertificateParams, KeyPair, PKCS_RSA_SHA256};
use rustls::{
    ClientConfig, ClientConnection, KeyLogFile, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, pem::PemObject},
};

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport, adb_transport_message::ADBTransportMessage,
    },
};
use std::{
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

mod connection;
#[cfg(test)]
mod tests;
use connection::Connection;

/// Transport running on TCP
#[derive(Clone, Debug)]
pub struct TcpTransport {
    address: SocketAddr,
    timeouts: crate::ADBTimeouts,
    current_connection: Option<Arc<Connection>>,
    private_key_path: PathBuf,
}

fn certificate_from_pk(key_pair: &KeyPair) -> Result<Vec<CertificateDer<'static>>> {
    let certificate_params = CertificateParams::default();
    let certificate = certificate_params.self_signed(key_pair)?;
    Ok(vec![certificate.der().to_owned()])
}

impl TcpTransport {
    /// Instantiate a new [`TcpTransport`] using a given private key
    pub fn new<A: Into<SocketAddr>, P: AsRef<Path>>(address: A, private_key_path: P) -> Self {
        Self {
            address: address.into(),
            timeouts: crate::ADBTimeouts::default(),
            current_connection: None,
            private_key_path: private_key_path.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn set_timeouts(&mut self, timeouts: crate::ADBTimeouts) -> Result<()> {
        self.timeouts = timeouts.validate()?;
        Ok(())
    }

    pub(crate) const fn timeouts(&self) -> crate::ADBTimeouts {
        self.timeouts
    }

    fn get_current_connection(&self) -> Result<Arc<Connection>> {
        self.current_connection
            .as_ref()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            )))
            .cloned()
    }
}

impl ADBTransport for TcpTransport {
    fn connect(&mut self) -> Result<()> {
        self.current_connection = Some(Arc::new(Connection::new(match self.timeouts.handshake {
            Some(timeout) => TcpStream::connect_timeout(&self.address, timeout)?,
            None => TcpStream::connect(self.address)?,
        })));
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(connection) = &self.current_connection {
            connection.disconnect()?;
        }
        Ok(())
    }
}

impl ADBMessageTransport for TcpTransport {
    fn read_message(&mut self) -> Result<ADBTransportMessage> {
        self.read_message_with_timeout(
            self.timeouts
                .read
                .unwrap_or(crate::message_devices::adb_message_transport::DEFAULT_READ_TIMEOUT),
        )
    }
    fn write_message(&mut self, message: ADBTransportMessage) -> Result<()> {
        self.write_message_with_timeout(
            message,
            self.timeouts
                .write
                .unwrap_or(crate::message_devices::adb_message_transport::DEFAULT_READ_TIMEOUT),
        )
    }

    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
        self.get_current_connection()?.read_message(timeout)
    }

    fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        self.get_current_connection()?
            .write_message(&message, timeout)
    }

    fn upgrade_connection(&mut self) -> Result<()> {
        self.upgrade_connection_with_deadline(
            self.timeouts
                .handshake
                .and_then(|timeout| Instant::now().checked_add(timeout)),
        )
    }

    fn upgrade_connection_with_deadline(&mut self, deadline: Option<Instant>) -> Result<()> {
        let Some(connection) = self.current_connection.clone() else {
            return Err(RustADBError::UpgradeError(
                "cannot upgrade a non-existing connection".into(),
            ));
        };
        if connection.is_tls()? {
            return Err(RustADBError::UpgradeError(
                "cannot upgrade a TLS connection".into(),
            ));
        }
        let key = crate::message_devices::models::read_adb_private_key(&self.private_key_path)?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "ADB TLS needs a persisted private key",
                )
            })?;
        let pk_content = key.to_pkcs8_pem()?;
        let key_pair = KeyPair::from_pkcs8_pem_and_sign_algo(&pk_content, &PKCS_RSA_SHA256)?;
        let certificate = certificate_from_pk(&key_pair)?;
        let private_key = PrivatePkcs8KeyDer::from_pem_slice(pk_content.as_bytes())?;
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {}))
            .with_client_auth_cert(certificate, private_key.into())?;
        config.key_log = Arc::new(KeyLogFile::new());
        let client = ClientConnection::new(Arc::new(config), self.address.ip().into())?;
        connection.upgrade(client, deadline)
    }
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}
