//! Best-effort, numeric service-stop evidence. Core exports it only when SCM says stopped.
//! Missing/older records stay unknown; no event-log text or user data is collected.

use std::mem::size_of_val;
use std::ptr;
use windows_sys::Win32::System::Registry as registry;

/// Last lifecycle outcome reported by the service, not an inferred crash diagnosis.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ServiceStopReason {
    Starting = 1,
    StopRequested = 2,
    Shutdown = 3,
    OwnerRemoved = 4,
    StartupFailed = 5,
    BrokerFailed = 6,
    RegistrationInterrupted = 7,
}

impl ServiceStopReason {
    /// Called by the service, which owns the key. Failure must not affect its lifecycle.
    pub fn record(self, hresult: u32) {
        let Ok(service_name) = crate::windows_sandbox_service_name() else {
            return;
        };
        let key = crate::to_wide(format!(r"SYSTEM\CurrentControlSet\Services\{service_name}"));
        let value = ((self as u64) << 32) | u64::from(hresult);
        let mut handle = ptr::null_mut();
        unsafe {
            // Do not recreate a service key that Windows is uninstalling.
            if registry::RegOpenKeyExW(
                registry::HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                0,
                registry::KEY_SET_VALUE,
                &mut handle,
            ) != 0
            {
                return;
            }
            registry::RegSetValueExW(
                handle,
                windows_sys::w!("CodexLastStop"),
                0,
                registry::REG_QWORD,
                ptr::from_ref(&value).cast(),
                size_of_val(&value) as u32,
            );
            registry::RegCloseKey(handle);
        }
    }
}
