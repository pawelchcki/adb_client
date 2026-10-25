# Examples

## Launch a command on device

```rust no_run
use adb_client::{usb::ADBUSBDevice, ADBDeviceExt};

let vendor_id = 0x04e8;
let product_id = 0x6860;
let mut device = ADBUSBDevice::new(vendor_id, product_id).expect("cannot find device");
device.shell_command(&"df -h", Some(&mut std::io::stdout()), None);
```

## Push a file to the device

```rust no_run
use adb_client::{usb::ADBUSBDevice, ADBDeviceExt};
use std::fs::File;
use std::path::Path;

let vendor_id = 0x04e8;
let product_id = 0x6860;
let mut device = ADBUSBDevice::new(vendor_id, product_id).expect("cannot find device");
let mut input = File::open(Path::new("/tmp/file.txt")).expect("Cannot open file");
device.push(&mut input, &"/data/local/tmp");
```

## Windows drivers

Native USB uses libusb and needs a compatible driver, such as WinUSB or
libusbK, on the device's ADB interface. A working Samsung/OEM `adb` driver does
not establish libusb compatibility; the USB backend can report
`Operation not supported` while server-backed commands work.

Configure the ADB interface of a composite device and retain drivers for its
other interfaces. Applications using the existing OEM driver can use
`ADBServerDevice` with the running ADB server. See the
[libusb Windows driver documentation](https://github.com/libusb/libusb/wiki/Windows)
for supported drivers and setup. Stop other programs holding the ADB interface
before opening it through the native USB backend.
