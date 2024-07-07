use alloy::{
    hex::FromHexError,
    network::EthereumWallet,
    primitives::Address,
    signers::local::{LocalSignerError, PrivateKeySigner},
};
use clap::{arg, command, Parser, Subcommand};
use ds::ds::DeliveryServiceError;

use std::io::{Read, Write};
use std::{str::FromStr, string::FromUtf8Error};
use url::Url;

use crate::user::UserError;

/// A client for de-MLS PoC
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Address of the Ethereum wallet
    #[arg(short = 'W', long)]
    user_wallet: String,

    /// User private key that correspond to Etherium wallet
    #[arg(short = 'K', long)]
    user_priv_key: String,

    /// Rpc url
    #[arg(short = 'U', long,
        default_value_t = Url::from_str("http://localhost:8545").unwrap())]
    pub storage_url: Url,

    /// Storage etherium address
    #[arg(short = 'S', long)]
    pub storage_addr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("Unable to parce the address: {0}")]
    AlloyFromHexError(#[from] FromHexError),
    #[error("Unable to parce the signer: {0}")]
    AlloyParceSignerError(#[from] LocalSignerError),
    #[error(transparent)]
    UserError(#[from] UserError),
    #[error(transparent)]
    ClapError(#[from] clap::error::Error),
    #[error("Write to stdout error")]
    IoError(#[from] std::io::Error),
    #[error("Can't split line")]
    SplitLineError,
    #[error("Unknown message type")]
    UnknownMsgError,
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("Parse String UTF8 error: {0}")]
    ParseUTF8Error(#[from] FromUtf8Error),
    #[error("Delivery Service error: {0}")]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error("Unknown error: {0}")]
    AnyHowError(anyhow::Error),
}

pub fn get_user_data(args: &Args) -> Result<(Address, EthereumWallet, Address), CliError> {
    let user_address = Address::from_str(&args.user_wallet)?;
    let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
    let wallet = EthereumWallet::from(signer);
    let storage_address = Address::from_str(&args.storage_addr)?;
    Ok((user_address, wallet, storage_address))
}

#[derive(Debug, Parser)]
#[command(multicall = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    CreateGroup {
        group_name: String,
    },
    Invite {
        group_name: String,
        user_wallet: String,
    },
    JoinGroup {
        welcome: String,
    },
    SendMessage {
        group_name: String,
        msg: String,
    },
    // RemoveUser { user_wallet: String },
    Exit,
}

pub fn readline() -> Result<String, CliError> {
    write!(std::io::stdout(), "$ ")?;
    std::io::stdout().flush()?;
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer)
}
