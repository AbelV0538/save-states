//! Chrome integration.
//!
//! - Which profile a window belongs to comes from the window itself (its per-profile
//!   AppUserModelID, see [`profiles`]) and needs no extension.
//! - Which tabs a window holds comes from the companion extension, relayed by the
//!   native-messaging bridge ([`hub`]) and paired to HWNDs by [`matching`].
//!
//! Chrome's own session files cannot serve as the tab source: the live profile's
//! current session file is held with an exclusive share mode while Chrome runs.

pub mod framing;
pub mod hub;
pub mod matching;
pub mod profiles;
pub mod setup;
pub mod snapshot;

pub use hub::{BridgeHub, BridgeInfo, HubEvent, activate_running_instance, quit_running_instance};
pub use profiles::{ChromeEnv, ChromeProfile};
pub use snapshot::{ExtSnapshot, InstanceMap};

/// Normalize a URL for equality checks between capture and restore
/// (Chrome may add or drop a trailing slash after navigation).
pub fn normalize_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}
