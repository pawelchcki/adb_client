use std::{sync::Arc, time::Duration};

use rusb::{
    Context, Device, DeviceHandle, Direction, TransferType, UsbContext,
    constants::LIBUSB_CLASS_VENDOR_SPEC,
};

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::{ADBTransportMessage, ADBTransportMessageHeader},
        message_commands::MessageCommand,
    },
};

#[derive(Clone, Debug)]
struct Endpoint {
    iface: u8,
    address: u8,
    max_packet_size: usize,
}

/// Transport running on USB
#[derive(Debug, Clone)]
pub struct USBTransport {
    device: Device<Context>,
    handle: Option<Arc<DeviceHandle<Context>>>,
    read_endpoint: Option<Endpoint>,
    write_endpoint: Option<Endpoint>,
}

impl USBTransport {
    /// Instantiate a new [`USBTransport`].
    /// Only the first device with given `vendor_id` and `product_id` is returned.
    pub fn new(vendor_id: u16, product_id: u16) -> Result<Self> {
        let context = Context::new()?;
        for device in context.devices()?.iter() {
            if let Ok(descriptor) = device.device_descriptor()
                && descriptor.vendor_id() == vendor_id
                && descriptor.product_id() == product_id
            {
                return Ok(Self::new_from_device(device));
            }
        }

        Err(RustADBError::DeviceNotFound(format!(
            "cannot find USB device with vendor_id={vendor_id} and product_id={product_id}",
        )))
    }

    /// Instantiate a new [`USBTransport`] from a [`rusb::Device`].
    ///
    /// Devices can be enumerated using [`rusb::Context::devices()`] and then filtered out to get desired device.
    #[must_use]
    pub const fn new_from_device(rusb_device: rusb::Device<Context>) -> Self {
        Self {
            device: rusb_device,
            handle: None,
            read_endpoint: None,
            write_endpoint: None,
        }
    }

    pub(crate) fn vendor_id(&self) -> Result<u16> {
        Ok(self.device.device_descriptor().map(|d| d.vendor_id())?)
    }

    pub(crate) fn product_id(&self) -> Result<u16> {
        Ok(self.device.device_descriptor().map(|d| d.product_id())?)
    }

    pub(crate) fn get_raw_connection(&self) -> Result<Arc<DeviceHandle<Context>>> {
        self.handle
            .as_ref()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            )))
            .cloned()
    }

    fn get_read_endpoint(&self) -> Result<Endpoint> {
        self.read_endpoint
            .as_ref()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no read endpoint setup",
            )))
            .cloned()
    }

    fn get_write_endpoint(&self) -> Result<&Endpoint> {
        self.write_endpoint
            .as_ref()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no write endpoint setup",
            )))
    }

    fn configure_endpoint(handle: &DeviceHandle<Context>, endpoint: &Endpoint) -> Result<()> {
        match handle.claim_interface(endpoint.iface) {
            Ok(()) => Ok(()),
            // busy state likely indicates an ADB server is running and has taken the lock over the device
            Err(rusb::Error::Busy) => Err(RustADBError::DeviceBusy),
            Err(err) => Err(err.into()),
        }
    }

    fn find_endpoints(handle: &DeviceHandle<Context>) -> Result<(Endpoint, Endpoint)> {
        let mut read_endpoint: Option<Endpoint> = None;
        let mut write_endpoint: Option<Endpoint> = None;

        for n in 0..handle.device().device_descriptor()?.num_configurations() {
            let Ok(config_desc) = handle.device().config_descriptor(n) else {
                continue;
            };

            for interface in config_desc.interfaces() {
                for interface_desc in interface.descriptors() {
                    for endpoint_desc in interface_desc.endpoint_descriptors() {
                        if endpoint_desc.transfer_type() == TransferType::Bulk
                            && interface_desc.class_code() == LIBUSB_CLASS_VENDOR_SPEC
                            && interface_desc.sub_class_code() == 0x42
                            && interface_desc.protocol_code() == 0x01
                        {
                            let endpoint = Endpoint {
                                iface: interface_desc.interface_number(),
                                address: endpoint_desc.address(),
                                max_packet_size: endpoint_desc.max_packet_size() as usize,
                            };
                            match endpoint_desc.direction() {
                                Direction::In => {
                                    if let Some(write_endpoint) = write_endpoint {
                                        return Ok((endpoint, write_endpoint));
                                    }
                                    read_endpoint = Some(endpoint);
                                }
                                Direction::Out => {
                                    if let Some(read_endpoint) = read_endpoint {
                                        return Ok((read_endpoint, endpoint));
                                    }
                                    write_endpoint = Some(endpoint);
                                }
                            }
                        }
                    }
                }
            }
        }

        Err(RustADBError::USBNoDescriptorFound)
    }

    fn write_bulk_data(&self, data: &[u8], timeout: Duration) -> Result<()> {
        let endpoint = self.get_write_endpoint()?;
        let handle = self.get_raw_connection()?;
        write_bulk_all(data, endpoint.max_packet_size, |bytes| {
            Ok(handle.write_bulk(endpoint.address, bytes, timeout)?)
        })
    }
}

