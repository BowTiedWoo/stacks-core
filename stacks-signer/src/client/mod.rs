/// The stacker db module for communicating with the stackerdb contract
mod stackerdb;
/// The stacks node client module for communicating with the stacks node
mod stacks_client;

use std::time::Duration;

use clarity::vm::types::serialization::SerializationError;
use clarity::vm::Value as ClarityValue;
use libsigner::RPCError;
use libstackerdb::Error as StackerDBError;
use slog::slog_debug;
pub use stackerdb::*;
pub use stacks_client::*;
use stacks_common::debug;

/// Backoff timer initial interval in milliseconds
const BACKOFF_INITIAL_INTERVAL: u64 = 128;
/// Backoff timer max interval in milliseconds
const BACKOFF_MAX_INTERVAL: u64 = 16384;

/// Temporary placeholder for the number of slots allocated to a stacker-db writer. This will be retrieved from the stacker-db instance in the future
/// See: https://github.com/stacks-network/stacks-blockchain/issues/3921
/// Is equal to the number of message types
pub const SIGNER_SLOTS_PER_USER: u32 = 10;
/// The number of miner slots available per miner
pub const MINER_SLOTS_PER_USER: u32 = 1;

#[derive(thiserror::Error, Debug)]
/// Client error type
pub enum ClientError {
    /// An error occurred serializing the message
    #[error("Unable to serialize stacker-db message: {0}")]
    StackerDBSerializationError(#[from] bincode::Error),
    /// Failed to sign stacker-db chunk
    #[error("Failed to sign stacker-db chunk: {0}")]
    FailToSign(#[from] StackerDBError),
    /// Failed to write to stacker-db due to RPC error
    #[error("Failed to write to stacker-db instance: {0}")]
    PutChunkFailed(#[from] RPCError),
    /// Stacker-db instance rejected the chunk
    #[error("Stacker-db rejected the chunk. Reason: {0}")]
    PutChunkRejected(String),
    /// Failed to find a given json entry
    #[error("Invalid JSON entry: {0}")]
    InvalidJsonEntry(String),
    /// Failed to call a read only function
    #[error("Failed to call read only function. {0}")]
    ReadOnlyFailure(String),
    /// Reqwest specific error occurred
    #[error("{0}")]
    ReqwestError(#[from] reqwest::Error),
    /// Failed to build and sign a new Stacks transaction.
    #[error("Failed to generate transaction from a transaction signer: {0}")]
    TransactionGenerationFailure(String),
    /// Stacks node client request failed
    #[error("Stacks node client request failed: {0}")]
    RequestFailure(reqwest::StatusCode),
    /// Failed to serialize a Clarity value
    #[error("Failed to serialize Clarity value: {0}")]
    ClaritySerializationError(#[from] SerializationError),
    /// Failed to parse a Clarity value
    #[error("Recieved a malformed clarity value: {0}")]
    MalformedClarityValue(ClarityValue),
    /// Invalid Clarity Name
    #[error("Invalid Clarity Name: {0}")]
    InvalidClarityName(String),
    /// Backoff retry timeout
    #[error("Backoff retry timeout occurred. Stacks node may be down.")]
    RetryTimeout,
}

/// Retry a function F with an exponential backoff and notification on transient failure
pub fn retry_with_exponential_backoff<F, E, T>(request_fn: F) -> Result<T, ClientError>
where
    F: FnMut() -> Result<T, backoff::Error<E>>,
{
    let notify = |_err, dur| {
        debug!(
            "Failed to connect to stacks-node. Next attempt in {:?}",
            dur
        );
    };

    let backoff_timer = backoff::ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_millis(BACKOFF_INITIAL_INTERVAL))
        .with_max_interval(Duration::from_millis(BACKOFF_MAX_INTERVAL))
        .build();

    backoff::retry_notify(backoff_timer, request_fn, notify).map_err(|_| ClientError::RetryTimeout)
}