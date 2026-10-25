use super::*;
use crate::{ADBDeviceExt, ADBTransport};
use std::{
    io::{Error, ErrorKind, Read},
    sync::{Arc, Mutex, mpsc},
};

type OpenScript = fn(u32, &mpsc::Sender<ADBTransportMessage>);

#[derive(Clone, Debug)]
struct FakeTransport {
    incoming: Arc<Mutex<mpsc::Receiver<ADBTransportMessage>>>,
    replies: mpsc::Sender<ADBTransportMessage>,
    sent: Arc<Mutex<Vec<ADBTransportMessage>>>,
    shell_output: Option<Vec<u8>>,
    on_open: Option<OpenScript>,
}

impl ADBTransport for FakeTransport {
    fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

fn packet(command: MessageCommand, remote: u32, local: u32, bytes: &[u8]) -> ADBTransportMessage {
    ADBTransportMessage::try_new(command, remote, local, bytes).unwrap()
}

impl ADBMessageTransport for FakeTransport {
    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
        self.incoming
            .lock()
            .unwrap()
            .recv_timeout(timeout.min(Duration::from_secs(2)))
            .map_err(|err| Error::new(ErrorKind::TimedOut, err).into())
    }

    fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        _: Duration,
    ) -> Result<()> {
        let command = message.header().command();
        let local = message.header().arg0();
        let remote = message.header().arg1();
        self.sent.lock().unwrap().push(message);
        match command {
            MessageCommand::Open => {
                if let Some(script) = self.on_open {
                    script(local, &self.replies);
                    return Ok(());
                }
                self.replies
                    .send(packet(MessageCommand::Okay, 7, local, &[]))
                    .unwrap();
                if let Some(output) = &self.shell_output {
                    self.replies
                        .send(packet(MessageCommand::Write, 7, local, output))
                        .unwrap();
                    self.replies
                        .send(packet(MessageCommand::Clse, 7, local, &[]))
                        .unwrap();
                }
            }
            MessageCommand::Clse => {
                self.replies
                    .send(packet(MessageCommand::Clse, remote, local, &[]))
                    .unwrap();
            }
            _ => {}
        }
        Ok(())
    }
}

fn device(output: Option<&[u8]>) -> ADBMessageDevice<FakeTransport> {
    let (replies, incoming) = mpsc::channel();
    ADBMessageDevice {
        transport: FakeTransport {
            incoming: Arc::new(Mutex::new(incoming)),
            replies,
            sent: Arc::default(),
            shell_output: output.map(<[u8]>::to_vec),
            on_open: None,
        },
        max_payload_size: 4096,
    }
}

fn scripted_device(script: OpenScript) -> ADBMessageDevice<FakeTransport> {
    let mut device = device(None);
    device.transport.on_open = Some(script);
    device
}

fn stale_local(local: u32) -> u32 {
    if local == 1 { 2 } else { 1 }
}

fn queue_open(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
    for message in [
        packet(MessageCommand::Okay, 17, stale_local(local), &[]),
        packet(MessageCommand::Clse, 17, stale_local(local), &[]),
        packet(MessageCommand::Write, 17, stale_local(local), b"stale"),
        packet(MessageCommand::Okay, 7, local, &[]),
    ] {
        replies.send(message).unwrap();
    }
}

#[test]
fn open_waits_for_its_own_ready_without_acknowledging_stale_packets() {
    let mut device = scripted_device(queue_open);
    let session = device.open_session(&ADBLocalCommand::Shell).unwrap();
    assert_ne!(session.local_id(), 0);
    assert_eq!(session.remote_id(), 7);
    let sent = device.transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header().command(), MessageCommand::Open);
}

#[test]
fn open_still_rejects_current_close_early_data_and_zero_remote_id() {
    fn rejected(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
        replies
            .send(packet(MessageCommand::Clse, 0, local, &[]))
            .unwrap();
    }
    fn early_data(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
        replies
            .send(packet(MessageCommand::Write, 7, local, b"early"))
            .unwrap();
    }
    fn zero_remote(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
        replies
            .send(packet(MessageCommand::Okay, 0, local, &[]))
            .unwrap();
    }
    for script in [rejected as OpenScript, early_data, zero_remote] {
        let mut device = scripted_device(script);
        assert!(device.open_session(&ADBLocalCommand::Shell).is_err());
        assert_eq!(device.transport.sent.lock().unwrap().len(), 1);
    }
}

fn queue_shell(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
    queue_shell_with_close(local, replies, 7);
}