impl ADBTransport for USBTransport {
    fn connect(&mut self) -> crate::Result<()> {
        let device = self.device.open()?;

        let (read_endpoint, write_endpoint) = Self::find_endpoints(&device)?;

        Self::configure_endpoint(&device, &read_endpoint)?;
        log::debug!("got read endpoint: {read_endpoint:?}");
        self.read_endpoint = Some(read_endpoint);

        Self::configure_endpoint(&device, &write_endpoint)?;
        log::debug!("got write endpoint: {write_endpoint:?}");
        self.write_endpoint = Some(write_endpoint);

        self.handle = Some(Arc::new(device));

        Ok(())
    }

    fn disconnect(&mut self) -> crate::Result<()> {
        if self.handle.is_none() {
            // device has not been initialized, nothing to do
            return Ok(());
        }

        let message = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[])?;
        if let Err(e) = self.write_message(message) {
            log::error!("error while sending CLSE message: {e}");
        }

        if let Some(handle) = &self.handle {
            let endpoint = self.read_endpoint.as_ref().or(self.write_endpoint.as_ref());
            if let Some(endpoint) = &endpoint {
                match handle.release_interface(endpoint.iface) {
                    Ok(()) => log::debug!("succesfully released interface"),
                    Err(e) => log::error!("error while release interface: {e}"),
                }
            }
        }

        Ok(())
    }
}

impl ADBMessageTransport for USBTransport {
    fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        self.write_message_ref_with_timeout(&message, timeout)
    }

    fn write_message_ref_with_timeout(
        &mut self,
        message: &ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        let message_bytes = message.header().to_bytes();
        self.write_bulk_data(&message_bytes, timeout)?;

        log::trace!("successfully write header: {} bytes", message_bytes.len());

        let payload = message.payload();
        if !payload.is_empty() {
            self.write_bulk_data(payload, timeout)?;
            log::trace!("successfully write payload: {} bytes", payload.len());
        }

        Ok(())
    }

    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
        let endpoint = self.get_read_endpoint()?;
        let handle = self.get_raw_connection()?;
        let mut data = [0u8; 24];
        read_bulk_exact(&mut data, |bytes| {
            Ok(handle.read_bulk(endpoint.address, bytes, timeout)?)
        })?;

        let header = ADBTransportMessageHeader::try_from(data)?;
        log::trace!("received header {header:?}");

        if header.data_length() != 0 {
            let mut msg_data = vec![0_u8; header.data_length() as usize];
            read_bulk_exact(&mut msg_data, |bytes| {
                Ok(handle.read_bulk(endpoint.address, bytes, timeout)?)
            })?;

            let message = ADBTransportMessage::from_header_and_payload(header, msg_data);

            // Check message integrity
            if !message.check_message_integrity() {
                return Err(RustADBError::InvalidIntegrity(
                    ADBTransportMessageHeader::compute_crc32(message.payload()),
                    message.header().data_crc32(),
                ));
            }

            return Ok(message);
        }

        Ok(ADBTransportMessage::from_header_and_payload(header, vec![]))
    }
}

// libusb handles endpoint packet splitting. Keep header and payload transfers
// separate and terminate packet-aligned writes with the ADB-required ZLP.
fn write_bulk_all(
    data: &[u8],
    packet_size: usize,
    mut write: impl FnMut(&[u8]) -> Result<usize>,
) -> Result<()> {
    let mut remaining = data;
    while !remaining.is_empty() {
        let count = write(remaining)?;
        if count == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
        }
        remaining = &remaining[count..];
    }
    if data.len().is_multiple_of(packet_size) {
        write(&[])?;
    }
    Ok(())
}
fn read_bulk_exact(
    mut bytes: &mut [u8],
    mut read: impl FnMut(&mut [u8]) -> Result<usize>,
) -> Result<()> {
    while !bytes.is_empty() {
        // USB ZLPs are legal between ADB messages: consume them and keep reading.
        let count = read(bytes)?;
        bytes = &mut bytes[count..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_writes_batch_packets_and_preserve_short_tails_and_zlps() {
        for packet_size in [512, 1024] {
            for length in [24, 0, packet_size, packet_size * 64, packet_size * 64 + 7] {
                let data = vec![123; length];
                let mut written = Vec::new();
                let mut lengths = Vec::new();
                write_bulk_all(&data, packet_size, |bytes| {
                    lengths.push(bytes.len());
                    written.extend_from_slice(bytes);
                    Ok(bytes.len())
                })
                .unwrap();
                assert_eq!(written, data);
                assert_eq!(
                    lengths,
                    if length == 0 {
                        vec![0]
                    } else if length.is_multiple_of(packet_size) {
                        vec![length, 0]
                    } else {
                        vec![length]
                    }
                );
                let mut short = Vec::new();
                write_bulk_all(&data, packet_size, |bytes| {
                    let count = bytes.len().min(13);
                    short.extend_from_slice(&bytes[..count]);
                    Ok(count)
                })
                .unwrap();
                assert_eq!(short, data);
            }
        }
        assert!(write_bulk_all(b"x", 512, |_| Ok(0)).is_err());
    }
    #[test]
    fn bulk_reads_accept_partial_transfers_and_interleaved_zlps() {
        let expected = vec![42; 65543];
        let mut output = vec![0; expected.len()];
        let mut offset = 0;
        let mut calls = 0;
        read_bulk_exact(&mut output, |bytes| {
            calls += 1;
            let count = if calls % 3 == 0 {
                0
            } else {
                bytes.len().min(8192)
            };
            bytes[..count].copy_from_slice(&expected[offset..offset + count]);
            offset += count;
            Ok(count)
        })
        .unwrap();
        assert_eq!(output, expected);
        assert!(calls < 16);
    }
}
