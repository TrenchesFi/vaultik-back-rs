use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ApyResponseDto {
	pub vault: String,
	pub apy: String,
}
