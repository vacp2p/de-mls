pub mod cli;
pub mod contact;
pub mod conversation;
pub mod identity;
pub mod user;

use std::{fs::File, io};

const SC_ADDRESS: &str = "contracts/broadcast/Deploy.s.sol/31337/run-latest.json";

pub fn get_contract_address() -> Result<String, HelperError> {
    let file = File::open(SC_ADDRESS)?;
    let json: serde_json::Value = serde_json::from_reader(file)?;

    let returns = match json.get("returns") {
        Some(v) => v,
        None => return Err(HelperError::UnknownJsonFieldError),
    };

    let sc_keystore = match returns.get("scKeystore") {
        Some(v) => v,
        None => return Err(HelperError::UnknownJsonFieldError),
    };

    let address = match sc_keystore.get("value") {
        Some(v) => v,
        None => return Err(HelperError::UnknownJsonFieldError),
    };

    match address.as_str() {
        Some(res) => Ok(res.to_string()),
        None => Err(HelperError::ParserError),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("Field doesn't exist")]
    UnknownJsonFieldError,
    #[error("Parser Error")]
    ParserError,
    #[error("Can't read file: {0}")]
    IoError(#[from] io::Error),
    #[error("Json Error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
