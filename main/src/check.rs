// Copyright 2023-2024, Offchain Labs, Inc.
// For licensing, see https://github.com/OffchainLabs/cargo-stylus/blob/main/licenses/COPYRIGHT.md

use crate::{
    check::ArbWasm::ArbWasmErrors,
    constants::{ARB_WASM_H160, ONE_ETH},
    macros::*,
    project,
    util::{
        color::{Color, GREY, LAVENDER, MINT, PINK, YELLOW},
        sys, text,
    },
    CheckConfig, DataFeeOpts,
};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_macro::sol;
use alloy_sol_types::{SolCall, SolInterface};
use bytesize::ByteSize;
use ethers::{
    core::types::spoof,
    prelude::*,
    providers::RawCall,
    types::{spoof::State, transaction::eip2718::TypedTransaction, Eip1559TransactionRequest},
};
use eyre::{bail, eyre, ErrReport, Result, WrapErr};
use serde_json::Value;
use std::path::PathBuf;

sol! {
    interface ArbWasm {
        function activateProgram(address program)
            external
            payable
            returns (uint16 version, uint256 dataFee);

        function stylusVersion() external view returns (uint16 version);

        function codehashVersion(bytes32 codehash) external view returns (uint16 version);

        error ProgramNotWasm();
        error ProgramNotActivated();
        error ProgramNeedsUpgrade(uint16 version, uint16 stylusVersion);
        error ProgramExpired(uint64 ageInSeconds);
        error ProgramUpToDate();
        error ProgramKeepaliveTooSoon(uint64 ageInSeconds);
        error ProgramInsufficientValue(uint256 have, uint256 want);
    }
}

/// Checks that a contract is valid and can be deployed onchain.
/// Returns whether the WASM is already up-to-date and activated onchain, and the data fee.
pub async fn check(cfg: &CheckConfig) -> Result<ContractCheck> {
    if cfg.common_cfg.endpoint == "https://stylus-testnet.arbitrum.io/rpc" {
        let version = "cargo stylus version 0.2.1".to_string().red();
        bail!("The old Stylus testnet is no longer supported.\nPlease downgrade to {version}",);
    }

    let verbose = cfg.common_cfg.verbose;
    let (wasm, project_hash) = cfg.build_wasm().wrap_err("failed to build wasm")?;

    if verbose {
        greyln!("reading wasm file at {}", wasm.to_string_lossy().lavender());
    }

    let (wasm_file_bytes, code) =
        project::compress_wasm(&wasm, project_hash).wrap_err("failed to compress WASM")?;

    greyln!("contract size: {}", format_file_size(code.len(), 16, 24));

    if verbose {
        greyln!(
            "wasm size: {}",
            format_file_size(wasm_file_bytes.len(), 96, 128)
        );
        greyln!("connecting to RPC: {}", &cfg.common_cfg.endpoint.lavender());
    }

    // Check if the contract already exists.
    let provider = sys::new_provider(&cfg.common_cfg.endpoint)?;
    let codehash = alloy_primitives::keccak256(&code);

    if contract_exists(codehash, &provider).await? {
        return Ok(ContractCheck::Active { code });
    }

    let address = cfg.contract_address.unwrap_or(H160::random());
    let fee = check_activate(code.clone().into(), address, &cfg.data_fee, &provider).await?;
    Ok(ContractCheck::Ready { code, fee })
}

/// Whether a contract is active, or needs activation.
#[derive(PartialEq)]
pub enum ContractCheck {
    /// Contract already exists onchain.
    Active { code: Vec<u8> },
    /// Contract can be activated with the given data fee.
    Ready { code: Vec<u8>, fee: U256 },
}

impl ContractCheck {
    pub fn code(&self) -> &[u8] {
        match self {
            Self::Active { code, .. } => code,
            Self::Ready { code, .. } => code,
        }
    }
    pub fn suggest_fee(&self) -> U256 {
        match self {
            Self::Active { .. } => U256::default(),
            Self::Ready { fee, .. } => *fee,
        }
    }
}

impl CheckConfig {
    fn build_wasm(&self) -> Result<(PathBuf, [u8; 32])> {
        if let Some(wasm) = self.wasm_file.clone() {
            return Ok((wasm, [0u8; 32]));
        }
        project::build_wasm_from_features(
            self.common_cfg.features.clone(),
            self.common_cfg.source_files_for_project_hash.clone(),
        )
    }
}

/// Pretty-prints a file size based on its limits.
pub fn format_file_size(len: usize, mid: u64, max: u64) -> String {
    let len = ByteSize::b(len as u64);
    let mid = ByteSize::kib(mid);
    let max = ByteSize::kib(max);
    let color = if len <= mid {
        MINT
    } else if len <= max {
        YELLOW
    } else {
        PINK
    };
    format!("{color}{}{GREY} ({} bytes)", len, len.as_u64())
}

