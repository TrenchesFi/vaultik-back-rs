use std::future::Future;
use std::net::SocketAddr;
use std::str::FromStr;

use alloy::primitives::{Address, U256};
use anyhow::{Context, Result};
use axum::{
	Json, Router,
	extract::{Path, State},
	response::IntoResponse,
	routing::get,
};
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::dto::ApyResponseDto;
use crate::service::MorphoService;
use alloy_provider::Provider;
use common_mem::ErasedRef;

pub async fn serve_http<P, F>(service: &MorphoService<P>, addr: SocketAddr, shutdown: F) -> Result<()>
where
	P: Provider + Clone + Send + Sync + 'static,
	F: Future<Output = ()> + Send + 'static,
{
	let erased = ErasedRef::new(service);
	let app = Router::new()
		.route("/apy/{vault_addr}", get(apy_handler::<P>))
		.route(
			"/apy/{vault_addr}/after-deposit/{amount}",
			get(apy_after_deposit_handler::<P>),
		)
		.with_state(erased)
		.layer(
			TraceLayer::new_for_http()
				.make_span_with(DefaultMakeSpan::new().level(Level::INFO))
				.on_request(DefaultOnRequest::new().level(Level::INFO))
				.on_response(DefaultOnResponse::new().level(Level::INFO))
				.on_failure(DefaultOnFailure::new().level(Level::ERROR)),
		);

	let listener = tokio::net::TcpListener::bind(addr).await?;
	axum::serve(listener, app)
		.with_graceful_shutdown(shutdown)
		.await
		.context("serve http")?;

	Ok(())
}

async fn apy_handler<P>(State(erased): State<ErasedRef>, Path(vault_addr): Path<String>) -> impl IntoResponse
where
	P: Provider + Clone,
{
	let service: &MorphoService<P> = unsafe { erased.get() };
	match Address::from_str(&vault_addr) {
		Ok(addr) => match service.compute_vault_net_apy_percent(addr).await {
			Ok(apy) => {
				let body = ApyResponseDto {
					vault: addr.to_string(),
					apy,
				};
				Json(body).into_response()
			}
			Err(err) => {
				tracing::error!(error = %err, "compute apy failed");
				axum::http::StatusCode::BAD_GATEWAY.into_response()
			}
		},
		Err(_) => axum::http::StatusCode::BAD_REQUEST.into_response(),
	}
}

async fn apy_after_deposit_handler<P>(
	State(erased): State<ErasedRef>,
	Path((vault_addr, amount)): Path<(String, String)>,
) -> impl IntoResponse
where
	P: Provider + Clone,
{
	let amount = amount
		.replace(",", "")
		.replace(".", "")
		.replace(" ", "")
		.replace("_", "");
	let amount = match U256::from_str(&amount) {
		Ok(amount) => amount,
		Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
	};
	let service: &MorphoService<P> = unsafe { erased.get() };
	match Address::from_str(&vault_addr) {
		Ok(addr) => match service.compute_vault_net_apy_after_deposit(addr, amount).await {
			Ok(apy) => {
				let body = ApyResponseDto {
					vault: addr.to_string(),
					apy,
				};
				Json(body).into_response()
			}
			Err(err) => {
				tracing::error!(error = %err, "compute apy failed");
				axum::http::StatusCode::BAD_GATEWAY.into_response()
			}
		},
		Err(_) => axum::http::StatusCode::BAD_REQUEST.into_response(),
	}
}
