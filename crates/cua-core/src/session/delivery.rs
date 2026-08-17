//! Delivery policy and capability snapshots for each input family.
//!
//! These interfaces intentionally stay separate: pointer fallback order,
//! keyboard focus enforcement, and experimental wheel delivery are different
//! contracts and do not benefit from one catch-all delivery trait.

use std::sync::OnceLock;

use super::env_flag;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerStrategy {
    PidOnly,
    AxFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PointerCapability {
    pub strategy: PointerStrategy,
}

pub(super) trait PointerDelivery {
    fn pointer_capability(&self) -> PointerCapability;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyboardStrategy {
    Pid,
    AxOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct KeyboardCapability {
    pub strategy: KeyboardStrategy,
    pub strict_focus: bool,
}

pub(super) trait KeyboardDelivery {
    fn keyboard_capability(&self) -> KeyboardCapability;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScrollCapability {
    pub wheel_enabled: bool,
}

pub(super) trait ScrollDelivery {
    fn scroll_capability(&self) -> ScrollCapability;
}

pub(super) struct DeliveryPolicy;

pub(super) const DELIVERY: DeliveryPolicy = DeliveryPolicy;

impl PointerDelivery for DeliveryPolicy {
    fn pointer_capability(&self) -> PointerCapability {
        static AX_FIRST: OnceLock<bool> = OnceLock::new();
        PointerCapability {
            strategy: if *AX_FIRST.get_or_init(|| env_flag("CUA_AX_FIRST")) {
                PointerStrategy::AxFirst
            } else {
                PointerStrategy::PidOnly
            },
        }
    }
}

impl KeyboardDelivery for DeliveryPolicy {
    fn keyboard_capability(&self) -> KeyboardCapability {
        static PID_ENABLED: OnceLock<bool> = OnceLock::new();
        static STRICT_FOCUS: OnceLock<bool> = OnceLock::new();
        KeyboardCapability {
            strategy: if *PID_ENABLED.get_or_init(|| !env_flag("CUA_KEY_AX_ONLY")) {
                KeyboardStrategy::Pid
            } else {
                KeyboardStrategy::AxOnly
            },
            strict_focus: *STRICT_FOCUS.get_or_init(|| env_flag("CUA_KEY_STRICT_FOCUS")),
        }
    }
}

impl ScrollDelivery for DeliveryPolicy {
    fn scroll_capability(&self) -> ScrollCapability {
        static WHEEL_ENABLED: OnceLock<bool> = OnceLock::new();
        ScrollCapability {
            wheel_enabled: *WHEEL_ENABLED.get_or_init(|| env_flag("CUA_WHEEL_SCROLL")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::session::flag_is_on;

    #[test]
    fn capability_switches_share_the_session_flag_vocabulary() {
        assert!(flag_is_on(Some("1")));
        assert!(flag_is_on(Some("TRUE")));
        assert!(!flag_is_on(Some("yes")));
        assert!(!flag_is_on(None));
    }
}
