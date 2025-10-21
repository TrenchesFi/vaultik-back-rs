use anyhow::{Context, Result};

use alloy_provider::ProviderBuilder;
use indexer_morpho::http::serve_http;
use indexer_morpho::service::MorphoService;

use crate::config::Config;

mod config;

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.with_target(false)
		.compact()
		.init();

	let config: Config = app_config::resolve_config_from_args().context("failed to load config")?;
	tracing::info!("Config loaded");

	let rpc_url = std::env::var("MORPHO_RPC").unwrap_or_else(|_| "https://rpc.hyperliquid.xyz/evm".to_string());

	let provider = ProviderBuilder::new().connect(&rpc_url).await?;
	let service = MorphoService::new(provider);

	let addr: std::net::SocketAddr = config.listen_addr.parse().context("parse listen addr")?;
	tracing::info!(%addr, "HTTP server listening");
	serve_http(&service, addr, shutdown_signal()).await?;

	Ok(())
}
async fn shutdown_signal() {
	#[cfg(unix)]
	{
		let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
			.expect("failed to install SIGTERM handler");
		tokio::select! {
			_ = tokio::signal::ctrl_c() => {  }
			_ = term_signal.recv() => {  }
		}
		return;
	}

	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
}
