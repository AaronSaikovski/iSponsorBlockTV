//! Native multi-device service. Endpoint injection is constructor-only; device
//! actors own all protocol work and share the immutable SponsorBlock service.
mod device;
pub mod scheduler;

use crate::{
    api::{ApiError, sponsorblock::ApiService},
    config::RuntimeConfig,
    http::{Endpoints, HttpClients, HttpError},
    lounge::{LoungeError, LoungeTransport},
};
use std::{sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Lounge(#[from] LoungeError),
    #[error("the runtime requires at least one configured device")]
    NoDevices,
    #[error("a device task terminated unexpectedly")]
    DeviceTask,
}

/// Run until cancellation. All devices get the same three-second termination
/// deadline, rather than a separate serial budget for each device.
pub async fn run(
    config: RuntimeConfig,
    endpoints: Endpoints,
    http_tracing: bool,
    shutdown: CancellationToken,
) -> Result<(), RuntimeError> {
    if config.devices.is_empty() {
        return Err(RuntimeError::NoDevices);
    }
    let clients = HttpClients::new(config.use_proxy, http_tracing)?;
    let api = Arc::new(ApiService::with_http(
        &config,
        endpoints.clone(),
        clients.clone(),
    )?);
    let config = Arc::new(config);
    let actors_shutdown = CancellationToken::new();
    let mut actors = JoinSet::new();
    // Construct every transport before starting tasks so a constructor failure
    // cannot leave a partially started supervisor behind.
    let transports = config
        .devices
        .iter()
        .map(|device| {
            LoungeTransport::new(
                clients.clone(),
                endpoints.lounge.clone(),
                device.screen_id.clone(),
                config.join_name.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    tracing::info!(
        devices = config.devices.len(),
        "Starting device-control service"
    );
    for (index, (device, transport)) in config.devices.iter().cloned().zip(transports).enumerate() {
        let actor = device::DeviceActor::new(device, config.clone(), api.clone(), transport);
        actors.spawn(
            actor
                .run(actors_shutdown.clone())
                .instrument(tracing::info_span!("device", number = index + 1)),
        );
    }
    let mut failed = false;
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {},
        _ = actors.join_next() => { failed = true; },
    }
    tracing::info!("Stopping device-control service");
    let deadline = Instant::now() + Duration::from_secs(3);
    actors_shutdown.cancel();
    if tokio::time::timeout_at(deadline, async {
        while let Some(result) = actors.join_next().await {
            if result.is_err() {
                failed = true;
            }
        }
    })
    .await
    .is_err()
    {
        tracing::warn!("Device shutdown deadline exceeded; cancelling remaining tasks");
        actors.abort_all();
        while actors.join_next().await.is_some() {}
    }
    if failed {
        tracing::error!("Device-control service stopped after a device task failure");
        Err(RuntimeError::DeviceTask)
    } else {
        tracing::info!("Device-control service stopped");
        Ok(())
    }
}
