//! Direct Android 11+ pairing-code authentication. Pairing uses its own TLS/SPAKE2
//! protocol and port, separate from ordinary ADB CNXN/AUTH/STLS messages.
//! The normal registry dependency `libadb` implements the pairing cryptography.

use crate::{Result, RustADBError};
use libadb::{
    keys::{rsa::rand_core::OsRng, store},
    pairing::pair as exchange,
    tls::{TlsClientConfig, TlsIdentity},
    transport::{tcp::TokioTcp, tls::MaybeTls},
};
use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

/// A successfully paired identity and the GUID advertised by the device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairedDevice {
    /// Device GUID, usable to match wireless-debugging mDNS advertisements.
    pub guid: String,
    /// Persisted RSA private key to reuse with `ADBTcpDevice` on the connect port.
    pub private_key_path: PathBuf,
}

/// Pair directly using the default `~/.android/adbkey` identity.
///
/// The address and six-digit code come from "Pair device with pairing code".
/// Use the separate wireless-debugging connect port for subsequent ADB commands;
/// the pairing protocol returns a GUID, not that port. Missing keys are generated
/// and persisted before pairing. Key preparation runs on a blocking worker;
/// `timeout` bounds the subsequent TCP/TLS/SPAKE2 exchange.
pub async fn pair(address: SocketAddr, code: &str, timeout: Duration) -> Result<PairedDevice> {
    let path = crate::utils::get_default_adb_key_path()?;
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ADB key path has no parent directory",
        )
    })?;
    pair_with_key_directory(address, code, directory, timeout).await
}

/// Pair directly with an existing or newly generated `directory/adbkey` identity.
///
/// Existing malformed keys are preserved and reported as errors. PKCS#8 and
/// legacy PKCS#1 identities are accepted. The wire exchange is cancelled on
/// timeout or when this future is dropped; retry with a fresh pairing port/code.
pub async fn pair_with_key_directory(
    address: SocketAddr,
    code: &str,
    directory: impl AsRef<Path>,
    timeout: Duration,
) -> Result<PairedDevice> {
    if code.len() != 6
        || !code.bytes().all(|byte| byte.is_ascii_digit())
        || timeout.is_zero()
        || address.port() == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pairing requires a six-digit code, a nonzero port and a positive timeout",
        )
        .into());
    }
    let directory = directory.as_ref().to_path_buf();
    let private_key_path = directory.join("adbkey");
    let (key, tls) = tokio::task::spawn_blocking(move || -> Result<_> {
        let key = load_pairing_key(&directory)?;
        let identity = TlsIdentity::from_key(&key, &mut OsRng).map_err(pairing_error)?;
        let tls = TlsClientConfig::adb(&identity).map_err(pairing_error)?;
        Ok((key, tls))
    })
    .await
    .map_err(io::Error::other)??;
    let operation = async {
        let socket = tokio::net::TcpStream::connect(address).await?;
        socket.set_nodelay(true)?;
        let mut transport = MaybeTls::plain(TokioTcp::new(socket));
        let paired = exchange(&mut transport, &tls, &key, code, &mut OsRng)
            .await
            .map_err(pairing_error)?;
        Ok::<_, RustADBError>(PairedDevice {
            guid: paired.guid,
            private_key_path,
        })
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

// libadb 0.6's directory creation iterates Windows prefixes and attempts
// mkdir("C:"), which fails with AccessDenied on ordinary user accounts.
// Publish a missing private key atomically first, then reuse its normal store.
#[cfg(windows)]
fn load_pairing_key(directory: &Path) -> Result<libadb::keys::AdbKey> {
    use std::{fs, io::Write};
    let path = directory.join("adbkey");
    match fs::File::open(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
            let key =
                libadb::keys::AdbKey::generate(&mut OsRng, "adb_client").map_err(pairing_error)?;
            let pem = key.to_pkcs8_pem().map_err(pairing_error)?;
            let temporary = directory.join(format!(".adbkey.{}.tmp", rand::random::<u128>()));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let published = (|| -> io::Result<()> {
                file.write_all(pem.as_bytes())?;
                file.sync_all()?;
                // Never replace an identity another process already published.
                match fs::hard_link(&temporary, &path) {
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                    result => result,
                }
            })();
            drop(file);
            let _ = fs::remove_file(&temporary);
            published?;
        }
        Err(error) => return Err(error.into()),
    }
    store::load_or_generate(directory, &mut OsRng, "adb_client").map_err(pairing_error)
}

#[cfg(not(windows))]
fn load_pairing_key(directory: &Path) -> Result<libadb::keys::AdbKey> {
    store::load_or_generate(directory, &mut OsRng, "adb_client").map_err(pairing_error)
}

fn pairing_error(error: impl std::fmt::Display) -> RustADBError {
    RustADBError::ADBRequestFailed(format!("wireless pairing: {error}"))
}

#[cfg(test)]
mod tests;
