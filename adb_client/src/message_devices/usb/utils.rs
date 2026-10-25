use rusb::{Context, Device, DeviceDescriptor, UsbContext, constants::LIBUSB_CLASS_VENDOR_SPEC};

use crate::{Result, RustADBError};

/// Represents an Android device connected via USB
#[derive(Clone, Debug)]
pub struct ADBDeviceInfo {
    /// Vendor ID of the device
    pub vendor_id: u16,
    /// Product ID of the device
    pub product_id: u16,
    /// Textual description of the device
    pub device_description: String,
}

/// Find and return a list of all connected Android devices with known interface class and subclass values
pub fn find_all_connected_adb_devices() -> Result<Vec<ADBDeviceInfo>> {
    let mut found_devices = vec![];

    let context = Context::new()?;
    for device in context.devices()?.iter() {
        let Ok(des) = device.device_descriptor() else {
            continue;
        };

        if is_adb_device(&device, &des) {
            let Ok(device_handle) = device.open() else {
                found_devices.push(ADBDeviceInfo {
                    vendor_id: des.vendor_id(),
                    product_id: des.product_id(),
                    device_description: "Unknown device".to_string(),
                });
                continue;
            };

            let manufacturer = device_handle
                .read_manufacturer_string_ascii(&des)
                .unwrap_or_else(|_| "Unknown".to_string());

            let product = device_handle
                .read_product_string_ascii(&des)
                .unwrap_or_else(|_| "Unknown".to_string());

            found_devices.push(ADBDeviceInfo {
                vendor_id: des.vendor_id(),
                product_id: des.product_id(),
                device_description: format!("{manufacturer} {product}"),
            });
        }
    }

    Ok(found_devices)
}

/// Find and return an USB-connected Android device with known interface class and subclass values.
///
/// Returns the first device found or None if no device is found.
/// If multiple devices are found, an error is returned.
pub fn get_single_connected_adb_device() -> Result<Option<ADBDeviceInfo>> {
    let found_devices = find_all_connected_adb_devices()?;
    match (found_devices.first(), found_devices.get(1)) {
        (None, _) => Ok(None),
        (Some(device_info), None) => {
            log::debug!(
                "Autodetect device {:04x}:{:04x} - {}",
                device_info.vendor_id,
                device_info.product_id,
                device_info.device_description
            );
            Ok(Some(device_info.clone()))
        }
        (Some(device_1), Some(device_2)) => Err(RustADBError::DeviceNotFound(format!(
            "Found two Android devices {:04x}:{:04x} and {:04x}:{:04x}",
            device_1.vendor_id, device_1.product_id, device_2.vendor_id, device_2.product_id
        ))),
    }
}

/// Check whether a device with given descriptor is an ADB device
fn is_adb_device<T: UsbContext>(device: &Device<T>, des: &DeviceDescriptor) -> bool {
    for n in 0..des.num_configurations() {
        let Ok(config_des) = device.config_descriptor(n) else {
            continue;
        };
        for interface in config_des.interfaces() {
            for interface_des in interface.descriptors() {
                let proto = interface_des.protocol_code();
                let class = interface_des.class_code();
                let subcl = interface_des.sub_class_code();
                if is_adb_interface(class, subcl, proto) {
                    return true;
                }
            }
        }
    }
    false
}

// Discovery and endpoint selection must agree, including legacy MTP/ADB interfaces.
pub(crate) const fn is_adb_interface(class: u8, subclass: u8, protocol: u8) -> bool {
    protocol == 1
        && ((class == LIBUSB_CLASS_VENDOR_SPEC && subclass == 0x42)
            || (class == 0xdc && subclass == 2))
}
