use rand::RngExt;
use std::{path::Path, time::Duration};

use crate::{
    Result, RustADBError,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_session::ADBSession,
        adb_transport_message::{
            ADBTransportMessage, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN, MAX_PAYLOAD_SIZE,
        },
        message_commands::{MessageCommand, MessageSubcommand},
        models::{ADBRsaKey, read_adb_private_key},
        utils::BinaryEncodable,
    },
    models::ADBLocalCommand,
};

/// Generic structure representing an ADB device reachable over an [`ADBMessageTransport`].
/// Structure is totally agnostic over which transport is truly used.
#[derive(Debug)]
pub struct ADBMessageDevice<T: ADBMessageTransport> {
    transport: T,
    /// Maximum message payload size, negotiated with the device during the connection.
    max_payload_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyPolicy {
    GenerateAndEnroll,
    ExistingKey,
    EnrollExistingKey,
}
impl KeyPolicy {
    fn timeout(self) -> Option<Duration> {
        (self != Self::GenerateAndEnroll).then_some(Duration::from_secs(10))
    }
}

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    /// Instantiate a new [`ADBMessageDevice`].
    pub fn new<P: AsRef<Path>>(transport: T, path: P) -> Result<Self> {
        Self::new_with_policy(transport, path, KeyPolicy::GenerateAndEnroll)
    }

    /// Authenticate with an existing key without generating a key or requesting enrollment.
    pub fn new_existing_key<P: AsRef<Path>>(transport: T, path: P) -> Result<Self> {
        Self::new_with_policy(transport, path, KeyPolicy::ExistingKey)
    }

    /// Explicitly request enrollment of an existing key; never generate a replacement.
    pub fn new_authorize_existing_key<P: AsRef<Path>>(transport: T, path: P) -> Result<Self> {
        Self::new_with_policy(transport, path, KeyPolicy::EnrollExistingKey)
    }

    fn new_with_policy<P: AsRef<Path>>(
        mut transport: T,
        path: P,
        policy: KeyPolicy,
    ) -> Result<Self> {
        let private_key = if let Some(key) = read_adb_private_key(path)? {
            key
        } else if policy == KeyPolicy::GenerateAndEnroll {
            log::warn!("No private key found; generating a new random key");
            ADBRsaKey::new_random()?
        } else {
            return Err(RustADBError::ADBRequestFailed(
                "Imported ADB key is missing; no key was generated".into(),
            ));
        };
        let max_payload_size = match Self::connect(&mut transport, &private_key, policy) {
            Ok(size) => size,
            Err(error) => {
                let _ = transport.disconnect();
                return Err(error);
            }
        };
        Ok(Self {
            transport,
            max_payload_size,
        })
    }

    pub(crate) const fn get_transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Send initial connect, and return the negotiated maximum payload size
    fn connect(transport: &mut T, private_key: &ADBRsaKey, policy: KeyPolicy) -> Result<usize> {
        transport.connect()?;

        let message = ADBTransportMessage::try_new(
            MessageCommand::Cnxn,
            0x0100_0000,
            MAX_PAYLOAD_SIZE,
            format!("host::{}\0", env!("CARGO_PKG_NAME")).as_bytes(),
        )?;

        transport.write_message(message)?;

        let message = Self::read_handshake_message(transport, policy.timeout())?;

        // Check if a client is requesting a secure connection and upgrade it if necessary
        match message.header().command() {
            MessageCommand::Stls => {
                if policy == KeyPolicy::ExistingKey {
                    return Err(RustADBError::ADBRequestFailed(
                        "Existing-key USB authentication does not negotiate TLS".into(),
                    ));
                }
                transport.write_message(ADBTransportMessage::try_new(
                    MessageCommand::Stls,
                    1,
                    0,
                    &[],
                )?)?;
                transport.upgrade_connection()?;
                log::debug!("Connection successfully upgraded from TCP to TLS");

                let message = Self::read_handshake_message(transport, policy.timeout())?;
                Self::handle_connection(&message)
            }
            MessageCommand::Cnxn => {
                log::debug!("Unencrypted connection established");
                Self::handle_connection(&message)
            }
            MessageCommand::Auth => {
                log::debug!("Authentication required");
                Self::auth_handshake(transport, message, private_key, policy)
            }
            _ => Err(crate::RustADBError::WrongResponseReceived(
                "Expected CNXN, STLS or AUTH command".to_string(),
                message.header().command().to_string(),
            )),
        }
    }

