use std::process::ExitCode;

use clap::Parser;
use isponsorblocktv::{
    AppContext, AppError, PreparedApp, cli::Cli, config::Environment, http::Endpoints, run_app,
    setup::SetupError,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let environment = match Environment::capture() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let shutdown = CancellationToken::new();
    let app = match PreparedApp::load(
        cli,
        AppContext {
            environment,
            endpoints: Endpoints::default(),
            shutdown: shutdown.clone(),
            prompt: None,
        },
    ) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(app.log_level().filter())
        .with_writer(std::io::stderr)
        .init();
    let signal_token = shutdown.clone();
    let signal = tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                result = tokio::signal::ctrl_c() => result?,
                _ = terminate.recv() => {},
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
        signal_token.cancel();
        Ok::<(), std::io::Error>(())
    });
    let result = run_app(app).await;
    signal.abort();
    let _ = signal.await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(AppError::Setup(SetupError::Interrupted)) => ExitCode::from(130),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
