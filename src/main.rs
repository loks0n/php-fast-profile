use anyhow::Result;
use clap::Parser;

mod cli;
mod discover;
mod offsets;
mod output;
mod proc;
mod remote;
mod sampler;
mod symbols;
mod target;
#[cfg(feature = "tui")]
mod tui;
mod zend;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = cli::Args::parse();
    cli::run(args)
}