    /// Read the next handshake message (`CNXN`, `AUTH` or `STLS`).
    ///
    /// Stream messages (`OKAY`, `WRTE`, `CLSE`) can still be in flight from a previous connection that was not
    /// properly closed.
    fn read_handshake_message(
        transport: &mut T,
        timeout: Option<Duration>,
    ) -> Result<ADBTransportMessage> {
        loop {
            let message = match timeout {
                Some(timeout) => transport.read_message_with_timeout(timeout)?,
                None => transport.read_message()?,
            };

            match message.header().command() {
                MessageCommand::Okay | MessageCommand::Write | MessageCommand::Clse => {
                    log::debug!(
                        "discarding stale {} message received during handshake",
                        message.header().command()
                    );
                }
                _ => return Ok(message),
            }
        }
    }

    /// Authenticate with `private_key`, and return the negotiated maximum payload size
    fn auth_handshake(
        transport: &mut T,
        message: ADBTransportMessage,
        private_key: &ADBRsaKey,
        policy: KeyPolicy,
    ) -> Result<usize> {
        // At this point, we should have received an AUTH message with arg0 == 1
        let auth_message = match message.header().arg0() {
            AUTH_TOKEN => message,
            v => {
                return Err(RustADBError::ADBRequestFailed(format!(
                    "Received AUTH message with type != 1 ({v})"
                )));
            }
        };

        let sign = private_key.sign(auth_message.into_payload())?;

        let message = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_SIGNATURE, 0, &sign)?;

        transport.write_message(message)?;

        let received_response = Self::read_handshake_message(transport, policy.timeout())?;

        if received_response.header().command() == MessageCommand::Cnxn {
            log::info!("Authentication OK");
            return Self::handle_connection(&received_response);
        }

        if policy == KeyPolicy::ExistingKey {
            return Err(RustADBError::ADBRequestFailed(
                "Imported ADB key was rejected; no authorization request was sent".into(),
            ));
        }

        let mut pubkey = private_key.android_pubkey_encode()?.into_bytes();
        pubkey.push(b'\0');

