use std::{
    sync::{LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use super::ipc_service::IPCService;

const IPC_RECONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

fn reconnect_is_due(
    reconnect_in_progress: bool,
    last_attempt: Option<Instant>,
    now: Instant,
) -> bool {
    !reconnect_in_progress
        && last_attempt.is_none_or(|last_attempt| {
            now.saturating_duration_since(last_attempt) >= IPC_RECONNECT_RETRY_INTERVAL
        })
}

#[derive(Debug)]
pub struct IMEState {
    pub ipc_service: Option<IPCService>,
    ipc_reconnect_in_progress: bool,
    last_ipc_reconnect_attempt: Option<Instant>,
}

pub static IME_STATE: LazyLock<Mutex<IMEState>> = LazyLock::new(|| {
    tracing::debug!("Creating IMEState");
    Mutex::new(IMEState {
        ipc_service: None,
        ipc_reconnect_in_progress: false,
        last_ipc_reconnect_attempt: None,
    })
});
unsafe impl Sync for IMEState {}
unsafe impl Send for IMEState {}

impl IMEState {
    pub fn get() -> anyhow::Result<MutexGuard<'static, IMEState>> {
        match IME_STATE.try_lock() {
            Ok(guard) => Ok(guard),
            Err(e) => anyhow::bail!("Failed to lock state: {:?}", e),
        }
    }

    /// Returns whether conversion IPC is ready. If it is not, starts one throttled background
    /// connection attempt and immediately returns false so the TSF key test can pass ordinary
    /// input through to the host application.
    pub fn ipc_available_or_start_reconnect() -> bool {
        let available = match Self::get() {
            Ok(state) => state.ipc_service.is_some(),
            Err(error) => {
                tracing::warn!("Failed to inspect IPC reconnect state: {error:?}");
                return false;
            }
        };

        if available {
            return true;
        }

        if let Err(error) = Self::start_ipc_reconnect() {
            tracing::warn!("Failed to schedule IPC reconnect: {error:?}");
        }
        false
    }

    pub fn start_ipc_reconnect() -> anyhow::Result<()> {
        let now = Instant::now();
        let should_start = {
            let mut state = Self::get()?;
            if state.ipc_service.is_some()
                || !reconnect_is_due(
                    state.ipc_reconnect_in_progress,
                    state.last_ipc_reconnect_attempt,
                    now,
                )
            {
                false
            } else {
                state.ipc_reconnect_in_progress = true;
                state.last_ipc_reconnect_attempt = Some(now);
                true
            }
        };

        if !should_start {
            return Ok(());
        }

        let spawn_result = std::thread::Builder::new()
            .name("azookey-ipc-reconnect".to_owned())
            .spawn(|| {
                let connection_result = (|| -> anyhow::Result<IPCService> {
                    let mut ipc_service = IPCService::new()?;
                    ipc_service.append_text(String::new())?;
                    Ok(ipc_service)
                })();

                // This worker never calls back into TSF while holding the state mutex, so a
                // blocking lock is safe and prevents a transient try_lock failure from leaving
                // reconnect_in_progress stuck forever.
                let mut state = IME_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.ipc_reconnect_in_progress = false;

                match connection_result {
                    Ok(ipc_service) => {
                        if state.ipc_service.is_none() {
                            state.ipc_service = Some(ipc_service);
                            tracing::info!("Reconnected AzooKey IPC services");
                        }
                    }
                    Err(error) => {
                        tracing::debug!("AzooKey IPC reconnect is not ready yet: {error:?}");
                    }
                }
            });

        if let Err(error) = spawn_result {
            let mut state = IME_STATE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.ipc_reconnect_in_progress = false;
            return Err(error.into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_attempts_are_throttled_and_not_duplicated() {
        let start = Instant::now();
        assert!(reconnect_is_due(false, None, start));
        assert!(!reconnect_is_due(true, None, start));
        assert!(!reconnect_is_due(
            false,
            Some(start),
            start + IPC_RECONNECT_RETRY_INTERVAL - Duration::from_millis(1)
        ));
        assert!(reconnect_is_due(
            false,
            Some(start),
            start + IPC_RECONNECT_RETRY_INTERVAL
        ));
    }
}
