use std::{
    sync::{LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use super::ipc_service::IPCService;

const IPC_RECONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

fn reconnect_is_due(
    active_attempt_id: Option<u64>,
    last_attempt: Option<Instant>,
    now: Instant,
    force: bool,
) -> bool {
    active_attempt_id.is_none()
        && (force
            || last_attempt.is_none_or(|last_attempt| {
                now.saturating_duration_since(last_attempt) >= IPC_RECONNECT_RETRY_INTERVAL
            }))
}

#[derive(Debug)]
pub struct IMEState {
    pub ipc_service: Option<IPCService>,
    active_ipc_reconnect_attempt: Option<u64>,
    next_ipc_reconnect_attempt: u64,
    last_ipc_reconnect_attempt: Option<Instant>,
}

pub static IME_STATE: LazyLock<Mutex<IMEState>> = LazyLock::new(|| {
    tracing::debug!("Creating IMEState");
    Mutex::new(IMEState {
        ipc_service: None,
        active_ipc_reconnect_attempt: None,
        next_ipc_reconnect_attempt: 0,
        last_ipc_reconnect_attempt: None,
    })
});
unsafe impl Sync for IMEState {}
unsafe impl Send for IMEState {}

impl IMEState {
    fn lock_blocking() -> MutexGuard<'static, IMEState> {
        IME_STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Clones the currently published active connection while holding the global mutex only for
    /// the clone. Key processing must not interpret a transient `try_lock` collision as an IPC
    /// outage, and no RPC is ever issued while this mutex is held.
    pub fn ipc_snapshot() -> Option<IPCService> {
        Self::lock_blocking()
            .ipc_service
            .as_ref()
            .filter(|service| service.is_active())
            .cloned()
    }

    /// Installs one freshly connected service without replacing a newer connection. The
    /// connection becomes usable only while it is the service stored in this state.
    pub fn install_ipc_if_absent(ipc_service: IPCService) -> anyhow::Result<bool> {
        let mut state = Self::lock_blocking();
        if state.ipc_service.is_some() {
            return Ok(false);
        }

        ipc_service.activate();
        state.ipc_service = Some(ipc_service);
        Ok(true)
    }

    /// Removes exactly the connection that reported a failure. A delayed error from an old
    /// clone cannot invalidate a newer service with a different connection id.
    pub fn invalidate_ipc(connection_id: u64) {
        let removed = {
            let mut state = Self::lock_blocking();
            let is_current = state
                .ipc_service
                .as_ref()
                .is_some_and(|service| service.connection_id() == connection_id);
            if is_current {
                let service = state.ipc_service.take();
                if let Some(service) = service.as_ref() {
                    service.deactivate();
                }
                service
            } else {
                None
            }
        };

        if removed.is_some() {
            // Drop the Tokio runtime/channel clone outside the global state mutex.
            drop(removed);
            if let Err(error) = Self::start_ipc_reconnect_inner(true) {
                tracing::warn!("Failed to schedule IPC reconnect after invalidation: {error:?}");
            }
        }
    }

    /// Returns whether conversion IPC is ready. If it is not, starts one throttled background
    /// connection attempt and immediately returns false so ordinary input can remain responsive.
    pub fn ipc_available_or_start_reconnect() -> bool {
        if Self::ipc_snapshot().is_some() {
            return true;
        }

        if let Err(error) = Self::start_ipc_reconnect() {
            tracing::warn!("Failed to schedule IPC reconnect: {error:?}");
        }
        false
    }

    pub fn start_ipc_reconnect() -> anyhow::Result<()> {
        Self::start_ipc_reconnect_inner(false)
    }

    fn start_ipc_reconnect_inner(force: bool) -> anyhow::Result<()> {
        let now = Instant::now();
        let attempt_id = {
            let mut state = Self::lock_blocking();
            if state.ipc_service.is_some()
                || !reconnect_is_due(
                    state.active_ipc_reconnect_attempt,
                    state.last_ipc_reconnect_attempt,
                    now,
                    force,
                )
            {
                None
            } else {
                state.next_ipc_reconnect_attempt = state.next_ipc_reconnect_attempt.wrapping_add(1);
                let attempt_id = state.next_ipc_reconnect_attempt;
                state.active_ipc_reconnect_attempt = Some(attempt_id);
                state.last_ipc_reconnect_attempt = Some(now);
                Some(attempt_id)
            }
        };

        let Some(attempt_id) = attempt_id else {
            return Ok(());
        };

        let spawn_result = std::thread::Builder::new()
            .name("azookey-ipc-reconnect".to_owned())
            .spawn(move || {
                // Connecting does not call AppendText/ClearText. Server composition belongs to
                // the focused TIP and is reset lazily by its first stateful RPC.
                let connection_result = IPCService::new();
                let mut discarded_service = None;
                {
                    let mut state = Self::lock_blocking();
                    if state.active_ipc_reconnect_attempt != Some(attempt_id) {
                        discarded_service = connection_result.ok();
                    } else {
                        state.active_ipc_reconnect_attempt = None;
                        match connection_result {
                            Ok(ipc_service) if state.ipc_service.is_none() => {
                                ipc_service.activate();
                                state.ipc_service = Some(ipc_service);
                                tracing::info!("Reconnected AzooKey conversion IPC");
                            }
                            Ok(ipc_service) => {
                                tracing::debug!("Discard stale IPC reconnect result");
                                discarded_service = Some(ipc_service);
                            }
                            Err(error) => {
                                tracing::debug!(
                                    "AzooKey IPC reconnect is not ready yet: {error:?}"
                                );
                            }
                        }
                    }
                }
                // Dropping the last Runtime/Channel may wait for driver shutdown. Never do that
                // while the global state mutex is held.
                drop(discarded_service);
            });

        if let Err(error) = spawn_result {
            let mut state = Self::lock_blocking();
            if state.active_ipc_reconnect_attempt == Some(attempt_id) {
                state.active_ipc_reconnect_attempt = None;
            }
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
        assert!(reconnect_is_due(None, None, start, false));
        assert!(!reconnect_is_due(Some(1), None, start, false));
        assert!(!reconnect_is_due(
            None,
            Some(start),
            start + IPC_RECONNECT_RETRY_INTERVAL - Duration::from_millis(1),
            false,
        ));
        assert!(reconnect_is_due(
            None,
            Some(start),
            start + IPC_RECONNECT_RETRY_INTERVAL,
            false,
        ));
    }

    #[test]
    fn failure_forces_retry_without_allowing_duplicate_worker() {
        let start = Instant::now();
        assert!(reconnect_is_due(None, Some(start), start, true));
        assert!(!reconnect_is_due(Some(2), Some(start), start, true));
    }
}