        let message =
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey)?;

        transport.write_message(message)?;

        let response = Self::read_handshake_message(transport, Some(Duration::from_secs(10)))?;

        log::info!("Authentication OK");
        Self::handle_connection(&response)
    }

    /// Handle the `CNXN` message ending the connection handshake, and return the negotiated maximum payload size.
    fn handle_connection(message: &ADBTransportMessage) -> Result<usize> {
        message.assert_command(MessageCommand::Cnxn)?;

        // The device answers with the minimum between our maximum payload size and its own
        let max_payload = message.header().arg1().min(MAX_PAYLOAD_SIZE) as usize;
        log::debug!(
            "device info {}, maximum payload size {max_payload}",
            String::from_utf8_lossy(message.payload()),
        );

        Ok(max_payload)
    }

    pub(crate) fn open_synchronization_session(&mut self) -> Result<ADBSession<T>> {
        self.open_session(&ADBLocalCommand::Sync)
    }

    /// Open a new ADB session with the given local command.
    pub(crate) fn open_session(&mut self, cmd: &ADBLocalCommand) -> Result<ADBSession<T>> {
        let mut rng = rand::rng();
        let local_id: u32 = rng.random();

        // adbd used to expect a null-terminated string.
        // keep doing so to maintain compatibility with older versions.
        // https://cs.android.com/android/platform/superproject/+/android-latest-release:packages/modules/adb/sockets.cpp;l=560?q=sockets.cpp
        let mut destination = cmd.to_string().into_bytes();
        if !destination.ends_with(&[0]) {
            destination.push(0);
        }

        let message = ADBTransportMessage::try_new(
            MessageCommand::Open,
            local_id, // Our 'local-id'
            0,
            &destination,
        )?;
        self.transport.write_message(message)?;

        let response = self.transport.read_message()?;

        if response.header().command() != MessageCommand::Okay {
            return Err(RustADBError::ADBRequestFailed(format!(
                "Open session failed: got {} in response instead of OKAY",
                response.header().command()
            )));
        }

        if response.header().arg1() != local_id {
            return Err(RustADBError::ADBRequestFailed(format!(
                "Open session failed: responses used {} for our local_id instead of {local_id}",
                response.header().arg1()
            )));
        }

        Ok(ADBSession::new(
            self.transport.clone(),
            local_id,
            response.header().arg0(),
            self.max_payload_size,
        ))
    }

    pub(crate) fn end_transaction(&mut self, session: &mut ADBSession<T>) -> Result<()> {
        let quit_buffer = MessageSubcommand::Quit.with_arg(0u32);
        session.send_and_expect_okay(ADBTransportMessage::try_new(
            MessageCommand::Write,
            session.local_id(),
            session.remote_id(),
            &quit_buffer.encode(),
        )?)?;

        let _discard_close = self.transport.read_message()?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod existing_key_tests {
    use super::*;
    use crate::adb_transport::ADBTransport;
    use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};
    use std::{
        collections::VecDeque,
        fs, io,
        path::PathBuf,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicU64, Ordering},
        },
    };

    #[derive(Debug, Default)]
    struct State {
        connected: bool,
        reads: VecDeque<ADBTransportMessage>,
        writes: Vec<(MessageCommand, u32)>,
        read_timeouts: Vec<Duration>,
    }
    #[derive(Debug, Clone)]
    struct Fake(Arc<Mutex<State>>);
    impl ADBTransport for Fake {
        fn connect(&mut self) -> Result<()> {
            self.0.lock().unwrap().connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }
    impl ADBMessageTransport for Fake {
        fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
            let mut state = self.0.lock().unwrap();
            state.read_timeouts.push(timeout);
            state
                .reads
                .pop_front()
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof).into())
        }
        fn write_message_with_timeout(
            &mut self,
            message: ADBTransportMessage,
            _: Duration,
        ) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .writes
                .push((message.header().command(), message.header().arg0()));
            Ok(())
        }
    }
    struct Key(PathBuf);
    impl Key {
        fn new() -> Self {
            static PEM: OnceLock<String> = OnceLock::new();
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let pem = PEM.get_or_init(|| {
                RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
                    .unwrap()
                    .to_pkcs8_pem(Default::default())
                    .unwrap()
                    .to_string()
            });
            let path = std::env::temp_dir().join(format!(
                "adb-existing-key-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            f.write_all(pem.as_bytes()).unwrap();
            Self(path)
        }
    }
    impl Drop for Key {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn handshake(accept: bool) -> Fake {
        let token =
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_TOKEN, 0, &[7; 20]).unwrap();
        let response = if accept {
            ADBTransportMessage::try_new(MessageCommand::Cnxn, 0x01000000, 4096, b"device::\0")
                .unwrap()
        } else {
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_TOKEN, 0, &[8; 20]).unwrap()
        };
        Fake(Arc::new(Mutex::new(State {
            reads: VecDeque::from([token, response]),
            ..State::default()
        })))
    }

    #[test]
    fn accepted_imported_key_sends_only_its_signature() {
        let key = Key::new();
        let fake = handshake(true);
        assert!(ADBMessageDevice::new_existing_key(fake.clone(), &key.0).is_ok());
        let s = fake.0.lock().unwrap();
        assert_eq!(s.writes.len(), 2);
        assert_eq!(s.writes[1], (MessageCommand::Auth, AUTH_SIGNATURE));
    }

    #[test]
    fn rejected_imported_key_never_sends_an_enrollment_request() {
        let key = Key::new();
        let fake = handshake(false);
        assert!(ADBMessageDevice::new_existing_key(fake.clone(), &key.0).is_err());
        let s = fake.0.lock().unwrap();
        assert_eq!(s.writes.len(), 2);
        assert!(
            !s.writes
                .contains(&(MessageCommand::Auth, AUTH_RSAPUBLICKEY))
        );
    }

    #[test]
    fn missing_imported_key_fails_before_connecting_or_generating_a_key() {
        let fake = handshake(false);
        let path = std::env::temp_dir().join(format!("adb-missing-key-{}", std::process::id()));
        assert!(!path.exists());
        assert!(ADBMessageDevice::new_existing_key(fake.clone(), &path).is_err());
        assert!(!path.exists());
        let s = fake.0.lock().unwrap();
        assert!(!s.connected);
        assert!(s.writes.is_empty());
    }

    #[test]
    fn explicit_enrollment_uses_existing_key_and_requires_device_acceptance() {
        let key = Key::new();
        let before = fs::read(&key.0).unwrap();
        let fake = handshake(false);
        fake.0.lock().unwrap().reads.push_back(
            ADBTransportMessage::try_new(MessageCommand::Cnxn, 0x01000000, 4096, b"device::\0")
                .unwrap(),
        );
        assert!(ADBMessageDevice::new_authorize_existing_key(fake.clone(), &key.0).is_ok());
        assert_eq!(
            fake.0.lock().unwrap().writes.last().unwrap(),
            &(MessageCommand::Auth, AUTH_RSAPUBLICKEY)
        );
        assert_eq!(
            fake.0.lock().unwrap().read_timeouts,
            vec![Duration::from_secs(10); 3]
        );
        assert_eq!(fs::read(&key.0).unwrap(), before);
        let absent =
            std::env::temp_dir().join(format!("adb-enrollment-missing-{}", std::process::id()));
        let fake = handshake(false);
        assert!(ADBMessageDevice::new_authorize_existing_key(fake.clone(), &absent).is_err());
        assert!(!fake.0.lock().unwrap().connected);
        assert!(!absent.exists());
    }
}

impl<T: ADBMessageTransport> Drop for ADBMessageDevice<T> {
    fn drop(&mut self) {
        // Best effort here
        let _ = self.get_transport_mut().disconnect();
    }
}
