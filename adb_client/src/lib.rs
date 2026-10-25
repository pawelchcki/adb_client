#![crate_type = "lib"]
#![forbid(unsafe_code)]
#![forbid(missing_debug_implementations)]
#![forbid(missing_docs)]
#![allow(clippy::missing_errors_doc)]
#![doc = include_str!("../README.md")]
// Feature `doc_cfg` is currently only available on nightly builds.
// It is activated when cfg `docsrs` is enabled.
// Documentation can be build locally using:
// `RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --all-features`
#![cfg_attr(docsrs, feature(doc_cfg))]

mod adb_device_ext;
mod adb_transport;
/// Emulator-related definitions
pub mod emulator;
mod error;
mod message_devices;
mod models;
mod package_filter;
pub use package_filter::PackageFilter;
mod timeouts;
pub use timeouts::ADBTimeouts;

/// Server-related definitions
pub mod server;

/// Tokio-native smart-socket clients and cancellable streaming service readers.
#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub mod asynchronous;
/// Direct Android wireless pairing without an ADB server or executable.
#[cfg(all(feature = "pairing", not(target_arch = "wasm32")))]
#[cfg_attr(docsrs, doc(cfg(feature = "pairing")))]
pub mod pairing;
/// Device reachable by the server related definitions
pub mod server_device;
mod utils;

/// MDNS-related definitions
#[cfg(all(feature = "mdns", not(target_arch = "wasm32")))]
#[cfg_attr(docsrs, doc(cfg(feature = "mdns")))]
pub mod mdns;

pub use adb_device_ext::ADBDeviceExt;
use adb_transport::ADBTransport;
pub use error::{Result, RustADBError};
pub use message_devices::*;
pub use models::{
    ADBListItem, ADBListItemType, ADBStatExtendedResponse, ADBStatMapping, AdbStatResponse,
    HostFeatures, RebootType, RemountInfo,
};
