// Coding conventions
#![allow(dead_code)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::ptr_arg)]
#![allow(clippy::unit_arg)]
#![forbid(unsafe_code)]
#![deny(non_upper_case_globals)]
#![deny(non_camel_case_types)]
#![deny(non_snake_case)]
#![deny(unused_mut)]
#![deny(unused_imports)]
#![deny(clippy::wildcard_enum_match_arm)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]

#[macro_use]
extern crate lazy_static;

mod actions;
mod address_util;
mod api;
mod box_kind;
mod cli_commands;
mod contracts;
mod datapoint_source;
mod default_parameters;
mod logging;
mod node_interface;
mod oracle_config;
mod oracle_state;
mod pool_commands;
mod pool_config;
mod scans;
mod serde;
mod spec_token;
mod state;
mod templates;
#[cfg(test)]
mod tests;
mod wallet;

use actions::execute_action;
use actions::PoolAction;
use anyhow::anyhow;
use anyhow::Context;
use clap::{Parser, Subcommand};
use crossbeam::channel::bounded;
use ergo_lib::ergo_chain_types::Digest32;
use ergo_lib::ergotree_ir::chain::address::Address;
use ergo_lib::ergotree_ir::chain::address::AddressEncoder;
use ergo_lib::ergotree_ir::chain::address::NetworkAddress;
use ergo_lib::ergotree_ir::chain::address::NetworkPrefix;
use ergo_lib::ergotree_ir::chain::token::Token;
use ergo_lib::ergotree_ir::chain::token::TokenId;
use log::debug;
use log::error;
use log::LevelFilter;
use node_interface::assert_wallet_unlocked;
use node_interface::current_block_height;
use node_interface::get_wallet_status;
use node_interface::new_node_interface;
use oracle_config::ORACLE_CONFIG;
use oracle_state::register_and_save_scans;
use oracle_state::OraclePool;
use pool_commands::build_action;
use pool_commands::publish_datapoint::PublishDatapointActionError::DataPointSource;
use pool_commands::refresh::RefreshActionError;
use pool_commands::PoolCommandError;
use pool_config::POOL_CONFIG;
use state::process;
use state::PoolState;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::env;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;
use wallet::WalletData;

use crate::api::start_rest_server;
use crate::default_parameters::print_contract_hashes;
use crate::oracle_config::OracleConfig;
use crate::oracle_config::OracleConfigFileError;
use crate::oracle_config::MAYBE_ORACLE_CONFIG;
use crate::pool_config::MAYBE_POOL_CONFIG;

/// A Base58 encoded String of a Ergo P2PK address. Using this type def until sigma-rust matures further with the actual Address type.
pub type P2PKAddress = String;
/// A Base58 encoded String of a Ergo P2S address. Using this type def until sigma-rust matures further with the actual Address type.
pub type P2SAddress = String;
/// The smallest unit of the Erg currency.
pub type NanoErg = u64;
/// A block height of the chain.
pub type BlockHeight = u64;
/// Duration in number of blocks.
pub type BlockDuration = u64;
/// The epoch counter
pub type EpochID = u32;

const APP_VERSION: &str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("GIT_COMMIT_HASH"),
    " ",
    env!("GIT_COMMIT_DATE")
);

#[derive(Debug, Parser)]
#[clap(author, version = APP_VERSION, about, long_about = None)]
struct Args {
    #[clap(subcommand)]
    command: Command,
    /// Increase the logging verbosity
    #[clap(short, long)]
    verbose: bool,
    /// Set path of oracle configuration file to use. Default is ./oracle_config.yaml
    #[clap(long)]
    oracle_config_file: Option<String>,
    /// Set path of pool configuration file to use. Default is ./pool_config.yaml
    #[clap(long)]
    pool_config_file: Option<String>,
    /// Set folder path for the data files (scanIDs.json, logs). Default is the current folder.
    #[clap(short, long)]
    data_dir: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bootstrap a new oracle-pool or generate a bootstrap config template file using default
    /// contract scripts and parameters.
    Bootstrap {
        /// The name of the bootstrap config file.
        yaml_config_name: String,
        #[clap(short, long)]
        /// Set this flag to output a bootstrap config template file to the given filename. If
        /// filename already exists, return error.
        generate_config_template: bool,
    },

