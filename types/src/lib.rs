use codec::Codec;
use std::{fmt::Debug, hash::Hash as StdHash};

mod dataio;
mod network;
mod nodes;
mod signed;
mod tasks;

pub use dataio::{
    DataProvider,
    FinalizationHandler,
};

pub use network::{
    Recipient,
    Network,
};

pub use nodes::{
    NodeCount,
    NodeIndex,
    NodeMap,
    NodeSubset,
};

/// Indicates that an implementor has been assigned some index.
pub trait Index {
    fn index(&self) -> NodeIndex;
}

pub use signed::{
    KeyBox,
    MultiKeychain,
    PartialMultisignature,
    Signable,
    Signature,
    SignatureSet,
};

pub use tasks::{
    TaskHandle,
    SpawnHandle,
};

/// A hasher, used for creating identifiers for blocks or units.
pub trait Hasher: Eq + Clone + Send + Sync + Debug + 'static {
    /// A hash, as an identifier for a block or unit.
    type Hash: AsRef<[u8]>
        + Eq
        + Ord
        + Copy
        + Clone
        + Send
        + Sync
        + Debug
        + StdHash
        + Codec;

    fn hash(s: &[u8]) -> Self::Hash;
}

/// The number of a session for which the consensus is run.
pub type SessionId = u64;

/// An asynchronous round of the protocol.
pub type Round = u16;
