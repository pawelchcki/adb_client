use std::{fs::File, io::Write};

use adb_client::server_device::ADBServerDevice;

use crate::models::{ADBCliResult, ForwardCommand, LocalDeviceCommand, ReverseCommand};

pub fn handle_local_commands(
    mut device: ADBServerDevice,
    local_device_commands: LocalDeviceCommand,
) -> ADBCliResult<()> {
    match local_device_commands {
        LocalDeviceCommand::HostFeatures => {
            let features = device
                .host_features()?
                .iter()
                .map(ToString::to_string)
                .reduce(|a, b| format!("{a},{b}"))
                .unwrap_or_default();
            log::info!("Available host features: {features}");

            Ok(())
        }
        LocalDeviceCommand::Logcat { path } => {
            let writer: Box<dyn Write> = if let Some(path) = path {
                let log_file = File::create(path)?;
                Box::new(log_file)
            } else {
                Box::new(std::io::stdout())
            };
            Ok(device.get_logs(writer)?)
        }
        LocalDeviceCommand::Forward(forward_command) => match forward_command {
            ForwardCommand::RemoveAll => Ok(device.forward_remove_all()?),
            ForwardCommand::Remove { local } => Ok(device.forward_remove(local)?),
            ForwardCommand::Add { local, remote } => Ok(device.forward(remote, local)?),
        },
        LocalDeviceCommand::Reverse(reverse_command) => match reverse_command {
            ReverseCommand::RemoveAll => Ok(device.reverse_remove_all()?),
            ReverseCommand::Remove { remote } => Ok(device.reverse_remove(remote)?),
            ReverseCommand::Add { remote, local } => Ok(device.reverse(remote, local)?),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::{
        io::Read,
        net::{SocketAddr, TcpListener},
        time::Duration,
    };

    fn assert_request(command: LocalDeviceCommand, expected: &'static str) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            for expected in ["host:transport:fixture", expected] {
                let mut length = [0; 4];
                stream.read_exact(&mut length).unwrap();
                let size =
                    usize::from_str_radix(std::str::from_utf8(&length).unwrap(), 16).unwrap();
                let mut body = vec![0; size];
                stream.read_exact(&mut body).unwrap();
                assert_eq!(body, expected.as_bytes());
                stream.write_all(b"OKAY").unwrap();
            }
        });
        handle_local_commands(
            ADBServerDevice::new("fixture".into(), Some(address)),
            command,
        )
        .unwrap_or_else(|err| panic!("{err}"));
        server.join().unwrap();
    }

    #[test]
    fn forward_cli_keeps_host_and_device_endpoints_in_wire_order() {
        let command =
            LocalDeviceCommand::try_parse_from(["local", "forward", "add", "tcp:1000", "tcp:2000"])
                .unwrap();
        assert_request(command, "host:forward:tcp:1000;tcp:2000");
    }

    #[test]
    fn reverse_remove_cli_uses_and_documents_device_endpoint() {
        let command =
            LocalDeviceCommand::try_parse_from(["local", "reverse", "remove", "tcp:2000"]).unwrap();
        assert_request(command, "reverse:killforward:tcp:2000");
        let mut cli = LocalDeviceCommand::command();
        let usage = cli
            .find_subcommand_mut("reverse")
            .unwrap()
            .find_subcommand_mut("remove")
            .unwrap()
            .render_usage()
            .to_string();
        assert!(usage.contains("<REMOTE>"), "{usage}");
    }
}
