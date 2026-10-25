use super::*;
use clap::Parser;
use std::{
    io::Read,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    time::Duration,
};

struct OutputFile(PathBuf);
impl Drop for OutputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn raw_exec_and_screenshot_cli_write_exact_binary_bytes() {
    for screenshot in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        let bytes = b"\x89PNG\r\n\x1a\n\0\xff\r\n";
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            for expected in ["host:transport:selected", "exec:screencap -p"] {
                let mut length = [0; 4];
                stream.read_exact(&mut length).unwrap();
                let length =
                    usize::from_str_radix(std::str::from_utf8(&length).unwrap(), 16).unwrap();
                let mut request = vec![0; length];
                stream.read_exact(&mut request).unwrap();
                assert_eq!(request, expected.as_bytes());
                stream.write_all(b"OKAY").unwrap();
            }
            stream.write_all(bytes).unwrap();
        });
        let output = OutputFile(
            std::env::temp_dir().join(format!("adb-cli-png-{}-{screenshot}", std::process::id())),
        );
        let path = output.0.to_str().unwrap();
        let command = if screenshot {
            DeviceCommands::try_parse_from(["device", "screenshot", path]).unwrap()
        } else {
            DeviceCommands::try_parse_from([
                "device",
                "exec-out",
                "--output",
                path,
                "screencap",
                "-p",
            ])
            .unwrap()
        };
        run_command(
            Box::new(adb_client::server_device::ADBServerDevice::new(
                "selected".into(),
                Some(address),
            )),
            command,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(std::fs::read(&output.0).unwrap(), bytes);
        peer.join().unwrap();
    }
}