    /// Run the oracle-pool
    Run {
        /// Run in read-only mode
        #[clap(long)]
        read_only: bool,
        #[clap(long)]
        /// Set this flag to enable the REST API. NOTE: SSL is not used!
        enable_rest_api: bool,
    },

    /// Send reward tokens accumulated in the oracle box to a chosen address
    ExtractRewardTokens {
        /// Base58 encoded address to send reward tokens to
        rewards_address: String,
    },

    /// Print the number of reward tokens earned by the oracle (in the last posted/collected oracle box)
    PrintRewardTokens,

    /// Transfer an oracle token to a chosen address.
    TransferOracleToken {
        /// Base58 encoded address to send oracle token to
        oracle_token_address: String,
    },

    /// Vote to update the oracle pool
    VoteUpdatePool {
        /// The base16-encoded blake2b hash of the serialized pool box contract for the new pool box.
        new_pool_box_address_hash_str: String,
        /// The base16-encoded reward token id of the new pool box (use existing if unchanged)
        reward_token_id_str: String,
        /// The reward token amount in the pool box at the time of update transaction is committed.
        reward_token_amount: u32,
        /// The creation height of the existing update box.
        update_box_creation_height: u32,
    },
    /// Initiate the Update Pool transaction.
    /// Run with no arguments to show diff between oracle_config.yaml and oracle_config_updated.yaml
    /// Updated config file must be created using --prepare-update command first
    UpdatePool {
        /// New pool box hash. Must match hash of updated pool contract
        new_pool_box_hash: Option<String>,
        /// New reward token id (optional, base64)
        reward_token_id: Option<String>,
        /// New reward token amount, required if new token id was voted for
        reward_token_amount: Option<u64>,
    },
    /// Prepare updating oracle pool with new contracts/parameters.
    /// Creates new refresh box and pool box if needed (e.g. if new reward tokens are minted)
    PrepareUpdate {
        /// Name of the parameters file (.yaml) with new contract parameters
        update_file: String,
    },

    /// Print base 64 encodings of the blake2b hash of ergo-tree bytes of each contract
    PrintContractHashes,

    /// Print the current config file with zeroed sensitive/private fields.
    /// Intended to be shared with pool operators.
    PrintSafeConfig,
}

fn main() {
    let args = Args::parse();
    debug!("Args: {:?}", args);
    oracle_config::ORACLE_CONFIG_FILE_PATH
        .set(
            args.oracle_config_file
                .unwrap_or_else(|| oracle_config::DEFAULT_ORACLE_CONFIG_FILE_NAME.to_string()),
        )
        .unwrap();
    pool_config::POOL_CONFIG_FILE_PATH
        .set(
            args.pool_config_file
                .unwrap_or_else(|| pool_config::DEFAULT_POOL_CONFIG_FILE_NAME.to_string()),
        )
        .unwrap();

    if MAYBE_POOL_CONFIG.is_err() {
        // TODO: in case of IO error try to migrate old config file to new format
    }

    if let Err(OracleConfigFileError::IoError(_)) = MAYBE_ORACLE_CONFIG.clone() {
        let config = OracleConfig::default();

        let s = serde_yaml::to_string(&config).unwrap();
        let mut file = std::fs::File::create(&config_file_name).unwrap();
        file.write_all(s.as_bytes()).unwrap();
        println!("Error: oracle_config.yaml not found. Default config file is generated.");
        println!("Please, set the required parameters and run again");
        return;
    }

    let cmdline_log_level = if args.verbose {
        Some(LevelFilter::Debug)
    } else {
        None
    };

    let data_dir_path = if let Some(data_dir) = args.data_dir {
        Path::new(&data_dir).to_path_buf()
    } else {
        env::current_dir().unwrap()
    };
    logging::setup_log(cmdline_log_level, &data_dir_path);
    scans::SCANS_DIR_PATH.set(data_dir_path).unwrap();

    let mut tokio_runtime = tokio::runtime::Runtime::new().unwrap();

    #[allow(clippy::wildcard_enum_match_arm)]
    match args.command {
        Command::Bootstrap {
            yaml_config_name,
            generate_config_template,
        } => {
            if let Err(e) = (|| -> Result<(), anyhow::Error> {
                if generate_config_template {
                    cli_commands::bootstrap::generate_bootstrap_config_template(yaml_config_name)?;
                } else {
                    cli_commands::bootstrap::bootstrap(yaml_config_name)?;
                }
                Ok(())
            })() {
                {
                    error!("Fatal advanced-bootstrap error: {:?}", e);
                    std::process::exit(exitcode::SOFTWARE);
                }
            };
        }
        Command::PrintContractHashes => {
            print_contract_hashes();
        }
        Command::PrintSafeConfig => cli_commands::print_conf::print_safe_config(&ORACLE_CONFIG),
        oracle_command => handle_oracle_command(oracle_command, &mut tokio_runtime),
    }
}

