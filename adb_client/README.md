# `adb_client`

[![MIT licensed](https://img.shields.io/crates/l/adb_client.svg)](./LICENSE-MIT)
[![Documentation](https://docs.rs/adb_client/badge.svg)](https://docs.rs/adb_client)
[![Crates.io Total Downloads](https://img.shields.io/crates/d/adb_client)](https://crates.io/crates/adb_client)
![MSRV](https://img.shields.io/crates/msrv/adb_client)

Rust library implementing ADB protocol.

## Installation

Add `adb_client` crate as a dependency by simply adding it to your `Cargo.toml`:

```toml
[dependencies]
adb_client = "*"
```

## Crate features

|    Feature    |                   Description                   | Default? |
| :-----------: | :---------------------------------------------: | :------: |
| `framebuffer` |      Enables _framebuffer_-related methods      |   Yes    |
|    `mdns`     | Enables mDNS device discovery on local network. |    No    |
|     `usb`     |     Enables native interactions with USB devices. |    No    |
|   `async`    | Tokio-native ADB-server APIs and streaming readers. | No |
|   `pairing`  | Direct Android wireless pairing (includes `async`). | No |
|   `webusb`    | Async browser USB API on `wasm32-unknown-unknown`. |    No    |

To deactivate some default features you can use the `default-features = false` option in your `Cargo.toml` file and manually specify the features you want to activate:

```toml
[dependencies]
adb_client = { version = "*", default-features = false, features = ["mdns", "usb"] }
```

## Native timeouts

`ADBTimeouts` configures handshake, read and write limits. `None` disables a
limit. Command limits apply to idle transport I/O; they do not stop a command
that continues producing data. TCP packet readers retain incomplete headers and
payloads across timeouts so a later read can resume safely.

```rust,no_run
# #[cfg(not(target_arch = "wasm32"))]
# fn example() -> adb_client::Result<()> {
use adb_client::{ADBTimeouts, tcp::ADBTcpDevice};
use std::{net::SocketAddr, time::Duration};
let limits = ADBTimeouts {
    handshake: Some(Duration::from_secs(60)),
    read: Some(Duration::from_secs(10)),
    ..ADBTimeouts::default()
};
let address: SocketAddr = "192.168.1.100:5555".parse()?;
let mut device = ADBTcpDevice::new_with_timeouts(address, limits)?;
device.set_timeouts(ADBTimeouts { read: None, ..limits })?;
# Ok(())
# }
```

USB has matching `new_with_timeouts` and
`new_with_custom_private_key_and_timeouts` constructors. For server-backed
connections, use `ADBServerDevice::with_timeouts` or `set_timeouts`.

## Raw output, screenshots, taps and installed packages

`ADBDeviceExt::exec_out` reads raw `exec:` output without allocating a terminal or
closing the stream when an empty stdin reader reaches EOF. `screenshot_png`
uses Android's `screencap -p` directly, avoiding framebuffer PNG re-encoding.

```rust,no_run
# fn example() -> adb_client::Result<()> {
use adb_client::{ADBDeviceExt, PackageFilter, server_device::ADBServerDevice};
let mut device = ADBServerDevice::new("DEVICE_SERIAL".into(), None);
device.screenshot_png(&mut std::fs::File::create("screen.png")?)?;
device.exec_out("cat /data/local/tmp/binary", &mut std::io::stdout())?;
device.tap(100, 200)?;
let packages = device.list_packages(PackageFilter::ThirdParty, None)?;
# let _ = packages;
# Ok(())
# }
```

Package filters are `All`, `System` and `ThirdParty`. The default user is
`current`, so inaccessible secondary profiles do not break ordinary queries;
pass `Some(user_id)` to select a specific Android user. Android error output is
reported as an error instead of being returned as package names.

The CLI exposes `exec-out`, `screenshot`, `tap` and `packages` for local, USB and
TCP devices. For example, `adb_cli local exec-out screencap -p > screen.png`
or `adb_cli local screenshot screen.png`. `packages` accepts `--system`,
`--third-party` and `--user ID`.

## Streaming and async logcat

`ADBServerDevice::log_reader()` returns an owned `std::net::TcpStream`.
It remains usable after the device object is dropped or used for another
command. Use a buffered reader for lines, or drop the stream to cancel.
`get_logs(writer)` copies all bytes, including an unterminated final line,
and propagates connection and output errors.

Enable `async` for `asynchronous::{AsyncADBServer, AsyncADBServerDevice}`. These
use Tokio sockets directly, with independent connections for concurrent
services. Their service streams implement `AsyncRead + AsyncWrite`; callers can
split them or set deadlines with `tokio::time::timeout`.

```rust,no_run
# #[cfg(all(feature = "async", not(target_arch = "wasm32")))]
# async fn example() -> adb_client::Result<()> {
use adb_client::asynchronous::AsyncADBServer;
let server = AsyncADBServer::default();
let devices = server.devices().await?;
let device = server.device("DEVICE_SERIAL");
let logs = device.log_reader().await?;
let png = device.exec_out("screencap -p").await?;
drop(logs); // Cancels logcat without occupying a blocking worker.
# let _ = (devices, png);
# Ok(())
# }
```

The synchronous native USB/TCP API remains useful on blocking worker threads.
The browser `WebUSB` API is asynchronous as described below.

## Daemon-free native forwarding

Convert an authenticated `ADBUSBDevice` or `ADBTcpDevice` with
`into_forwarder()`. This transfers ownership to one packet dispatcher, preventing
ordinary command readers from consuming forwarding packets. Run shell/setup
commands before converting. The controller supports concurrent connections with
independent acknowledgment credits, bounded queues and a 64-stream limit.

```rust,no_run
# #[cfg(all(feature = "usb", not(target_arch = "wasm32")))]
# fn example() -> adb_client::Result<()> {
use adb_client::usb::ADBUSBDevice;
let mut forwarding = ADBUSBDevice::autodetect()?.into_forwarder();
let local = forwarding.forward("127.0.0.1:0".parse()?, "tcp:8001")?;
// A host service is already listening on port 8080.
let remote_port = forwarding.reverse(0, "127.0.0.1:8080".parse()?)?;
// Keep the controller alive while clients use the rules.
forwarding.forward_remove(local)?;
forwarding.reverse_remove(remote_port)?;
forwarding.close()?;
# Ok(())
# }
```

`forward` accepts a host TCP address and an ADB remote service string, such as
`tcp:8001` or `localabstract:my_service`. Reverse rules listen on device TCP ports
and connect only to explicitly registered host TCP targets. Port zero selects an
available listening port. Individual removal and remove-all operations are
available; disconnecting removes transport-owned reverse rules in adbd. Dropping the
controller stops host listeners and closes active host sockets.

The native devices also offer consuming `forward` and `reverse` convenience
methods that return the owning controller. Server-backed devices retain their
existing `forward_remove(local)` and `reverse_remove(remote)` methods.

## Direct wireless pairing

Enable `pairing` to use `pairing::pair` or `pairing::pair_with_key_directory`
without an ADB executable or server. The implementation uses the published
`libadb` pairing library as a normal optional Cargo dependency. Key generation
and loading run on a blocking worker, then a timeout bounds the TCP/TLS/SPAKE2
exchange. Existing host identities are reused; new keys are persisted before
pairing. Native TCP connections accept PKCS#8 and legacy PKCS#1 key files.

```rust,no_run
# #[cfg(all(feature = "pairing", not(target_arch = "wasm32")))]
# async fn example() -> adb_client::Result<()> {
use adb_client::pairing;
use std::time::Duration;
let paired = pairing::pair(
    "192.168.1.100:37421".parse()?, "123456", Duration::from_secs(30),
).await?;
println!("Paired {}", paired.guid.escape_debug());
// Reuse paired.private_key_path with ADBTcpDevice::new_with_custom_private_key.
# Ok(())
# }
```

Use the address/code shown under Android's **Pair device with pairing code**.
Subsequent ADB sessions use the separate **connect** port from Wireless debugging
or mDNS discovery. Pairing returns the device GUID and key path; Android's pairing
protocol does not return the connect port. `ADBServer::pair` remains available
for applications that already use a running ADB server.

## Browser `WebUSB`

Enable `webusb` with `default-features = false` and build for
`wasm32-unknown-unknown`. Browser callers use the async
`webusb::{ADBWebUSBDevice, ADBWebUSBStream, WebUSBKey, request_device}` API.
The native blocking clients remain available on native targets.

The current `web-sys` USB bindings require `--cfg=web_sys_unstable_apis` in
Rust flags. This repository configures it in `.cargo/config.toml`; downstream
applications must configure it too.

See the [runnable browser example](../examples/webusb/README.md) for building,
requesting device permission from a click handler, Android RSA authorization,
key management, streaming shell output, and connection lifecycle limitations.

## Examples

Usage examples can be found in the `examples/` directory of this repository.

Some example are also provided in the various `README.md` files of modules.

## Benchmarks

Benchmarks run on `v2.0.6`, on a **Samsung S10 SM-G973F** device and an **Intel i7-1265U** CPU laptop

### `ADBServerDevice` push vs `adb push`

`ADBServerDevice` performs all operations by using adb server as a bridge.

| File size | Sample size | `ADBServerDevice` |   `adb`   |               Difference               |
| :-------: | :---------: | :---------------: | :-------: | :------------------------------------: |
|   10 MB   |     100     |     350,79 ms     | 356,30 ms | <div style="color:green">-1,57 %</div> |
|  500 MB   |     50      |      15,60 s      |  15,64 s  | <div style="color:green">-0,25 %</div> |
|   1 GB    |     20      |      31,09 s      |  31,12 s  | <div style="color:green">-0,10 %</div> |
