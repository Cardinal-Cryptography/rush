use crate::crypto::{PartialMultisignature, Signature};
use aleph_bft_types::{
    Index, KeyBox as KeyBoxT, MultiKeychain as MultiKeychainT, NodeCount, NodeIndex,
    PartialMultisignature as PartialMultisignatureT, SignatureSet,
};
use async_trait::async_trait;

#[derive(Clone, Debug)]
pub struct Keychain {
    count: NodeCount,
    index: NodeIndex,
}

impl Keychain {
    pub fn new(count: NodeCount, index: NodeIndex) -> Self {
        Keychain { count, index }
    }

    fn quorum(&self) -> usize {
        2 * self.count.0 / 3 + 1
    }
}

impl Index for Keychain {
    fn index(&self) -> NodeIndex {
        self.index
    }
}

#[async_trait]
impl KeyBoxT for Keychain {
    type Signature = Signature;

    fn node_count(&self) -> NodeCount {
        self.count
    }

    async fn sign(&self, msg: &[u8]) -> Self::Signature {
        Signature::new(msg.to_vec(), self.index)
    }

    fn verify(&self, msg: &[u8], sgn: &Self::Signature, index: NodeIndex) -> bool {
        index == sgn.index() && msg == sgn.msg()
    }
}

impl MultiKeychainT for Keychain {
    type PartialMultisignature = PartialMultisignature;

    fn from_signature(
        &self,
        signature: &Self::Signature,
        index: NodeIndex,
    ) -> Self::PartialMultisignature {
        SignatureSet::add_signature(SignatureSet::with_size(self.node_count()), signature, index)
    }

    fn is_complete(&self, msg: &[u8], partial: &Self::PartialMultisignature) -> bool {
        let signature_count = partial.iter().count();
        if signature_count < self.quorum() {
            return false;
        }
        partial.iter().all(|(i, sgn)| self.verify(msg, sgn, i))
    }
}

impl crate::crypto::wrappers::MK for Keychain {}
