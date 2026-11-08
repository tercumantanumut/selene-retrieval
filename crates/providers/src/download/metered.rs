//! Metered-connection check (Amendment 1: "downloads pause on metered connections").
//!
//! `INetworkCostManager::GetCost` (Network List Manager, no admin, no network traffic) returns the
//! machine-wide connection cost Windows shows in Settings → Network ("Metered connection"). Cost
//! flags `FIXED`, `VARIABLE`, `OVERDATALIMIT` and `ROAMING` count as metered; `UNRESTRICTED` and
//! `UNKNOWN` (no network, or the service is unavailable) do not, so an unknown state never blocks a
//! download the user asked for (it simply fails as `Offline` if there is no network).
//!
//! COM runs on a short-lived thread of its own (MTA), so it never touches the apartment of a tokio
//! worker or the UI thread.

/// Raw `NLM_CONNECTION_COST` flags, `None` if the query failed.
pub fn connection_cost() -> Option<u32> {
    imp::connection_cost()
}

/// Bits that mean "metered" (data plan with a cap, pay per byte, over the limit, roaming).
pub const METERED_MASK: u32 = 0x2 | 0x4 | 0x1_0000 | 0x4_0000;

/// `cost` describes a metered connection.
pub const fn cost_is_metered(cost: u32) -> bool {
    cost & METERED_MASK != 0
}

/// Whether the current internet connection is metered (false if unknown).
pub fn is_metered() -> bool {
    connection_cost().is_some_and(cost_is_metered)
}

#[cfg(not(windows))]
mod imp {
    pub fn connection_cost() -> Option<u32> {
        None
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 COM: CoInitializeEx / CoCreateInstance / INetworkCostManager. Reviewed (M2-G).
mod imp {
    use windows::Win32::Networking::NetworkListManager::{INetworkCostManager, NetworkListManager};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };

    pub fn connection_cost() -> Option<u32> {
        std::thread::Builder::new()
            .name("sr-nlm-cost".into())
            .spawn(|| {
                // SAFETY: a fresh thread with no COM state; balanced by CoUninitialize below.
                let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if hr.is_err() {
                    return None;
                }
                let r = (|| {
                    // SAFETY: documented CLSID/interface pair; in-proc server (netprofm).
                    let m: INetworkCostManager =
                        unsafe { CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL) }.ok()?;
                    let mut cost = 0u32;
                    // SAFETY: valid out pointer; null destination = machine-wide cost.
                    unsafe { m.GetCost(&mut cost, std::ptr::null()) }.ok()?;
                    Some(cost)
                })();
                // SAFETY: matches the successful CoInitializeEx on this thread (the interface
                // above was released when the closure returned).
                unsafe { CoUninitialize() };
                r
            })
            .ok()?
            .join()
            .ok()?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_flags() {
        assert!(!cost_is_metered(0)); // unknown
        assert!(!cost_is_metered(0x1)); // unrestricted
        assert!(!cost_is_metered(0x2_0000)); // congested only
        assert!(cost_is_metered(0x2)); // fixed
        assert!(cost_is_metered(0x4)); // variable
        assert!(cost_is_metered(0x1 | 0x1_0000)); // over data limit
        assert!(cost_is_metered(0x4_0000)); // roaming
    }

    #[test]
    fn query_is_fast_and_does_not_panic() {
        let t = std::time::Instant::now();
        let c = connection_cost();
        println!("connection cost: {c:?} metered={}", is_metered());
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
        #[cfg(windows)]
        assert!(c.is_some(), "Network List Manager answers on Windows");
    }
}
