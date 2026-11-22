//! Read or retire an existing local runtime without starting a replacement.

use std::path::Path;
use std::time::Duration;

use leveler_client_protocol::{RestartReason, RetireDecision, RetireRequest, RuntimeInfo};
use leveler_local_transport::{ClientKind, LocalRuntimeService, TransportError};

use crate::client::{EndpointProbe, probe_endpoint_as};

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

pub enum RuntimeInspection {
    Absent,
    Unresponsive,
    Legacy {
        info: Option<Box<RuntimeInfo>>,
        local_waiters: Option<usize>,
    },
    Authenticated {
        info: Option<Box<RuntimeInfo>>,
        local_waiters: Option<usize>,
    },
}

pub enum StopRuntime {
    Absent,
    Stopped,
    Busy {
        active_turns: u32,
        active_background_tasks: u32,
    },
    Legacy,
    IdentityUnknown,
    GenerationChanged,
    Unresponsive,
    OutcomeUnknown,
    ExitUnconfirmed,
}

pub async fn inspect_runtime(path: &Path) -> Result<RuntimeInspection, TransportError> {
    match probe_endpoint_as(path, PROBE_TIMEOUT, ClientKind::Remote).await {
        Ok(EndpointProbe::Absent) => Ok(RuntimeInspection::Absent),
        Ok(EndpointProbe::Ready(client)) => {
            let (info, waiters) = tokio::join!(
                tokio::time::timeout(REQUEST_TIMEOUT, client.runtime_info()),
                tokio::time::timeout(REQUEST_TIMEOUT, client.local_waiter_count()),
            );
            Ok(RuntimeInspection::Authenticated {
                info: info.ok().and_then(Result::ok).map(Box::new),
                local_waiters: waiters.ok().and_then(Result::ok),
            })
        }
        Ok(_) => Ok(RuntimeInspection::Unresponsive),
        Err(TransportError::PredatesAccessToken(_)) => {
            #[cfg(unix)]
            let (info, local_waiters) = tokio::join!(
                leveler_local_transport::legacy_runtime_info(path, PROBE_TIMEOUT),
                leveler_local_transport::legacy_local_waiter_count(path, PROBE_TIMEOUT),
            );
            #[cfg(not(unix))]
            let (info, local_waiters): (Option<RuntimeInfo>, Option<usize>) = (None, None);
            Ok(RuntimeInspection::Legacy {
                info: info.map(Box::new),
                local_waiters,
            })
        }
        Err(error) => Err(error),
    }
}

pub async fn stop_runtime(path: &Path) -> Result<StopRuntime, TransportError> {
    let client = match probe_endpoint_as(path, PROBE_TIMEOUT, ClientKind::Remote).await {
        Ok(EndpointProbe::Absent) => return Ok(StopRuntime::Absent),
        Ok(EndpointProbe::Ready(client)) => client,
        Ok(_) => return Ok(StopRuntime::Unresponsive),
        Err(TransportError::PredatesAccessToken(_)) => return Ok(StopRuntime::Legacy),
        Err(error) => return Err(error),
    };
    let Ok(Ok(info)) = tokio::time::timeout(REQUEST_TIMEOUT, client.runtime_info()).await else {
        return Ok(StopRuntime::IdentityUnknown);
    };
    if info.pid == 0 || !info.build.is_known() {
        return Ok(StopRuntime::IdentityUnknown);
    }

    let request = RetireRequest {
        reason: RestartReason::RestartRequested,
        expected_build: info.build,
        expected_pid: info.pid,
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, client.try_retire_if_idle(request)).await {
        Ok(Ok(RetireDecision::Accepted | RetireDecision::AlreadyRetiring { .. })) => {}
        Ok(Ok(RetireDecision::Busy {
            active_turns,
            active_background_tasks,
        })) => {
            return Ok(StopRuntime::Busy {
                active_turns,
                active_background_tasks,
            });
        }
        Ok(Ok(RetireDecision::Unsupported)) => return Ok(StopRuntime::Legacy),
        Ok(Ok(RetireDecision::GenerationChanged)) => return Ok(StopRuntime::GenerationChanged),
        _ => return Ok(StopRuntime::OutcomeUnknown),
    }
    drop(client);

    let released = async {
        loop {
            match probe_endpoint_as(path, PROBE_TIMEOUT, ClientKind::Remote).await {
                Ok(EndpointProbe::Absent) => break,
                Ok(EndpointProbe::Ready(client)) => {
                    if let Ok(Ok(current)) =
                        tokio::time::timeout(PROBE_TIMEOUT, client.runtime_info()).await
                        && current.pid != 0
                        && current.pid != info.pid
                    {
                        break;
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    Ok(
        if tokio::time::timeout(EXIT_TIMEOUT, released).await.is_ok() {
            StopRuntime::Stopped
        } else {
            StopRuntime::ExitUnconfirmed
        },
    )
}
