use clap::Parser;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Debug, Parser)]
struct Cli {
    /// Transport endpoint: stdio, stdio://, or grpc://IP:PORT.
    #[arg(long, value_name = "URL", default_value = codex_code_mode_host::DEFAULT_LISTEN_URL)]
    listen: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        .init();
    codex_code_mode_host::run_main(&cli.listen).await
}
