//! Native iSponsorBlockTV configuration and runtime boundaries.

pub mod config;

pub mod api;
pub mod cli;
pub mod discovery;
pub mod http;
pub mod lounge;
pub mod runtime;
pub mod setup;

use cli::{Cli, Command};
use config::{ConfigDocument, ConfigError, ConfigStore, Environment, LogLevel, resolve_data_dir};
use http::Endpoints;
use setup::{PromptIo, SetupError};
use tokio_util::sync::CancellationToken;

pub struct AppContext {
    pub environment: Environment,
    pub endpoints: Endpoints,
    pub shutdown: CancellationToken,
    pub prompt: Option<PromptIo>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Setup(#[from] SetupError),
    #[error(transparent)]
    Runtime(#[from] runtime::RuntimeError),
}

/// A single loaded configuration snapshot and its explicitly injected services.
/// Prepare before installing the binary's subscriber, so spawned actors share
/// the configured process-wide filter without reusable code installing one.
pub struct PreparedApp {
    cli: Cli,
    context: AppContext,
    store: ConfigStore,
    document: ConfigDocument,
    log_level: LogLevel,
}

impl PreparedApp {
    pub fn load(cli: Cli, context: AppContext) -> Result<Self, AppError> {
        let data_dir = resolve_data_dir(cli.data.as_deref(), &context.environment)?;
        let (store, document) = ConfigStore::load(data_dir, context.environment.docker)?;
        let configured_level = document.log_level()?;
        let log_level = if cli.debug {
            LogLevel::Debug
        } else {
            configured_level
        };
        Ok(Self {
            cli,
            context,
            store,
            document,
            log_level,
        })
    }

    pub fn log_level(&self) -> LogLevel {
        self.log_level
    }
}

/// Run a prepared snapshot under the caller's existing tracing subscriber.
pub async fn run_app(app: PreparedApp) -> Result<(), AppError> {
    let PreparedApp {
        cli,
        mut context,
        store,
        mut document,
        ..
    } = app;
    match cli.selected_command() {
        Command::Setup => {
            drop(context.prompt);
            setup::tui::run(
                &store,
                document,
                context.endpoints,
                context.shutdown,
                cli.http_tracing,
            )
            .await?;
        }
        Command::Start => {
            let config = document.runtime_config()?;
            for warning in &config.warnings {
                tracing::warn!("{warning}");
            }
            if context.endpoints.sponsorblock == Endpoints::default().sponsorblock {
                context.endpoints.sponsorblock = config.sponsorblock_api_url.clone();
            }
            drop(context.prompt);
            runtime::run(
                config,
                context.endpoints,
                cli.http_tracing,
                context.shutdown,
            )
            .await?;
        }
        Command::SetupCli => {
            let mut prompt = match context.prompt.take() {
                Some(prompt) => prompt,
                None => PromptIo::stdio()?,
            };
            setup::cli::run(
                &store,
                &mut document,
                context.endpoints,
                &mut prompt,
                context.shutdown,
                cli.http_tracing,
            )
            .await?;
        }
    }
    Ok(())
}
