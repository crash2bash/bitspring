//! The key an output's processing settings are remembered under.
//!
//! Each output has its own processing (upsampling, DSD output, crossfeed and the rest), so the
//! settings need a name for the output that stays the same from one connection to the next.
//! The uid does that on macOS, Windows, iOS and for network players. On Android it doesn't:
//! the uid carries the `AudioDeviceInfo` id, and Android hands out a new one every time a
//! device is plugged in again. There the key is the kind of connection and the name the
//! device gives itself, which a replug keeps.

use hifi_engine::output::Transport;
use hifi_engine::player::Output;

/// The settings key for the output with this `uid`, `transport` and `name`.
pub fn settings_key(uid: &str, transport: Transport, name: &str) -> String {
    if uid.starts_with("android:") {
        format!("android:{transport:?}:{}", name.trim())
    } else {
        uid.to_string()
    }
}

/// [`settings_key`] of `device`.
pub fn of(device: &Output) -> String {
    settings_key(device.uid(), device.transport(), device.name())
}

/// The settings key of `uid` when the uid alone tells it (everything but Android, where the
/// device itself has to be looked at).
pub fn of_uid(uid: &str) -> Option<String> {
    (!uid.starts_with("android:")).then(|| uid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_outputs_are_known_by_connection_and_name() {
        assert_eq!(settings_key("android:17", Transport::Usb, "FiiO Q7"), "android:Usb:FiiO Q7");
        assert_eq!(settings_key("android:3", Transport::BuiltIn, "Speaker"), "android:BuiltIn:Speaker");
        // Plugged in again: another id, the same key.
        assert_eq!(settings_key("android:42", Transport::Usb, "FiiO Q7"), settings_key("android:17", Transport::Usb, "FiiO Q7"));
        // The name as shown: no stray spaces.
        assert_eq!(settings_key("android:5", Transport::Bluetooth, "  AirPods Pro \n"), "android:Bluetooth:AirPods Pro");
        // The same name over another connection is another output.
        assert_ne!(settings_key("android:5", Transport::Bluetooth, "Q7"), settings_key("android:6", Transport::Usb, "Q7"));
        assert_eq!(of_uid("android:17"), None);
    }

    #[test]
    fn other_outputs_are_known_by_their_uid() {
        for uid in ["AppleUSBAudioEngine:FiiO:Q7:1234:1", "{0.0.0.00000000}.{a1b2}", "upnp:uuid:5e1f", "cast:shield", "ios:route"] {
            assert_eq!(settings_key(uid, Transport::Usb, "FiiO Q7"), uid);
            assert_eq!(of_uid(uid).as_deref(), Some(uid));
        }
        // The name doesn't matter there: a renamed device keeps its settings.
        assert_eq!(settings_key("dac", Transport::Usb, "Renamed"), "dac");
    }
}
