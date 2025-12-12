use anyhow::Context;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Init basic logging to stdout using tracing
pub fn init_tracing(verbosity: u8) -> anyhow::Result<()> {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(format!("magnet={level}")))
        .context("failed to create tracing filter")?;

    tracing_subscriber::registry()
        .with(fmt::layer().without_time())
        .with(filter)
        .try_init()
        .context("failed to initialize tracing subscriber")?;

    Ok(())
}
