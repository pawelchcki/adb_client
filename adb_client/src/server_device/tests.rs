use super::ADBServerDevice;
use crate::{ADBDeviceExt, ADBTimeouts, HostFeatures};
use std::{
    io::{Read, Write},
    net::{SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    sync::mpsc,
    time::Duration,
};

fn fixture() -> (TcpListener, SocketAddrV4) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
        unreachable!()
    };
    (listener, address)
}

fn request(stream: &mut TcpStream, expected: &str) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut length = [0; 4];
    stream.read_exact(&mut length).unwrap();
    let size = usize::from_str_radix(std::str::from_utf8(&length).unwrap(), 16).unwrap();
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, expected.as_bytes());
    stream.write_all(b"OKAY").unwrap();
}

#[test]
fn log_reader_owns_its_socket_and_preserves_unterminated_lines() {
    let (listener, address) = fixture();
    let (release, wait) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream, "host:transport:selected-device");
        request(&mut stream, "exec:logcat");
        wait.recv_timeout(Duration::from_secs(3)).unwrap();
        stream.write_all(b"last log line without newline").unwrap();
    });
    let mut device = ADBServerDevice::new("selected-device".into(), Some(address));
    let mut logs = device.log_reader().unwrap();
    drop(device);
    release.send(()).unwrap();
    let mut bytes = Vec::new();
    logs.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"last log line without newline");
    peer.join().unwrap();
}

#[test]
fn interactive_shell_uses_legacy_service_and_drains_output_after_stdin_eof() {
    let (listener, address) = fixture();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream, "host:transport:selected-device");
        request(&mut stream, "shell:");
        let mut input = Vec::new();
        stream.read_to_end(&mut input).unwrap();
        assert_eq!(input, b"exit\n");
        stream.write_all(b"bye\n").unwrap();
    });
    let mut device = ADBServerDevice::new("selected-device".into(), Some(address));
    device
        .shell(
            Box::new(std::io::Cursor::new(b"exit\n")),
            Box::new(std::io::sink()),
        )
        .unwrap();
    peer.join().unwrap();
}

#[test]
fn selected_device_features_do_not_query_ambiguous_host_features() {
    for transport_id in [None, Some(42)] {
        let (listener, address) = fixture();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            request(
                &mut stream,
                if transport_id.is_some() {
                    "host-transport-id:42:features"
                } else {
                    "host-serial:selected-device:features"
                },
            );
            stream.write_all(b"0008shell_v2").unwrap();
        });
        let mut device = ADBServerDevice::new("selected-device".into(), Some(address));
        device.transport_id = transport_id;
        assert!(
            device
                .host_features()
                .unwrap()
                .contains(&HostFeatures::ShellV2)
        );
        // Cached result requires no second connection.
        assert!(
            device
                .host_features()
                .unwrap()
                .contains(&HostFeatures::ShellV2)
        );
        peer.join().unwrap();
    }
}

#[test]
fn server_command_read_timeout_is_configurable() {
    let (listener, address) = fixture();
    let (release, wait) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(3));
    });
    let mut device = ADBServerDevice::new("selected-device".into(), Some(address))
        .with_timeouts(ADBTimeouts {
            read: Some(Duration::from_millis(30)),
            ..ADBTimeouts::default()
        })
        .unwrap();
    let error = device.log_reader().unwrap_err();
    assert!(
        matches!(error, crate::RustADBError::IOError(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock))
    );
    release.send(()).unwrap();
    peer.join().unwrap();
}