fn queue_shell_with_close(
    local: u32,
    replies: &mpsc::Sender<ADBTransportMessage>,
    close_remote: u32,
) {
    queue_open(local, replies);
    for message in [
        packet(MessageCommand::Write, 17, stale_local(local), b"old stream"),
        packet(MessageCommand::Write, 99, local, b"wrong remote"),
        packet(MessageCommand::Clse, 99, local, &[]),
        packet(MessageCommand::Clse, 17, stale_local(local), &[]),
        packet(MessageCommand::Write, 7, local, b"current"),
        packet(MessageCommand::Okay, 7, local, &[]),
        packet(MessageCommand::Clse, close_remote, local, &[]),
    ] {
        replies.send(message).unwrap();
    }
}

#[test]
fn sequential_shells_ignore_stale_output_and_close_and_ack_only_current_data() {
    let mut device = scripted_device(queue_shell);
    for _ in 0..2 {
        let mut output = Vec::new();
        device
            .shell_command(&"id", Some(&mut output), None)
            .unwrap();
        assert_eq!(output, b"current");
    }
    let sent = device.transport.sent.lock().unwrap();
    let opens: Vec<_> = sent
        .iter()
        .filter(|m| m.header().command() == MessageCommand::Open)
        .collect();
    let acks: Vec<_> = sent
        .iter()
        .filter(|m| m.header().command() == MessageCommand::Okay)
        .collect();
    assert_eq!(opens.len(), 2);
    assert_eq!(acks.len(), 2);
    for (open, ack) in opens.into_iter().zip(acks) {
        assert_eq!(ack.header().arg0(), open.header().arg0());
        assert_eq!(ack.header().arg1(), 7);
    }
}

#[test]
fn interactive_shell_filters_both_ids_and_accepts_legacy_close() {
    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn legacy(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
        queue_shell_with_close(local, replies, 0);
    }
    for script in [queue_shell as OpenScript, legacy] {
        let mut device = scripted_device(script);
        let output = Output::default();
        device
            .shell(Box::new(std::io::empty()), Box::new(output.clone()))
            .unwrap();
        assert_eq!(*output.0.lock().unwrap(), b"current");
        let sent = device.transport.sent.lock().unwrap();
        let acks: Vec<_> = sent
            .iter()
            .filter(|m| m.header().command() == MessageCommand::Okay)
            .collect();
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].header().arg0(), sent[0].header().arg0());
        assert_eq!(acks[0].header().arg1(), 7);
    }
}

#[test]
fn fragmented_list_ignores_stale_initial_and_continuation_packets() {
    fn listing(local: u32, replies: &mpsc::Sender<ADBTransportMessage>) {
        queue_open(local, replies);
        let metadata: Vec<u8> = [0o100_644_u32, 123, 456, 4]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let mut tail = b"fileDONE".to_vec();
        tail.extend_from_slice(&[0; 16]);
        for message in [
            // Acknowledge LIST, then interleave old stream data at each boundary.
            packet(MessageCommand::Okay, 7, local, &[]),
            packet(MessageCommand::Write, 17, stale_local(local), b"DONE"),
            packet(MessageCommand::Write, 7, local, b"DENT"),
            packet(MessageCommand::Write, 99, local, &[0; 16]),
            packet(MessageCommand::Write, 7, local, &metadata),
            packet(MessageCommand::Clse, 17, stale_local(local), &[]),
            packet(MessageCommand::Write, 7, local, &tail),
            packet(MessageCommand::Okay, 17, stale_local(local), &[]),
            // Acknowledge QUIT and close the active synchronization stream.
            packet(MessageCommand::Okay, 7, local, &[]),
            packet(MessageCommand::Clse, 99, local, &[]),
            packet(MessageCommand::Clse, 7, local, &[]),
        ] {
            replies.send(message).unwrap();
        }
    }
    let mut device = scripted_device(listing);
    assert_eq!(
        device.list("/data/local/tmp").unwrap(),
        vec![crate::ADBListItemType::File(crate::ADBListItem {
            name: "file".into(),
            time: 456,
            permissions: 0o644,
            size: 123,
        })]
    );
    let sent = device.transport.sent.lock().unwrap();
    let local = sent[0].header().arg0();
    for message in sent.iter().skip(1) {
        assert_eq!(message.header().arg0(), local);
        assert_eq!(message.header().arg1(), 7);
    }
}

#[test]
fn handshake_rejects_payload_limits_that_cannot_hold_sync_data() {
    for limit in 0..=8 {
        let cnxn = packet(MessageCommand::Cnxn, 0x0100_0000, limit, b"device::\0");
        assert!(ADBMessageDevice::<FakeTransport>::handle_connection(&cnxn).is_err());
    }
    for (limit, expected) in [(9, 9), (4096, 4096), (u32::MAX, MAX_PAYLOAD_SIZE)] {
        let cnxn = packet(MessageCommand::Cnxn, 0x0100_0000, limit, b"device::\0");
        assert_eq!(
            ADBMessageDevice::<FakeTransport>::handle_connection(&cnxn).unwrap(),
            expected as usize
        );
    }
}

