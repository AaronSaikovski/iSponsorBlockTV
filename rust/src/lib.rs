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
use config::{ConfigError, ConfigStore, Environment, resolve_data_dir};
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

pub async fn run_app(cli: Cli, mut context: AppContext) -> Result<(), AppError> {
    let data_dir = resolve_data_dir(cli.data.as_deref(), &context.environment)?;
    let (store, mut document) = ConfigStore::load(data_dir, context.environment.docker)?;
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
