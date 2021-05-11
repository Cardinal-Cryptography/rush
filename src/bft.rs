use crate::{units::UncheckedSignedUnit, Data, Hasher, NodeIndex};
use codec::{Decode, Encode};

#[derive(Debug, Encode, Decode)]
pub(crate) struct ForkProof<H: Hasher, D: Data, Signature: Clone + Encode + Decode> {
    pub(crate) u1: UncheckedSignedUnit<H, D, Signature>,
    pub(crate) u2: UncheckedSignedUnit<H, D, Signature>,
}

#[derive(Debug, Encode, Decode)]
pub(crate) struct Alert<H: Hasher, D: Data, Signature: Clone + Encode + Decode> {
    pub(crate) sender: NodeIndex,
    pub(crate) forker: NodeIndex,
    pub(crate) proof: ForkProof<H, D, Signature>,
    pub(crate) legit_units: Vec<UncheckedSignedUnit<H, D, Signature>>,
}
