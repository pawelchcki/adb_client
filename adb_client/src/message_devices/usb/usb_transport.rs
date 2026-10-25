use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use rusb::{Context, Device, DeviceHandle, Direction, TransferType, UsbContext};

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport, adb_transport_message::ADBTransportMessage,
        packet_reader::PacketReader,
    },
};

#[derive(Clone, Debug)]
struct Endpoint {
    iface: u8,
    address: u8,
    max_packet_size: usize,
    setting: u8,
}

/// Transport running on USB
#[derive(Debug, Clone)]
pub struct USBTransport {
    device: Device<Context>,
    timeouts: crate::ADBTimeouts,
    receive: Option<Arc<Mutex<PacketReader>>>,
    send: Option<Arc<Mutex<()>>>,
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
            timeouts: crate::ADBTimeouts::DEFAULT,
            receive: None,
            send: None,
            handle: None,
            read_endpoint: None,
            write_endpoint: None,
        }
    }

    /// Set handshake and command I/O timeouts before connecting, or for future operations.
    pub fn set_timeouts(&mut self, timeouts: crate::ADBTimeouts) -> Result<()> {
        self.timeouts = timeouts.validate()?;
        Ok(())
    }

    pub(crate) const fn timeouts(&self) -> crate::ADBTimeouts {
        self.timeouts
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
        for n in 0..handle.device().device_descriptor()?.num_configurations() {
            let Ok(config_desc) = handle.device().config_descriptor(n) else {
                continue;
            };

            for interface in config_desc.interfaces() {
                for interface_desc in interface.descriptors() {
                    let mut read_endpoint: Option<Endpoint> = None;
                    let mut write_endpoint: Option<Endpoint> = None;
                    for endpoint_desc in interface_desc.endpoint_descriptors() {
                        if endpoint_desc.transfer_type() == TransferType::Bulk
                            && super::utils::is_adb_interface(
                                interface_desc.class_code(),
                                interface_desc.sub_class_code(),
                                interface_desc.protocol_code(),
                            )
                        {
                            let endpoint = Endpoint {
                                iface: interface_desc.interface_number(),
                                setting: interface_desc.setting_number(),
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
        let max_packet_size = endpoint.max_packet_size;

        let mut offset = 0;
        let data_len = data.len();
        while offset < data_len {
            let write_amount = handle.write_bulk(endpoint.address, &data[offset..], timeout)?;
            if write_amount == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
            }
            offset += write_amount;

            log::trace!("wrote chunk of size {write_amount} - {offset}/{data_len}");
        }

        if offset % max_packet_size == 0 {
            log::trace!("must send final zero-length packet");
            handle.write_bulk(endpoint.address, &[], timeout)?;
        }

        Ok(())
    }
}

impl ADBTransport for USBTransport {
    fn connect(&mut self) -> crate::Result<()> {
        let device = self.device.open()?;

        let (read_endpoint, write_endpoint) = Self::find_endpoints(&device)?;

        Self::configure_endpoint(&device, &read_endpoint)?;
        log::debug!("got read endpoint: {read_endpoint:?}");
        self.read_endpoint = Some(read_endpoint.clone());

        if write_endpoint.iface != read_endpoint.iface {
            Self::configure_endpoint(&device, &write_endpoint)?;
        }
        if read_endpoint.setting != 0 {
            device.set_alternate_setting(read_endpoint.iface, read_endpoint.setting)?;
        }
        log::debug!("got write endpoint: {write_endpoint:?}");
        self.write_endpoint = Some(write_endpoint);

        self.receive = Some(Arc::default());
        self.send = Some(Arc::default());
        self.handle = Some(Arc::new(device));

        Ok(())
    }

    fn disconnect(&mut self) -> crate::Result<()> {
        if self.handle.is_none() {
            // device has not been initialized, nothing to do
            return Ok(());
        }

        // Release without taking the packet-send mutex: disconnect must also
        // interrupt a transfer configured with an unlimited write timeout.
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
    fn read_message(&mut self) -> Result<ADBTransportMessage> {
        self.read_message_with_timeout(
            self.timeouts
                .read
                .unwrap_or(crate::message_devices::adb_message_transport::DEFAULT_READ_TIMEOUT),
        )
    }
    fn write_message(&mut self, message: ADBTransportMessage) -> Result<()> {
        self.write_message_with_timeout(
            message,
            self.timeouts
                .write
                .unwrap_or(crate::message_devices::adb_message_transport::DEFAULT_READ_TIMEOUT),
        )
    }

    fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        let timeout =
            if timeout == crate::message_devices::adb_message_transport::DEFAULT_READ_TIMEOUT {
                Duration::ZERO
            } else {
                timeout
            };
        let _send = self
            .send
            .as_ref()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotConnected))?
            .lock()?;
        let message_bytes = message.header().as_bytes();
        self.write_bulk_data(&message_bytes, timeout)?;

        log::trace!("successfully write header: {} bytes", message_bytes.len());

        let payload = message.into_payload();
        if !payload.is_empty() {
            self.write_bulk_data(&payload, timeout)?;
            log::trace!("successfully write payload: {} bytes", payload.len());
        }

        Ok(())
    }

    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
        let endpoint = self.get_read_endpoint()?;
        let handle = self.get_raw_connection()?;
        let timeout =
            if timeout == crate::message_devices::adb_message_transport::DEFAULT_READ_TIMEOUT {
                Duration::ZERO
            } else {
                timeout
            };
        self.receive
            .as_ref()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotConnected))?
            .lock()?
            .read(|bytes| {
                loop {
                    let size = handle.read_bulk(endpoint.address, bytes, timeout)?;
                    if size != 0 {
                        return Ok(size);
                    }
                }
            })
    }
}
