# `pyadb_client`

<p align="center">
    <p align="center">Python library to communicate with ADB devices. Built on top of Rust adb_client library.</p>
    <p align="center">
        <a href="https://pypi.org/project/pyadb_client">
            <img alt="pypi.org" src="https://img.shields.io/pypi/v/pyadb_client.svg" />
        </a>
        <a href="https://pypi.org/project/pyadb_client">
            <img alt="downloads" src="https://static.pepy.tech/badge/pyadb_client" />
        </a>
    </p>
</p>

## Installation

```bash
pip install pyadb_client
```

## Examples

### Use ADB server

```python
from pyadb_client import PyADBServer

server = PyADBServer("127.0.0.1:5037")
for i, device in enumerate(server.devices()):
    print(i, device.identifier, device.state)

# Get only connected device
device = server.get_device()
print(device, device.identifier)
```

### Connect and Disconnect with device id

```python
from pyadb_client import PyADBServer

server = PyADBServer("127.0.0.1:5037")

# Connect to a device with device id
device = server.connect_device("192.168.1.100:5555")
print(f"Connected to {device.identifier}")

# Disconnect from a device with device id
server.disconnect_device("192.168.1.100:5555")
```

### Push a file on device

```python
from pyadb_client import PyADBUSBDevice

usb_device = PyADBUSBDevice.autodetect()
usb_device.push("file.txt", "/data/local/tmp/file.txt")
```

## Local development

```bash
# Create Python virtual environment
python3 -m venv .venv
source .venv/bin/activate

# Install needed build dependencies
pip install maturin

# Build development package
maturin develop

# Build stub file (.pyi)
cargo run --bin stub_gen

# Build release Python package
maturin build --release -m pyadb_client/Cargo.toml
```

## Direct TCP devices and binary helpers

A running ADB server can connect a remote device using `server.connect_device`,
as shown above. To talk directly to an already-enabled TCP adbd endpoint:

```python
from pyadb_client import PyADBTcpDevice

device = PyADBTcpDevice.new("192.168.1.100:5555")
device.set_timeouts(read_ms=30_000)
output = device.shell_command("id")
packages = device.list_packages("third-party")  # current Android user
system = device.list_packages("system", user=0)
device.tap(100, 200)
with open("screen.png", "wb") as image:
    image.write(device.screenshot_png())
raw = device.exec_out("cat /data/local/tmp/binary")
```

The helpers also exist on server-backed and USB devices. Timeout values are
milliseconds; `None` means unlimited idle I/O. For multiple connected devices,
select a specific server device or transport ID. Device feature queries retain
that selection, including with older ADB servers.

Android 11+ wireless debugging uses a separate pairing port and connect port;
the TCP constructor expects the connect port and an already trusted host key.
The Rust crate's optional `pairing` API performs direct pairing without a server.
