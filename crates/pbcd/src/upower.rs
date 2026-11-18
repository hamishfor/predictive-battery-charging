//! UPower's charge limit, which GNOME Settings shows as Power → Battery Charging.
//!
//! With `upower_switch = true` (the default), that setting is Predictive Battery Charging's
//! on/off switch:
//! - "Preserve Battery Health" (limit enabled) turns it on.
//! - "Maximize Charge" (limit disabled) turns it off. pbcd pauses and the
//!   battery charges to 100%.
//!
//! While its limit is enabled, UPower writes its own thresholds (often 75/80) at
//! every boot and on every restart. pbcd sets its own graded level and corrects
//! UPower's write on its next sample, so a restart costs at most a minute at
//! UPower's level.
//!
//! With `upower_switch = false`, pbcd pauses while UPower's limit is on, so the
//! two don't fight.

use crate::alert::Pause;
use zbus::proxy;

#[proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
pub trait Device {
    fn enable_charge_threshold(&self, enabled: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn charge_threshold_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn charge_threshold_supported(&self) -> zbus::Result<bool>;
}

pub async fn device<'a>(conn: &zbus::Connection, battery: &str) -> zbus::Result<DeviceProxy<'a>> {
    DeviceProxy::builder(conn)
        .path(format!("/org/freedesktop/UPower/devices/battery_{battery}"))?
        .build()
        .await
}

/// Returns Some(enabled) if UPower is present and supports thresholds.
pub async fn limit_enabled(dev: &DeviceProxy<'_>) -> Option<bool> {
    if !dev.charge_threshold_supported().await.unwrap_or(false) {
        return None;
    }
    dev.charge_threshold_enabled().await.ok()
}

/// What pbcd should do, given how UPower's limit is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// pbcd controls charging.
    Run,
    /// pbcd must not write.
    Pause(Pause),
}

/// Decide from the switch mode and UPower's limit (`None`: UPower absent, or no threshold support).
pub fn verdict(switch: bool, limit: Option<bool>) -> Verdict {
    match (switch, limit) {
        // No UPower switch available: there's nothing to follow, so run.
        (_, None) => Verdict::Run,
        (true, Some(true)) => Verdict::Run,
        (true, Some(false)) => Verdict::Pause(Pause::MaximizeCharge),
        (false, Some(true)) => Verdict::Pause(Pause::UpowerLimitOn),
        (false, Some(false)) => Verdict::Run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_switch() {
        assert_eq!(verdict(true, Some(true)), Verdict::Run);
        assert!(verdict(true, Some(false)) == Verdict::Pause(Pause::MaximizeCharge));
        assert_eq!(verdict(true, None), Verdict::Run);
    }

    #[test]
    fn legacy_yield() {
        assert_eq!(
            verdict(false, Some(true)),
            Verdict::Pause(Pause::UpowerLimitOn)
        );
        assert_eq!(verdict(false, Some(false)), Verdict::Run);
        assert_eq!(verdict(false, None), Verdict::Run);
    }
}
