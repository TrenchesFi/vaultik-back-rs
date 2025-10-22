use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::SolCall;
use alloy_network::TransactionBuilder;
use alloy_provider::Provider;
use alloy_rpc_client::BatchRequest;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, transaction::TransactionRequest};
use anyhow::{Context, Result};

use crate::contract::{IIrm, IMorpho, MetaMorpho};

/// Number of seconds in one year.  Used to annualize per‑second rates.
const SECONDS_PER_YEAR: u64 = 31_536_000;

/// MorphoService wraps an Alloy provider and exposes methods for computing
/// APYs on MetaMorpho vaults.  The type parameter `P` allows the caller to
/// supply any provider implementation supported by Alloy.
#[derive(Clone)]
pub struct MorphoService<P>
where
	P: Provider + Clone,
{
	provider: P,
}

impl<P> MorphoService<P>
where
	P: Provider + Clone,
{
	/// Construct a new service from an existing provider.
	pub fn new(provider: P) -> Self {
		Self { provider }
	}

	/// Compute the vault's net APY (after fees) as a percentage string.  This
	/// method queries the vault's underlying markets, measures each market's
	/// current supply APY, weights those rates by the vault's exposure, then
	/// applies the vault performance fee.  The result is formatted as a
	/// percentage with two decimal places (e.g. "4.23%").
	pub async fn compute_vault_net_apy_percent(&self, vault_addr: Address) -> Result<String> {
		// Delegate to the generic implementation with zero additional deposit.
		self
			.compute_vault_net_apy_after_deposit(vault_addr, U256::from(0))
			.await
	}

	/// Compute the vault's net APY after depositing a fixed amount of assets
	/// into the vault.  The deposit is expressed in the vault's asset units
	/// (typically the same ERC20 token that the vault accepts).  This
	/// approximation distributes the deposit across all underlying markets
	/// proportionally to the vault's current exposure in each market.  It
	/// assumes the deposit immediately increases the total supply assets for
	/// each market, which lowers utilization and therefore the supply APY.  The
	/// resulting weighted APY is then reduced by the vault performance fee.
	///
	/// # Arguments
	///
	/// * `vault_addr` – The address of the MetaMorpho vault contract.
	/// * `deposit_assets` – The number of asset units that will be deposited
	///   into the vault.  Specify `U256::from(0)` to compute the current
	///   APY without any additional deposit.
	pub async fn compute_vault_net_apy_after_deposit(&self, vault_addr: Address, deposit_assets: U256) -> Result<String> {
		// Prepare the three initial batched calls to the vault: MORPHO(),
		// fee(), and supplyQueueLength().  These calls return the address
		// of the Morpho protocol contract, the vault's performance fee in WAD
		// format, and the number of markets in the vault's supply queue.
		let call_morpho = MetaMorpho::MORPHOCall {};
		let call_fee = MetaMorpho::feeCall {};
		let call_supply_queue_len = MetaMorpho::withdrawQueueLengthCall {};

		let base_tx = TransactionRequest::default().with_to(vault_addr);

		let mut batch = BatchRequest::new(self.provider.client());

		let tx_morpho = base_tx.clone().with_input(Bytes::from(call_morpho.abi_encode()));
		let morpho_waiter = batch
			.add_call::<(TransactionRequest, BlockId), Bytes>("eth_call", &(tx_morpho, BlockNumberOrTag::Pending.into()))
			.context("prepare MORPHO() batch call")?;

		let tx_fee = base_tx.clone().with_input(Bytes::from(call_fee.abi_encode()));
		let fee_waiter = batch
			.add_call::<(TransactionRequest, BlockId), Bytes>("eth_call", &(tx_fee, BlockNumberOrTag::Pending.into()))
			.context("prepare fee() batch call")?;

		let tx_sql = base_tx
			.clone()
			.with_input(Bytes::from(call_supply_queue_len.abi_encode()));
		let sql_waiter = batch
			.add_call::<(TransactionRequest, BlockId), Bytes>("eth_call", &(tx_sql, BlockNumberOrTag::Pending.into()))
			.context("prepare supplyQueueLength() batch call")?;

		// Execute the initial batch.
		batch
			.send()
			.await
			.context("send batch 1 (MORPHO, fee, supplyQueueLength)")?;

		// Decode the responses.
		let morpho_raw = morpho_waiter.await.context("MORPHO() batch response")?;
		let fee_raw = fee_waiter.await.context("fee() batch response")?;
		let sql_raw = sql_waiter.await.context("supplyQueueLength() batch response")?;

		let morpho_addr: Address =
			MetaMorpho::MORPHOCall::abi_decode_returns(morpho_raw.as_ref()).context("decode MORPHO()")?;
		let morpho = IMorpho::new(morpho_addr, self.provider.clone());

		// Directly decode the fee and supply queue length values.
		let vault_fee_wad_u96 = MetaMorpho::feeCall::abi_decode_returns(fee_raw.as_ref())?;
		let vault_fee_wad = U256::from(vault_fee_wad_u96);
		let n_markets_u256 = MetaMorpho::withdrawQueueLengthCall::abi_decode_returns(sql_raw.as_ref())?;
		let n_markets: usize = U256::from(n_markets_u256).to::<u128>() as usize;

		// Load all market IDs in the supply queue.  We issue another batch of
		// eth_call requests to fetch each ID.  The result of supplyQueue(i)
		// returns the bytes32 ID of the i-th market.
		let mut market_ids: Vec<B256> = Vec::with_capacity(n_markets);
		if n_markets > 0 {
			let mut batch2 = BatchRequest::new(self.provider.client());
			let mut waiters = Vec::with_capacity(n_markets);
			for i in 0..n_markets {
				let call = MetaMorpho::withdrawQueueCall {
					index: U256::from(i as u128),
				};
				let tx = base_tx.clone().with_input(Bytes::from(call.abi_encode()));
				let w = batch2
					.add_call::<(TransactionRequest, BlockId), Bytes>("eth_call", &(tx, BlockNumberOrTag::Pending.into()))
					.with_context(|| format!("prepare supplyQueue({i}) batch call"))?;
				waiters.push(w);
			}

			batch2.send().await.context("send batch 2 (supplyQueue[*])")?;
			for (i, w) in waiters.into_iter().enumerate() {
				let bytes = w.await.with_context(|| format!("supplyQueue({i}) batch response"))?;
				let id = MetaMorpho::withdrawQueueCall::abi_decode_returns(bytes.as_ref())?;
				let id = B256::from(id);
				market_ids.push(id);
			}
		}

		// We'll aggregate the vault's total assets and the weighted sum of supply
		// APYs across all markets.  For each market we also record the vault's
		// current asset exposure so we can distribute any additional deposit.
		let mut total_assets_u256 = U256::from(0);
		let mut weighted_sum_wad = U256::from(0);
		let mut market_vault_assets: Vec<U256> = Vec::with_capacity(n_markets);
		let mut market_borrow_apy_wad: Vec<U256> = Vec::with_capacity(n_markets);
		let mut market_fee_wad: Vec<U256> = Vec::with_capacity(n_markets);
		let mut market_total_supply_assets: Vec<U256> = Vec::with_capacity(n_markets);
		let mut market_total_borrow_assets: Vec<U256> = Vec::with_capacity(n_markets);

		// Precompute the constant WAD (10^18) as a U256.  We'll reuse this
		// value in several calculations below.
		let one_wad = U256::from(10u128.pow(18));

		for id in &market_ids {
			// Fetch market parameters, market status and the vault's position in
			// that market.  We use synchronous RPC calls here because the
			// supply queue is typically short, and making further batches
			// complicates the logic for modest benefit.
			let params = morpho.idToMarketParams(*id).call().await.context("idToMarketParams")?;
			let market = morpho.market(*id).call().await.context("market")?;
			let pos = morpho.position(*id, vault_addr).call().await.context("position")?;
			let supply_shares = U256::from(pos.supplyShares);

			// Query the interest rate model for the borrow rate.  The borrow
			// rate is returned as a per‑second rate in WAD units.  We then
			// exponentiate it over a year to obtain the borrow APY in WAD.
			let borrow_apy_wad = if params.irm == Address::ZERO {
				U256::ZERO
			} else {
				let irm = IIrm::new(params.irm, self.provider.clone());
				let brate_wad = irm
					.borrowRateView(params.clone(), market.clone())
					.call()
					.await
					.context("borrowRateView")?;
				exp_wad(brate_wad, SECONDS_PER_YEAR as u128)?
			};

			let tsupp_assets = U256::from(market.totalSupplyAssets);
			let tbor_assets = U256::from(market.totalBorrowAssets);
			market_total_supply_assets.push(tsupp_assets);
			market_total_borrow_assets.push(tbor_assets);

			// Calculate current utilization as totalBorrowAssets / totalSupplyAssets.
			let utilization_wad = if tsupp_assets.is_zero() {
				U256::from(0)
			} else {
				tbor_assets
					.saturating_mul(one_wad)
					.checked_div(tsupp_assets)
					.unwrap_or(U256::from(0))
			};

			let market_fee = U256::from(market.fee);
			market_fee_wad.push(market_fee);
			let one_minus_fee = one_wad.saturating_sub(market_fee);

			// Compute the market's supply APY: borrow APY * utilization * (1 - fee).
			let supply_apy_wad = borrow_apy_wad
				.saturating_mul(utilization_wad)
				.checked_div(one_wad)
				.unwrap_or(U256::from(0))
				.saturating_mul(one_minus_fee)
				.checked_div(one_wad)
				.unwrap_or(U256::from(0));
			market_borrow_apy_wad.push(borrow_apy_wad);

			// Derive the vault's current asset exposure to this market.  The
			// position holds supplyShares; convert those to assets using the
			// market's totalSupplyAssets / totalSupplyShares ratio.
			let tsupp_shares = U256::from(market.totalSupplyShares).max(U256::from(1));
			let market_assets_for_vault = supply_shares
				.saturating_mul(tsupp_assets)
				.checked_div(tsupp_shares)
				.unwrap_or(U256::from(0));
			market_vault_assets.push(market_assets_for_vault);

			// Accumulate weighted APY for the existing vault assets.
			weighted_sum_wad = weighted_sum_wad.saturating_add(market_assets_for_vault.saturating_mul(supply_apy_wad));
			total_assets_u256 = total_assets_u256.saturating_add(market_assets_for_vault);
		}

		// If the vault currently holds no assets, return 0%.  This avoids
		// dividing by zero and handles empty vaults gracefully.
		if total_assets_u256.is_zero() {
			return Ok("0".to_string());
		}

		// If a deposit is specified, adjust each market's total supply assets
		// and the vault's exposure proportionally.  This approximation
		// distributes the deposit across markets in proportion to the vault's
		// existing exposure.  The deposit will increase total supply assets
		// and therefore reduce utilization and supply APY.  We update the
		// weighted_sum_wad and total_assets_u256 accordingly.
		if !deposit_assets.is_zero() {
			// Compute each market's weight in the vault.
			let mut new_weighted_sum = U256::from(0);
			for i in 0..market_ids.len() {
				let m_assets = market_vault_assets[i];
				// Proportion of vault exposure in this market.  Use checked
				// division to avoid division by zero (we already handled
				// total_assets_u256 == 0 above).
				let weight_wad = m_assets
					.saturating_mul(one_wad)
					.checked_div(total_assets_u256)
					.unwrap_or(U256::from(0));
				// Additional assets directed to this market.
				let add_assets = deposit_assets
					.saturating_mul(weight_wad)
					.checked_div(one_wad)
					.unwrap_or(U256::from(0));
				// New total supply assets for the underlying market.
				let tsupp_assets = market_total_supply_assets[i].saturating_add(add_assets);
				let tbor_assets = market_total_borrow_assets[i];
				// Recompute utilization after the deposit: borrow / supply.
				let new_util = if tsupp_assets.is_zero() {
					U256::from(0)
				} else {
					tbor_assets
						.saturating_mul(one_wad)
						.checked_div(tsupp_assets)
						.unwrap_or(U256::from(0))
				};
				// The borrow APY remains the same (borrow_apy_wad[i]) because the
				// interest rate is determined by the IRM given current supply
				// and borrow amounts.  Since the deposit only changes supply
				// assets, the borrow rate does not change in this simplified
				// approximation.
				let borrow_apy = market_borrow_apy_wad[i];
				// Market fee is constant.
				let m_fee = market_fee_wad[i];
				let one_minus_fee = one_wad.saturating_sub(m_fee);
				// Updated supply APY after deposit.
				let new_supply_apy_wad = borrow_apy
					.saturating_mul(new_util)
					.checked_div(one_wad)
					.unwrap_or(U256::from(0))
					.saturating_mul(one_minus_fee)
					.checked_div(one_wad)
					.unwrap_or(U256::from(0));
				// Vault's updated asset exposure for this market.
				let new_market_assets_for_vault = m_assets.saturating_add(add_assets);
				// Accumulate weighted APY using updated exposure and supply APY.
				new_weighted_sum =
					new_weighted_sum.saturating_add(new_market_assets_for_vault.saturating_mul(new_supply_apy_wad));
			}
			weighted_sum_wad = new_weighted_sum;
			total_assets_u256 = total_assets_u256.saturating_add(deposit_assets);
		}

		// Derive the weighted APY across all markets.  This is simply the sum
		// of (vault assets * supply APY) divided by total vault assets.
		let w_apy_wad = weighted_sum_wad.checked_div(total_assets_u256).unwrap_or(U256::from(0));
		// Apply the vault performance fee: net = weighted_apy * (1 - fee).
		let one_minus_vault_fee = one_wad.saturating_sub(U256::from(vault_fee_wad));
		let net_vault_apy_wad = w_apy_wad
			.saturating_mul(one_minus_vault_fee)
			.checked_div(one_wad)
			.unwrap_or(U256::from(0));
		// Format the result as a percentage string with two decimal places.
		wad_to_string(net_vault_apy_wad)
	}
}

