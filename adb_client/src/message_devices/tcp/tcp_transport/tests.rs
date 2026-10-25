use super::*;
use crate::message_devices::{
    adb_transport_message::ADBTransportMessageHeader, message_commands::MessageCommand,
};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};

trait Duplex: Read + Write {}
impl<T: Read + Write> Duplex for T {}

struct KeyFile(PathBuf);
impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn key_file() -> KeyFile {
    static KEY: OnceLock<String> = OnceLock::new();
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let key = KEY.get_or_init(|| {
        rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            .unwrap()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string()
    });
    let path = std::env::temp_dir().join(format!(
        "adb-duplex-{}-{}.pem",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, key).unwrap();
    KeyFile(path)
}

fn read_packet(stream: &mut dyn Duplex) -> ADBTransportMessage {
    let mut header = [0; 24];
    stream.read_exact(&mut header).unwrap();
    let header = ADBTransportMessageHeader::try_from(header).unwrap();
    let mut payload = vec![0; header.data_length() as usize];
    stream.read_exact(&mut payload).unwrap();
    let packet = ADBTransportMessage::from_header_and_payload(header, payload);
    assert!(packet.check_message_integrity());
    packet
}

fn serve(stream: &mut dyn Duplex, prefix: &mpsc::Sender<()>) {
    let reply =
        ADBTransportMessage::try_new(MessageCommand::Write, 7, 1, &vec![b'r'; 128 * 1024]).unwrap();
    let header = reply.header().as_bytes();
    // Let the reader consume a partial packet, then wait for its input thread.
    stream.write_all(&header[..1]).unwrap();
    stream.flush().unwrap();
    prefix.send(()).unwrap();
    let mut tags = Vec::new();
    for _ in 0..2 {
        let packet = read_packet(stream);
        assert_eq!(packet.header().command(), MessageCommand::Write);
        assert_eq!(packet.payload().len(), 128 * 1024);
        let tag = packet.payload()[0];
        assert!(packet.payload().iter().all(|&byte| byte == tag));
        tags.push(tag);
    }
    tags.sort_unstable();
    assert_eq!(tags, [b'a', b'b']);
    stream.write_all(&header[1..]).unwrap();
    stream.write_all(reply.payload()).unwrap();
    stream
        .write_all(
            &ADBTransportMessage::try_new(MessageCommand::Clse, 7, 1, &[])
                .unwrap()
                .header()
                .as_bytes(),
        )
        .unwrap();
    stream.flush().unwrap();
}

fn fixture(
    tls: bool,
    legacy_key: bool,
) -> (TcpTransport, std::thread::JoinHandle<()>, Option<KeyFile>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = tls.then(|| {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
                )
                .unwrap(),
        )
    });
    let (prefix_tx, prefix_rx) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut stream: Box<dyn Duplex> = match config {
            Some(config) => {
                let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), stream);
                stream.conn.complete_io(&mut stream.sock).unwrap();
                stream.conn.refresh_traffic_keys().unwrap();
                Box::new(stream)
            }
            None => Box::new(stream),
        };
        serve(&mut *stream, &prefix_tx);
    });
    let key = tls.then(key_file);
    if legacy_key {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::pkcs8::DecodePrivateKey;
        let path = &key.as_ref().unwrap().0;
        let key =
            rsa::RsaPrivateKey::from_pkcs8_pem(&std::fs::read_to_string(path).unwrap()).unwrap();
        std::fs::write(path, key.to_pkcs1_pem(LineEnding::LF).unwrap().as_bytes()).unwrap();
    }
    let path = key.as_ref().map_or_else(PathBuf::new, |key| key.0.clone());
    let mut transport = TcpTransport::new(address, path);
    transport.connect().unwrap();
    if tls {
        transport.upgrade_connection().unwrap();
    }
    prefix_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    (transport, peer, key)
}

fn exercise_duplex(tls: bool, legacy_key: bool) {
    let (mut transport, peer, _key) = fixture(tls, legacy_key);
    let mut reader = transport.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let receive = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let message = reader.read_message_with_timeout(Duration::from_secs(3))?;
        let close = reader.read_message_with_timeout(Duration::from_secs(3))?;
        assert_eq!(close.header().command(), MessageCommand::Clse);
        Ok::<_, RustADBError>(message)
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let writers: Vec<_> = (*b"ab")
        .into_iter()
        .map(|tag| {
            let mut writer = transport.clone();
            std::thread::spawn(move || {
                writer.write_message_with_timeout(
                    ADBTransportMessage::try_new(
                        MessageCommand::Write,
                        1,
                        7,
                        &vec![tag; 128 * 1024],
                    )
                    .unwrap(),
                    Duration::from_secs(3),
                )
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap().unwrap();
    }
    let reply = receive.join().unwrap().unwrap();
    assert_eq!(reply.payload().len(), 128 * 1024);
    assert!(reply.payload().iter().all(|&byte| byte == b'r'));
    peer.join().unwrap();
    transport.disconnect().unwrap();
}

#[test]
fn plaintext_reader_does_not_block_writers_or_interleave_packets() {
    exercise_duplex(false, false);
}

#[test]
fn tls_reader_does_not_block_writers_or_interleave_packets() {
    exercise_duplex(true, false);
}

#[test]
fn native_handshake_timeout_does_not_wait_for_the_default_read_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(
            read_packet(&mut stream).header().command(),
            MessageCommand::Cnxn
        );
        let _ = wait.recv_timeout(Duration::from_secs(3));
    });
    let key = key_file();
    let error = crate::tcp::ADBTcpDevice::new_with_custom_private_key_and_timeouts(
        address,
        &key.0,
        crate::ADBTimeouts {
            handshake: Some(Duration::from_millis(50)),
            ..crate::ADBTimeouts::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(error, RustADBError::IOError(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock))
    );
    release.send(()).unwrap();
    peer.join().unwrap();
}

#[test]
fn tls_accepts_a_legacy_adb_pkcs1_identity_without_rewriting_it() {
    exercise_duplex(true, true);
}
