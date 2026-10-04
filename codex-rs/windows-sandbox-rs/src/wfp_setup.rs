use crate::install_wfp_filters_for_account;
use anyhow::Result;
use std::path::Path;

fn panic_payload_to_string(panic_payload: Box<dyn std::any::Any + Send>) -> String {
    match panic_payload.downcast::<String>() {
        Ok(message) => *message,
        Err(panic_payload) => match panic_payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "unknown panic payload".to_string(),
        },
    }
}

pub fn install_wfp_filters<F>(_codex_home: &Path, offline_username: &str, mut log: F) -> Result<()>
where
    F: FnMut(&str),
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        install_wfp_filters_for_account(offline_username)
    })) {
        Ok(Ok(installed_filter_count)) => {
            log(&format!(
                "WFP setup succeeded for {offline_username} with {installed_filter_count} installed filters"
            ));
            Ok(())
        }
        Ok(Err(err)) => {
            log(&format!("WFP setup failed for {offline_username}: {err}"));
            Err(err)
        }
        Err(payload) => {
            let error = panic_payload_to_string(payload);
            log(&format!(
                "WFP setup panicked for {offline_username}: {error}"
            ));
            Err(anyhow::anyhow!("panic: {error}"))
        }
    }
}
