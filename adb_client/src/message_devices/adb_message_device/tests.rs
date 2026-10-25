use super::*;
use crate::{ADBDeviceExt, ADBTransport};
use std::{
    io::{Error, ErrorKind, Read},
    sync::{Arc, Mutex, mpsc},
};

#[derive(Clone, Debug)]
struct FakeTransport {
    incoming: Arc<Mutex<mpsc::Receiver<ADBTransportMessage>>>,
    replies: mpsc::Sender<ADBTransportMessage>,
    sent: Arc<Mutex<Vec<ADBTransportMessage>>>,
    shell_output: Option<Vec<u8>>,
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
    fn read_message_with_timeout(&mut self, _: Duration) -> Result<ADBTransportMessage> {
        self.incoming
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
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
        },
        max_payload_size: 4096,
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