/// Handle all non-bootstrap commands that require ORACLE_CONFIG/OraclePool
fn handle_oracle_command(command: Command, tokio_runtime: &mut tokio::runtime::Runtime) {
    log_on_launch();
    assert_wallet_unlocked(&new_node_interface());
    register_and_save_scans().unwrap();
    let op = OraclePool::new().unwrap();
    match command {
        Command::Run {
            read_only,
            enable_rest_api,
        } => {
            assert_wallet_unlocked(&new_node_interface());
            let (_, repost_receiver) = bounded::<bool>(1);

            // Start Oracle Core GET API Server
            if enable_rest_api {
                tokio_runtime.spawn(start_rest_server(repost_receiver));
            }
            loop {
                if let Err(e) = main_loop_iteration(&op, read_only) {
                    error!("error: {:?}", e);
                }
                // Delay loop restart
                thread::sleep(Duration::new(30, 0));
            }
        }

        Command::ExtractRewardTokens { rewards_address } => {
            let wallet = WalletData {};
            if let Err(e) = cli_commands::extract_reward_tokens::extract_reward_tokens(
                &wallet,
                op.get_local_datapoint_box_source(),
                rewards_address,
            ) {
                error!("Fatal extract-rewards-token error: {:?}", e);
                std::process::exit(exitcode::SOFTWARE);
            }
        }

        Command::PrintRewardTokens => {
            if let Err(e) = cli_commands::print_reward_tokens::print_reward_tokens(
                op.get_local_datapoint_box_source(),
            ) {
                error!("Fatal print-rewards-token error: {:?}", e);
                std::process::exit(exitcode::SOFTWARE);
            }
        }

        Command::TransferOracleToken {
            oracle_token_address,
        } => {
            let wallet = WalletData {};
            if let Err(e) = cli_commands::transfer_oracle_token::transfer_oracle_token(
                &wallet,
                op.get_local_datapoint_box_source(),
                oracle_token_address,
            ) {
                error!("Fatal transfer-oracle-token error: {:?}", e);
                std::process::exit(exitcode::SOFTWARE);
            }
        }

        Command::VoteUpdatePool {
            new_pool_box_address_hash_str,
            reward_token_id_str,
            reward_token_amount,
            update_box_creation_height,
        } => {
            let wallet = WalletData {};
            if let Err(e) = cli_commands::vote_update_pool::vote_update_pool(
                &wallet,
                op.get_local_ballot_box_source(),
                new_pool_box_address_hash_str,
                reward_token_id_str,
                reward_token_amount,
                update_box_creation_height,
            ) {
                error!("Fatal vote-update-pool error: {:?}", e);
                std::process::exit(exitcode::SOFTWARE);
            }
        }
        Command::UpdatePool {
            new_pool_box_hash,
            reward_token_id,
            reward_token_amount,
        } => {
            let new_reward_tokens =
                reward_token_id
                    .zip(reward_token_amount)
                    .map(|(token_id, amount)| Token {
                        token_id: TokenId::from(Digest32::try_from(token_id).unwrap()),
                        amount: amount.try_into().unwrap(),
                    });
            if let Err(e) =
                cli_commands::update_pool::update_pool(&op, new_pool_box_hash, new_reward_tokens)
            {
                error!("Fatal update-pool error: {}", e);
                std::process::exit(exitcode::SOFTWARE);
            }
        }
        Command::PrepareUpdate { update_file } => {
            if let Err(e) = cli_commands::prepare_update::prepare_update(update_file) {
                error!("Fatal update error : {}", e);
                std::process::exit(exitcode::SOFTWARE);
            }
        }
        Command::Bootstrap { .. } | Command::PrintContractHashes => unreachable!(),
        Command::PrintSafeConfig => unreachable!(),
    }
}