#[test]
fn shell_input_error_closes_stream_and_returns_original_error() {
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(Error::other("input reader failed"))
        }
    }

    let mut device = device(None);
    let sent = device.transport.sent.clone();
    let error = device
        .shell(Box::new(FailingReader), Box::new(std::io::sink()))
        .unwrap_err();
    assert!(error.to_string().contains("input reader failed"), "{error}");
    assert_eq!(
        sent.lock()
            .unwrap()
            .iter()
            .filter(|m| m.header().command() == MessageCommand::Clse)
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn stat_paths_reach_the_shell_as_one_literal_operand() {
    for path in [
        "/sdcard/My File",
        "-leading-option",
        "/sdcard/quote'file",
        "/sdcard/file; printf injected",
        "/sdcard/$(printf injected)\nnext",
    ] {
        let mut device = device(Some(b"stat: missing: No such file or directory\n"));
        assert!(device.stat_extended(&path).unwrap().is_none());
        let sent = device.transport.sent.lock().unwrap();
        let open = sent
            .iter()
            .find(|m| m.header().command() == MessageCommand::Open)
            .unwrap();
        let command = std::str::from_utf8(open.payload())
            .unwrap()
            .trim_end_matches('\0')
            .strip_prefix("shell:")
            .unwrap();
        let arguments = command.strip_prefix("stat ").unwrap();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {arguments}; printf '%s\\0' \"$@\""))
            .output()
            .unwrap();
        assert!(output.status.success());
        let mut expected = b"--\0".to_vec();
        expected.extend_from_slice(path.as_bytes());
        expected.push(0);
        assert_eq!(output.stdout, expected, "{path}");
    }
}

fn auth_transport() -> (FakeTransport, mpsc::Sender<ADBTransportMessage>) {
    let (replies, incoming) = mpsc::channel();
    let transport = FakeTransport {
        incoming: Arc::new(Mutex::new(incoming)),
        replies: replies.clone(),
        sent: Arc::default(),
        shell_output: None,
        on_open: None,
    };
    (transport, replies)
}

fn key_file() -> (std::path::PathBuf, ADBRsaKey) {
    let key = ADBRsaKey::new_random().unwrap();
    let path = std::env::temp_dir().join(format!(
        "adb_client-auth-{}-{}",
        std::process::id(),
        rand::rng().random::<u64>()
    ));
    std::fs::write(&path, key.to_pkcs8_pem().unwrap()).unwrap();
    (path, key)
}

#[test]
fn existing_key_only_never_offers_the_public_key() {
    let (path, _) = key_file();
    let (transport, replies) = auth_transport();
    let sent = transport.sent.clone();
    // The device asks to authenticate, then rejects the signature with a new token.
    replies
        .send(packet(MessageCommand::Auth, AUTH_TOKEN, 0, &[7; 20]))
        .unwrap();
    replies
        .send(packet(MessageCommand::Auth, AUTH_TOKEN, 0, &[8; 20]))
        .unwrap();
    let result =
        ADBMessageDevice::new_with_auth_policy(transport, &path, AuthPolicy::ExistingKeyOnly);
    assert!(result.is_err());
    let sent = sent.lock().unwrap();
    assert!(sent.iter().any(
        |m| m.header().command() == MessageCommand::Auth && m.header().arg0() == AUTH_SIGNATURE
    ));
    assert!(
        !sent
            .iter()
            .any(|m| m.header().command() == MessageCommand::Auth
                && m.header().arg0() == AUTH_RSAPUBLICKEY)
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn existing_key_only_requires_a_key_file() {
    let (transport, _replies) = auth_transport();
    let missing = std::env::temp_dir().join("adb_client-auth-missing-key");
    assert!(
        ADBMessageDevice::new_with_auth_policy(transport, missing, AuthPolicy::ExistingKeyOnly)
            .is_err()
    );
}

#[test]
fn offer_public_key_still_requests_authorization() {
    let (path, _) = key_file();
    let (transport, replies) = auth_transport();
    let sent = transport.sent.clone();
    replies
        .send(packet(MessageCommand::Auth, AUTH_TOKEN, 0, &[7; 20]))
        .unwrap();
    replies
        .send(packet(MessageCommand::Auth, AUTH_TOKEN, 0, &[8; 20]))
        .unwrap();
    replies
        .send(packet(
            MessageCommand::Cnxn,
            0x0100_0000,
            4096,
            b"device::\0",
        ))
        .unwrap();
    ADBMessageDevice::new_with_auth_policy(transport, &path, AuthPolicy::OfferPublicKey).unwrap();
    assert!(
        sent.lock()
            .unwrap()
            .iter()
            .any(|m| m.header().command() == MessageCommand::Auth
                && m.header().arg0() == AUTH_RSAPUBLICKEY)
    );
    let _ = std::fs::remove_file(path);
}