/// Pretty-prints a data fee.
fn format_data_fee(fee: U256) -> String {
    let Ok(fee): Result<u64, _> = (fee / U256::from(1e9)).try_into() else {
        return ("???").red();
    };
    let fee: f64 = fee as f64 / 1e9;
    let text = format!("{fee:.6} ETH");
    if fee <= 5e14 {
        text.mint()
    } else if fee <= 5e15 {
        text.yellow()
    } else {
        text.pink()
    }
}

pub struct EthCallError {
    pub data: Vec<u8>,
    pub msg: String,
}

impl From<EthCallError> for ErrReport {
    fn from(value: EthCallError) -> Self {
        eyre!(value.msg)
    }
}

/// A funded eth_call.
pub async fn eth_call(
    tx: Eip1559TransactionRequest,
    mut state: State,
    provider: &Provider<Http>,
) -> Result<Result<Vec<u8>, EthCallError>> {
    let tx = TypedTransaction::Eip1559(tx);
    state.account(Default::default()).balance = Some(ethers::types::U256::MAX); // infinite balance

    match provider.call_raw(&tx).state(&state).await {
        Ok(bytes) => Ok(Ok(bytes.to_vec())),
        Err(ProviderError::JsonRpcClientError(error)) => {
            let error = error
                .as_error_response()
                .ok_or_else(|| eyre!("json RPC failure: {error}"))?;

            let msg = error.message.clone();
            let data = match &error.data {
                Some(Value::String(data)) => text::decode0x(data)?.to_vec(),
                Some(value) => bail!("failed to decode RPC failure: {value}"),
                None => vec![],
            };
            Ok(Err(EthCallError { data, msg }))
        }
        Err(error) => Err(error.into()),
    }
}

/// Checks whether a contract has already been activated with the most recent version of Stylus.
async fn contract_exists(codehash: B256, provider: &Provider<Http>) -> Result<bool> {
    let data = ArbWasm::codehashVersionCall { codehash }.abi_encode();
    let tx = Eip1559TransactionRequest::new()
        .to(*ARB_WASM_H160)
        .data(data);
    let outs = eth_call(tx, State::default(), provider).await?;

    let program_version = match outs {
        Ok(outs) => {
            if outs.is_empty() {
                bail!(
                    r#"No data returned from the ArbWasm precompile when checking if your Stylus contract exists.
Perhaps the Arbitrum node for the endpoint you are connecting to has not yet upgraded to Stylus"#
                );
            }
            let ArbWasm::codehashVersionReturn { version } =
                ArbWasm::codehashVersionCall::abi_decode_returns(&outs, true)?;
            version
        }
        Err(EthCallError { data, msg }) => {
            let Ok(error) = ArbWasmErrors::abi_decode(&data, true) else {
                bail!("unknown ArbWasm error: {msg}");
            };
            use ArbWasmErrors as A;
            match error {
                A::ProgramNotWasm(_) => bail!("not a Stylus contract"),
                A::ProgramNotActivated(_) | A::ProgramNeedsUpgrade(_) | A::ProgramExpired(_) => {
                    return Ok(false);
                }
                _ => bail!("unexpected ArbWasm error: {msg}"),
            }
        }
    };

    let data = ArbWasm::stylusVersionCall {}.abi_encode();
    let tx = Eip1559TransactionRequest::new()
        .to(*ARB_WASM_H160)
        .data(data);
    let outs = eth_call(tx, State::default(), provider).await??;
    let ArbWasm::stylusVersionReturn { version } =
        ArbWasm::stylusVersionCall::abi_decode_returns(&outs, true)?;

    Ok(program_version == version)
}

/// Checks contract activation, returning the data fee.
pub async fn check_activate(
    code: Bytes,
    address: H160,
    opts: &DataFeeOpts,
    provider: &Provider<Http>,
) -> Result<U256> {
    let contract = Address::from(address.to_fixed_bytes());
    let data = ArbWasm::activateProgramCall { program: contract }.abi_encode();
    let tx = Eip1559TransactionRequest::new()
        .to(*ARB_WASM_H160)
        .data(data)
        .value(ONE_ETH);
    let state = spoof::code(address, code);
    let eth_call_result = eth_call(tx, state, provider).await?;
    let outs = match eth_call_result {
        Ok(outs) => outs,
        Err(e) => {
            if e.msg.contains("pay_for_memory_grow") {
                let msg = "Contract could not be activated as it is missing an entrypoint. \
                Please ensure that your contract has an #[entrypoint] defined on your main struct";
                bail!(msg);
            } else {
                return Err(e.into());
            }
        }
    };
    let ArbWasm::activateProgramReturn {
        dataFee: data_fee, ..
    } = ArbWasm::activateProgramCall::abi_decode_returns(&outs, true)?;

    let bump = opts.data_fee_bump_percent;
    let adjusted_data_fee = data_fee * U256::from(100 + bump) / U256::from(100);
    greyln!(
        "wasm data fee: {} {GREY}(originally {}{GREY} with {LAVENDER}{bump}%{GREY} bump)",
        format_data_fee(adjusted_data_fee),
        format_data_fee(data_fee)
    );

    Ok(adjusted_data_fee)
}
