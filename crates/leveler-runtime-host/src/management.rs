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
    /// A pre-token runtime still has a local UI attached. It is not asked to
    /// retire: the request would close admission under the person using it.
    LegacyAttached {
        local_waiters: usize,
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

/// Ask the runtime serving `path` to exit if no work would be interrupted.
///
/// A runtime that predates the access token cannot authenticate the atomic
/// retirement question, but it does honor the cooperative `shutdown_when_idle`
/// request. It is sent only after the SAME identity gate the startup handshake
/// uses: the endpoint must report no work, no attached local UI, and a pid
/// owned by this user whose command line names this workspace. A runtime that
/// does not meet that gate is reported, never signalled.
pub async fn stop_runtime(
    path: &Path,
    workspace: Option<&Path>,
) -> Result<StopRuntime, TransportError> {
    let client = match probe_endpoint_as(path, PROBE_TIMEOUT, ClientKind::Remote).await {
        Ok(EndpointProbe::Absent) => return Ok(StopRuntime::Absent),
        Ok(EndpointProbe::Ready(client)) => client,
        Ok(_) => return Ok(StopRuntime::Unresponsive),
        Err(TransportError::PredatesAccessToken(_)) => {
            return stop_legacy_runtime(path, workspace).await;
        }
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

/// A pre-token runtime can only be asked cooperatively. Every fact in the
/// refusal is read from the runtime itself; nothing is inferred from silence.
#[cfg(unix)]
async fn stop_legacy_runtime(
    path: &Path,
    workspace: Option<&Path>,
) -> Result<StopRuntime, TransportError> {
    let (info, local_waiters) = tokio::join!(
        leveler_local_transport::legacy_runtime_info(path, REQUEST_TIMEOUT),
        leveler_local_transport::legacy_local_waiter_count(path, REQUEST_TIMEOUT),
    );
    let Some(info) = info else {
        return Ok(StopRuntime::Legacy);
    };
    if let Some(waiters) = local_waiters
        && waiters > 0
    {
        return Ok(StopRuntime::LegacyAttached {
            local_waiters: waiters,
        });
    }
    if info.health.turn_capacity.is_some()
        && (info.health.active_turns > 0 || info.health.active_background_tasks > 0)
    {
        return Ok(StopRuntime::Busy {
            active_turns: info.health.active_turns,
            active_background_tasks: info.health.active_background_tasks,
        });
    }
    // Another shell (or an earlier invocation) already asked; this is an
    // accepted retirement that is draining, not a version that cannot retire.
    if info.health.shutting_down {
        return Ok(if wait_legacy_release(path).await {
            StopRuntime::Stopped
        } else {
            StopRuntime::ExitUnconfirmed
        });
    }
    let Some(_verified) =
        crate::client::legacy_cooperative_retirement_target(path, workspace).await
    else {
        // The two-observation gate can lose a race with another client's
        // retirement request or with a UI attaching. Re-observe BOTH facts so
        // the refusal names the one that actually blocks.
        let (after, waiters) = tokio::join!(
            leveler_local_transport::legacy_runtime_info(path, REQUEST_TIMEOUT),
            leveler_local_transport::legacy_local_waiter_count(path, REQUEST_TIMEOUT),
        );
        if let Some(waiters) = waiters
            && waiters > 0
        {
            return Ok(StopRuntime::LegacyAttached {
                local_waiters: waiters,
            });
        }
        if after.is_some_and(|info| info.health.shutting_down) {
            return Ok(if wait_legacy_release(path).await {
                StopRuntime::Stopped
            } else {
                StopRuntime::ExitUnconfirmed
            });
        }
        return Ok(StopRuntime::Legacy);
    };
    match leveler_local_transport::legacy_request_shutdown_when_idle(
        path,
        RestartReason::RestartRequested,
        REQUEST_TIMEOUT,
        info.pid,
    )
    .await
    {
        Ok(()) => {}
        Err(error) => {
            // A missing acknowledgement does not prove the request was
            // ignored: the frame may have been processed and the reply lost.
            // Ask the runtime's own state before deciding what to report.
            let after = leveler_local_transport::legacy_runtime_info(path, REQUEST_TIMEOUT).await;
            match after {
                None => return Ok(StopRuntime::Stopped),
                Some(info) if info.health.shutting_down => {
                    return Ok(if wait_legacy_release(path).await {
                        StopRuntime::Stopped
                    } else {
                        StopRuntime::ExitUnconfirmed
                    });
                }
                Some(_) => {
                    return Ok(match error {
                        // The peer answered an explicit refusal, or the
                        // connection was never established: nothing ran.
                        TransportError::Unavailable(_) | TransportError::Io(_) => {
                            StopRuntime::Legacy
                        }
                        // The request may have been delivered; its effect is
                        // unknown, which is a different fact from a refusal.
                        _ => StopRuntime::OutcomeUnknown,
                    });
                }
            }
        }
    }
    Ok(if wait_legacy_release(path).await {
        StopRuntime::Stopped
    } else {
        StopRuntime::ExitUnconfirmed
    })
}

/// The old owner released the endpoint: it is absent, or a current-generation
/// runtime now answers on it.
#[cfg(unix)]
async fn legacy_endpoint_released(path: &Path) -> bool {
    matches!(
        probe_endpoint_as(path, PROBE_TIMEOUT, ClientKind::Remote).await,
        Ok(EndpointProbe::Absent | EndpointProbe::Ready(_))
    )
}

#[cfg(unix)]
async fn wait_legacy_release(path: &Path) -> bool {
    let released = async {
        while !legacy_endpoint_released(path).await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(EXIT_TIMEOUT, released).await.is_ok()
}

#[cfg(not(unix))]
async fn stop_legacy_runtime(
    _path: &Path,
    _workspace: Option<&Path>,
) -> Result<StopRuntime, TransportError> {
    Ok(StopRuntime::Legacy)
}