fn main_loop_iteration(op: &OraclePool, read_only: bool) -> std::result::Result<(), anyhow::Error> {
    let height = current_block_height().context("Failed to get the current height")? as u32;
    let wallet = WalletData::new();
    let network_change_address = get_change_address_from_node()?;
    let pool_state = match op.get_live_epoch_state() {
        Ok(live_epoch_state) => PoolState::LiveEpoch(live_epoch_state),
        Err(error) => {
            log::debug!("error getting live epoch state: {}", error);
            PoolState::NeedsBootstrap
        }
    };
    let epoch_length = POOL_CONFIG
        .refresh_box_wrapper_inputs
        .contract_inputs
        .contract_parameters()
        .epoch_length() as u32;
    if let Some(cmd) = process(pool_state, epoch_length, height) {
        log::debug!("Height {height}. Building action for command: {:?}", cmd);
        let build_action_res =
            build_action(cmd, op, &wallet, height, network_change_address.address());
        if let Some(action) =
            log_and_continue_if_non_fatal(network_change_address.network(), build_action_res)?
        {
            if !read_only {
                execute_action(action)?;
            }
        };
    }
    Ok(())
}

fn log_and_continue_if_non_fatal(
    network_prefix: NetworkPrefix,
    res: Result<PoolAction, PoolCommandError>,
) -> Result<Option<PoolAction>, PoolCommandError> {
    match res {
        Ok(action) => Ok(Some(action)),
        Err(PoolCommandError::RefreshActionError(RefreshActionError::FailedToReachConsensus {
            expected,
            found_public_keys,
            found_num,
        })) => {
            let found_oracle_addresses: String = found_public_keys
                .into_iter()
                .map(|pk| NetworkAddress::new(network_prefix, &Address::P2Pk(pk)).to_base58())
                .collect::<Vec<String>>()
                .join(", ");
            log::error!("Refresh failed, not enough datapoints. The minimum number of datapoints within the deviation range: required minumum {expected}, found {found_num} from addresses {found_oracle_addresses},");
            Ok(None)
        }
        Err(PoolCommandError::PublishDatapointActionError(DataPointSource(e))) => {
            log::error!("Failed to get datapoint with error: {}", e);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn get_change_address_from_node() -> Result<NetworkAddress, anyhow::Error> {
    let change_address_str = get_wallet_status()?
        .change_address
        .ok_or_else(|| anyhow!("failed to get wallet's change address (locked wallet?)"))?;
    let addr = AddressEncoder::unchecked_parse_network_address_from_str(&change_address_str)?;
    Ok(addr)
}

fn log_on_launch() {
    log::info!("{}", APP_VERSION);
    if let Ok(config) = MAYBE_ORACLE_CONFIG.clone() {
        // log::info!("Token ids: {:?}", config.token_ids);
        log::info!("Oracle address: {}", config.oracle_address.to_base58());
    }
}