/// Exponentiate a per‑second rate (in WAD) over a fixed number of seconds to
/// compute an annualized rate.  This function approximates `exp(rate * t) - 1`
/// using a third‑order Taylor expansion.  It is suitable for small rates and
/// avoids floating point arithmetic.
fn exp_wad(rate_per_sec_wad: U256, seconds: u128) -> Result<U256> {
	let x_wad = rate_per_sec_wad.saturating_mul(U256::from(seconds));
	let one = U256::from(10u128.pow(18));
	let x = x_wad;
	let x2 = x.saturating_mul(x).checked_div(one).unwrap_or(U256::from(0));
	let x3 = x2.saturating_mul(x).checked_div(one).unwrap_or(U256::from(0));
	// e^x ≈ 1 + x + x^2/2 + x^3/6
	let e_x = one
		.saturating_add(x)
		.saturating_add(x2.checked_div(U256::from(2)).unwrap_or(U256::from(0)))
		.saturating_add(x3.checked_div(U256::from(6)).unwrap_or(U256::from(0)));
	// Subtract 1 to obtain APY (exp(rate) - 1)
	Ok(e_x.saturating_sub(one))
}

fn wad_to_string(wad: U256) -> Result<String> {
	let num = wad.saturating_mul(U256::from(100u128));
	let int = num.checked_div(U256::from(10u128.pow(18))).unwrap_or(U256::from(0));
	let rem = num.checked_rem(U256::from(10u128.pow(18))).unwrap_or(U256::from(0));
	const PRECISION: u8 = 5;
	let int = int * U256::from(10u128.pow(PRECISION as u32));
	let rem = rem / U256::from(10u128.pow(18 - PRECISION as u32));
	let val = int + rem;
	Ok(val.to_string())
}

/// Convert a WAD fixed‑point number into a human‑readable percentage string.
/// The input is multiplied by 100 and truncated to two decimal places.
fn _wad_to_percent_string(wad: U256) -> Result<String> {
	// Multiply by 100 to convert to percent.
	let num = wad.saturating_mul(U256::from(100u128));
	let int = num.checked_div(U256::from(10u128.pow(18))).unwrap_or(U256::from(0));
	let rem = num.checked_rem(U256::from(10u128.pow(18))).unwrap_or(U256::from(0));
	// Extract two decimal digits.
	let dec = rem
		.saturating_mul(U256::from(100u128))
		.checked_div(U256::from(10u128.pow(18)))
		.unwrap_or(U256::from(0));
	Ok(format!("{}.{}%", int.to::<u128>(), dec.to::<u128>()))
}
