mod crypto;
mod dataio;
mod hasher;
mod network;
mod router;
mod signable;
mod spawner;

pub use crypto::{
    BadSignatureWrapper, Keychain, PartialMultisignature, Signature, ThresholdMultiWrapper,
    YesManWrapper,
};
pub use dataio::{Data, DataProvider, FinalizationHandler};
pub use hasher::{Hash64, Hasher64};
pub use network::{Network, NetworkReceiver, NetworkSender};
pub use router::{NetworkHook, Peer, Router};
pub use signable::{Signable, SignableByte};
pub use spawner::Spawner;

// ugly renames
use aleph_bft_types::{NodeCount, NodeIndex};
pub type KeyBox = ThresholdMultiWrapper<YesManWrapper<Keychain>>;
impl KeyBox {
    pub fn new(count: NodeCount, index: NodeIndex) -> Self {
        YesManWrapper::from(Keychain::new(count, index)).into()
    }
}
