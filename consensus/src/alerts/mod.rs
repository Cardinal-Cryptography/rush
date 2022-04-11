use crate::{
    units::UncheckedSignedUnit, Data, Hasher, Index, MultiKeychain, Multisigned, NodeCount,
    NodeIndex, PartialMultisignature, Receiver, Recipient, Sender, SessionId, Signable, Signature,
    Signed, UncheckedSigned,
};
use aleph_bft_rmc::{DoublingDelayScheduler, Message as RmcMessage, ReliableMulticast};
use codec::{Decode, Encode};
use derivative::Derivative;
use futures::{
    channel::{mpsc, oneshot},
    FutureExt, StreamExt,
};
use log::{debug, error, info, trace, warn};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    time,
};

mod io;

pub(crate) type ForkProof<H, D, S> = (UncheckedSignedUnit<H, D, S>, UncheckedSignedUnit<H, D, S>);

#[derive(Debug, Decode, Encode, Derivative)]
#[derivative(PartialEq, Eq, Hash)]
pub struct Alert<H: Hasher, D: Data, S: Signature> {
    sender: NodeIndex,
    proof: ForkProof<H, D, S>,
    legit_units: Vec<UncheckedSignedUnit<H, D, S>>,
    #[codec(skip)]
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    hash: RwLock<Option<H::Hash>>,
}

impl<H: Hasher, D: Data, S: Signature> Clone for Alert<H, D, S> {
    fn clone(&self) -> Self {
        let hash = match self.hash.try_read() {
            None => None,
            Some(guard) => *guard.deref(),
        };
        Alert {
            sender: self.sender,
            proof: self.proof.clone(),
            legit_units: self.legit_units.clone(),
            hash: RwLock::new(hash),
        }
    }
}

impl<H: Hasher, D: Data, S: Signature> Alert<H, D, S> {
    pub fn new(
        sender: NodeIndex,
        proof: ForkProof<H, D, S>,
        legit_units: Vec<UncheckedSignedUnit<H, D, S>>,
    ) -> Alert<H, D, S> {
        Alert {
            sender,
            proof,
            legit_units,
            hash: RwLock::new(None),
        }
    }
    fn hash(&self) -> H::Hash {
        let hash = *self.hash.read();
        match hash {
            Some(hash) => hash,
            None => {
                let hash = self.using_encoded(H::hash);
                *self.hash.write() = Some(hash);
                hash
            }
        }
    }

    // Simplified forker check, should only be called for alerts that have already been checked to
    // contain valid proofs.
    fn forker(&self) -> NodeIndex {
        self.proof.0.as_signable().creator()
    }

    pub(crate) fn included_data(&self) -> Vec<D> {
        // Only legit units might end up in the DAG, we can ignore the fork proof.
        self.legit_units
            .iter()
            .map(|uu| uu.as_signable().data().clone())
            .collect()
    }
}

impl<H: Hasher, D: Data, S: Signature> Index for Alert<H, D, S> {
    fn index(&self) -> NodeIndex {
        self.sender
    }
}

impl<H: Hasher, D: Data, S: Signature> Signable for Alert<H, D, S> {
    type Hash = H::Hash;
    fn hash(&self) -> Self::Hash {
        self.hash()
    }
}

/// A message concerning alerts.
#[derive(Debug, Encode, Decode, Clone, PartialEq, Eq, Hash)]
pub enum AlertMessage<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> {
    /// Alert regarding forks, signed by the person claiming misconduct.
    ForkAlert(UncheckedSigned<Alert<H, D, S>, S>),
    /// An internal RMC message, together with the id of the sender.
    RmcMessage(NodeIndex, RmcMessage<H::Hash, S, MS>),
    /// A request by a node for a fork alert identified by the given hash.
    AlertRequest(NodeIndex, H::Hash),
}

impl<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> AlertMessage<H, D, S, MS> {
    pub(crate) fn included_data(&self) -> Vec<D> {
        match self {
            Self::ForkAlert(unchecked_alert) => unchecked_alert.as_signable().included_data(),
            Self::RmcMessage(_, _) => Vec::new(),
            Self::AlertRequest(_, _) => Vec::new(),
        }
    }
}

/// A response to an [`AlertMessage`], generated by the alerter.
///
/// Certain calls to the alerter may generate responses. It is the caller's responsibility to
/// forward them appropriately.
#[derive(Debug, PartialEq, Eq)]
enum AlerterResponse<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> {
    ForkAlert(UncheckedSigned<Alert<H, D, S>, S>, Recipient),
    ForkResponse(Option<ForkingNotification<H, D, S>>, H::Hash),
    AlertRequest(H::Hash, Recipient),
    RmcMessage(RmcMessage<H::Hash, S, MS>),
}

// Notifications being sent to consensus, so that it can learn about proven forkers and receive
// legitimized units.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum ForkingNotification<H: Hasher, D: Data, S: Signature> {
    Forker(ForkProof<H, D, S>),
    Units(Vec<UncheckedSignedUnit<H, D, S>>),
}

/// The component responsible for fork alerts in AlephBFT. We refer to the documentation
/// https://cardinal-cryptography.github.io/AlephBFT/how_alephbft_does_it.html Section 2.5 and
/// https://cardinal-cryptography.github.io/AlephBFT/reliable_broadcast.html and to the Aleph
/// paper https://arxiv.org/abs/1908.05156 Appendix A1 for a discussion.
struct Alerter<'a, H: Hasher, D: Data, MK: MultiKeychain> {
    session_id: SessionId,
    keychain: &'a MK,
    known_forkers: HashMap<NodeIndex, ForkProof<H, D, MK::Signature>>,
    known_alerts: HashMap<H::Hash, Signed<'a, Alert<H, D, MK::Signature>, MK>>,
    known_rmcs: HashMap<(NodeIndex, NodeIndex), H::Hash>,
    exiting: bool,
}

pub(crate) struct AlertConfig {
    pub n_members: NodeCount,
    pub session_id: SessionId,
}

impl<'a, H: Hasher, D: Data, MK: MultiKeychain> Alerter<'a, H, D, MK> {
    fn new(keychain: &'a MK, config: AlertConfig) -> Self {
        Self {
            session_id: config.session_id,
            keychain,
            known_forkers: HashMap::new(),
            known_alerts: HashMap::new(),
            known_rmcs: HashMap::new(),
            exiting: false,
        }
    }

    fn index(&self) -> NodeIndex {
        self.keychain.index()
    }

    fn is_forker(&self, forker: NodeIndex) -> bool {
        self.known_forkers.contains_key(&forker)
    }

    fn on_new_forker_detected(&mut self, forker: NodeIndex, proof: ForkProof<H, D, MK::Signature>) {
        self.known_forkers.insert(forker, proof);
    }

    // Correctness rules:
    // 1) All units must be created by forker
    // 2) All units must come from different rounds
    // 3) There must be fewer of them than the maximum defined in the configuration.
    // Note that these units will have to be validated before being used in the consensus.
    // This is alright, if someone uses their alert to commit to incorrect units it's their own
    // problem.
    fn correct_commitment(
        &self,
        forker: NodeIndex,
        units: &[UncheckedSignedUnit<H, D, MK::Signature>],
    ) -> bool {
        let mut rounds = HashSet::new();
        for u in units {
            let u = match u.clone().check(self.keychain) {
                Ok(u) => u,
                Err(_) => {
                    warn!(target: "AlephBFT-alerter", "{:?} One of the units is incorrectly signed.", self.index());
                    return false;
                }
            };
            let full_unit = u.as_signable();
            if full_unit.creator() != forker {
                warn!(target: "AlephBFT-alerter", "{:?} One of the units {:?} has wrong creator.", self.index(), full_unit);
                return false;
            }
            if rounds.contains(&full_unit.round()) {
                warn!(target: "AlephBFT-alerter", "{:?} Two or more alerted units have the same round {:?}.", self.index(), full_unit.round());
                return false;
            }
            rounds.insert(full_unit.round());
        }
        true
    }

    fn who_is_forking(&self, proof: &ForkProof<H, D, MK::Signature>) -> Option<NodeIndex> {
        let (u1, u2) = proof;
        let (u1, u2) = {
            let u1 = u1.clone().check(self.keychain);
            let u2 = u2.clone().check(self.keychain);
            match (u1, u2) {
                (Ok(u1), Ok(u2)) => (u1, u2),
                _ => {
                    warn!(target: "AlephBFT-alerter", "{:?} Invalid signatures in a proof.", self.index());
                    return None;
                }
            }
        };
        let full_unit1 = u1.as_signable();
        let full_unit2 = u2.as_signable();
        if full_unit1.session_id() != self.session_id || full_unit2.session_id() != self.session_id
        {
            warn!(target: "AlephBFT-alerter", "{:?} Alert from different session.", self.index());
            return None;
        }
        if full_unit1 == full_unit2 {
            warn!(target: "AlephBFT-alerter", "{:?} Two copies of the same unit do not constitute a fork.", self.index());
            return None;
        }
        if full_unit1.creator() != full_unit2.creator() {
            warn!(target: "AlephBFT-alerter", "{:?} One of the units creators in proof does not match.", self.index());
            return None;
        }
        if full_unit1.round() != full_unit2.round() {
            warn!(target: "AlephBFT-alerter", "{:?} The rounds in proof's units do not match.", self.index());
            return None;
        }
        Some(full_unit1.creator())
    }

    #[must_use = "`rmc_alert()` registers the RMC but does not actually send it; the returned hash must be passed to `start_rmc()` separately"]
    async fn rmc_alert(
        &mut self,
        forker: NodeIndex,
        alert: Signed<'a, Alert<H, D, MK::Signature>, MK>,
    ) -> H::Hash {
        let hash = alert.as_signable().hash();
        self.known_rmcs
            .insert((alert.as_signable().sender, forker), hash);
        self.known_alerts.insert(hash, alert);
        hash
    }

    #[must_use = "`on_own_alert()` registers RMCs and messages but does not actually send them; make sure the returned values are forwarded to IO"]
    async fn on_own_alert(
        &mut self,
        alert: Alert<H, D, MK::Signature>,
    ) -> (
        AlertMessage<H, D, MK::Signature, MK::PartialMultisignature>,
        Recipient,
        H::Hash,
    ) {
        let forker = alert.forker();
        self.known_forkers.insert(forker, alert.proof.clone());
        let alert = Signed::sign(alert, self.keychain).await;
        let hash = self.rmc_alert(forker, alert.clone()).await;
        (
            AlertMessage::ForkAlert(alert.into_unchecked()),
            Recipient::Everyone,
            hash,
        )
    }

    #[must_use = "`on_network_alert()` may return a `ForkingNotification`, which should be propagated"]
    async fn on_network_alert(
        &mut self,
        alert: UncheckedSigned<Alert<H, D, MK::Signature>, MK::Signature>,
    ) -> Option<(Option<ForkingNotification<H, D, MK::Signature>>, H::Hash)> {
        let alert = match alert.check(self.keychain) {
            Ok(alert) => alert,
            Err(e) => {
                warn!(target: "AlephBFT-alerter","{:?} We have received an incorrectly signed alert: {:?}.", self.index(), e);
                return None;
            }
        };
        let contents = alert.as_signable();
        if let Some(forker) = self.who_is_forking(&contents.proof) {
            if self.known_rmcs.contains_key(&(contents.sender, forker)) {
                debug!(target: "AlephBFT-alerter","{:?} We already know about an alert by {:?} about {:?}.", self.index(), alert.as_signable().sender, forker);
                self.known_alerts.insert(contents.hash(), alert);
                return None;
            }
            let propagate_alert = if self.is_forker(forker) {
                None
            } else {
                // We learn about this forker for the first time, need to send our own alert
                self.on_new_forker_detected(forker, contents.proof.clone());
                Some(ForkingNotification::Forker(contents.proof.clone()))
            };
            let hash_for_rmc = self.rmc_alert(forker, alert).await;
            Some((propagate_alert, hash_for_rmc))
        } else {
            warn!(target: "AlephBFT-alerter","{:?} We have received an incorrect forking proof from {:?}.", self.index(), alert.as_signable().sender);
            None
        }
    }

    #[must_use = "`on_message()` may return an `AlerterResponse` which should be propagated"]
    async fn on_message(
        &mut self,
        message: AlertMessage<H, D, MK::Signature, MK::PartialMultisignature>,
    ) -> Option<AlerterResponse<H, D, MK::Signature, MK::PartialMultisignature>> {
        use AlertMessage::*;
        match message {
            ForkAlert(alert) => {
                trace!(target: "AlephBFT-alerter", "{:?} Fork alert received {:?}.", self.index(), alert);
                self.on_network_alert(alert)
                    .await
                    .map(|(n, h)| AlerterResponse::ForkResponse(n, h))
            }
            RmcMessage(sender, message) => {
                let hash = message.hash();
                if let Some(alert) = self.known_alerts.get(hash) {
                    let alert_id = (alert.as_signable().sender, alert.as_signable().forker());
                    if self.known_rmcs.get(&alert_id) == Some(hash) || message.is_complete() {
                        Some(AlerterResponse::RmcMessage(message))
                    } else {
                        None
                    }
                } else {
                    Some(AlerterResponse::AlertRequest(
                        *hash,
                        Recipient::Node(sender),
                    ))
                }
            }
            AlertRequest(node, hash) => match self.known_alerts.get(&hash) {
                Some(alert) => Some(AlerterResponse::ForkAlert(
                    alert.clone().into_unchecked(),
                    Recipient::Node(node),
                )),
                None => {
                    debug!(target: "AlephBFT-alerter", "{:?} Received request for unknown alert.", self.index());
                    None
                }
            },
        }
    }

    #[must_use = "`alert_confirmed()` may return a `ForkingNotification`, which should be propagated"]
    fn alert_confirmed(
        &mut self,
        multisigned: Multisigned<'a, H::Hash, MK>,
    ) -> Option<ForkingNotification<H, D, MK::Signature>> {
        let alert = match self.known_alerts.get(multisigned.as_signable()) {
            Some(alert) => alert.as_signable(),
            None => {
                error!(target: "AlephBFT-alerter", "{:?} Completed an RMC for an unknown alert.", self.index());
                return None;
            }
        };
        let forker = alert.proof.0.as_signable().creator();
        self.known_rmcs.insert((alert.sender, forker), alert.hash());
        if !self.correct_commitment(forker, &alert.legit_units) {
            warn!(target: "AlephBFT-alerter","{:?} We have received an incorrect unit commitment from {:?}.", self.index(), alert.sender);
            return None;
        }
        Some(ForkingNotification::Units(alert.legit_units.clone()))
    }
}

pub(crate) async fn run<H: Hasher, D: Data, MK: MultiKeychain>(
    keychain: MK,
    messages_for_network: Sender<(
        AlertMessage<H, D, MK::Signature, MK::PartialMultisignature>,
        Recipient,
    )>,
    messages_from_network: Receiver<AlertMessage<H, D, MK::Signature, MK::PartialMultisignature>>,
    notifications_for_units: Sender<ForkingNotification<H, D, MK::Signature>>,
    alerts_from_units: Receiver<Alert<H, D, MK::Signature>>,
    config: AlertConfig,
    mut exit: oneshot::Receiver<()>,
) {
    use self::io::IO;

    let n_members = config.n_members;
    let mut alerter = Alerter::new(&keychain, config);
    let (messages_for_rmc, messages_from_us) = mpsc::unbounded();
    let (messages_for_us, messages_from_rmc) = mpsc::unbounded();
    let mut io = IO {
        messages_for_network,
        messages_from_network,
        notifications_for_units,
        alerts_from_units,
        rmc: ReliableMulticast::new(
            messages_from_us,
            messages_for_us,
            &keychain,
            n_members,
            DoublingDelayScheduler::new(time::Duration::from_millis(500)),
        ),
        messages_from_rmc,
        messages_for_rmc,
        alerter_index: alerter.index(),
    };
    loop {
        futures::select! {
            message = io.messages_from_network.next() => match message {
                Some(message) => {
                    match alerter.on_message(message).await {
                        Some(AlerterResponse::ForkAlert(alert, recipient)) => {
                            io.send_message_for_network(
                                AlertMessage::ForkAlert(alert),
                                recipient,
                                &mut alerter.exiting,
                            );
                        }
                        Some(AlerterResponse::AlertRequest(hash, peer)) => {
                            let message = AlertMessage::AlertRequest(alerter.index(), hash);
                            io.send_message_for_network(message, peer, &mut alerter.exiting);
                        }
                        Some(AlerterResponse::RmcMessage(message)) => {
                            if io.messages_for_rmc.unbounded_send(message).is_err() {
                                warn!(target: "AlephBFT-alerter", "{:?} Channel with messages for rmc should be open", alerter.index());
                                alerter.exiting = true;
                            }
                        }
                        Some(AlerterResponse::ForkResponse(maybe_notification, hash)) => {
                            io.rmc.start_rmc(hash).await;
                            if let Some(notification) = maybe_notification {
                                io.send_notification_for_units(notification, &mut alerter.exiting);
                            }
                        }
                        None => {}
                    }
                }
                None => {
                    error!(target: "AlephBFT-alerter", "{:?} Message stream closed.", alerter.index());
                    break;
                }
            },
            alert = io.alerts_from_units.next() => match alert {
                Some(alert) => {
                    let (message, recipient, hash) = alerter.on_own_alert(alert.clone()).await;
                    io.send_message_for_network(message, recipient, &mut alerter.exiting);
                    io.rmc.start_rmc(hash).await;
                }
                None => {
                    error!(target: "AlephBFT-alerter", "{:?} Alert stream closed.", alerter.index());
                    break;
                }
            },
            message = io.messages_from_rmc.next() => match message {
                Some(message) => io.rmc_message_to_network(message, &mut alerter.exiting),
                None => {
                    error!(target: "AlephBFT-alerter", "{:?} RMC message stream closed.", alerter.index());
                    break;
                }
            },
            multisigned = io.rmc.next_multisigned_hash().fuse() => {
                if let Some(notification) = alerter.alert_confirmed(multisigned) {
                    io.send_notification_for_units(notification, &mut alerter.exiting);
                }
            },
            _ = &mut exit => {
                info!(target: "AlephBFT-alerter", "{:?} received exit signal", alerter.index());
                alerter.exiting = true;
            },
        }
        if alerter.exiting {
            info!(target: "AlephBFT-alerter", "{:?} Alerter decided to exit.", alerter.index());
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        alerts::{
            Alert, AlertConfig, AlertMessage, Alerter, AlerterResponse, ForkProof,
            ForkingNotification, RmcMessage,
        },
        units::{ControlHash, FullUnit, PreUnit},
        Recipient, Round,
    };
    use aleph_bft_mock::{Data, Hasher64, Keychain, Signature};
    use aleph_bft_types::{NodeCount, NodeIndex, NodeMap, Signable, Signed};

    type TestForkProof = ForkProof<Hasher64, Data, Signature>;

    fn full_unit(
        n_members: NodeCount,
        node_id: NodeIndex,
        round: Round,
        variant: u32,
    ) -> FullUnit<Hasher64, Data> {
        FullUnit::new(
            PreUnit::new(
                node_id,
                round,
                ControlHash::new(&NodeMap::with_size(n_members)),
            ),
            variant,
            0,
        )
    }

    /// Fabricates proof of a fork by a particular node, given its private key.
    async fn make_fork_proof(
        node_id: NodeIndex,
        keychain: &Keychain,
        round: Round,
        n_members: NodeCount,
    ) -> TestForkProof {
        let unit_0 = full_unit(n_members, node_id, round, 0);
        let unit_1 = full_unit(n_members, node_id, round, 1);
        let signed_unit_0 = Signed::sign(unit_0, keychain).await.into_unchecked();
        let signed_unit_1 = Signed::sign(unit_1, keychain).await.into_unchecked();
        (signed_unit_0, signed_unit_1)
    }

    #[tokio::test]
    async fn distributes_alert_from_units() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let mut own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof = make_fork_proof(forker_index, &forker_keychain, 0, n_members).await;
        let alert = Alert::new(own_index, fork_proof, vec![]);
        let signed_alert = Signed::sign(alert.clone(), own_node.keychain)
            .await
            .into_unchecked();
        let alert_hash = Signable::hash(&alert);
        assert_eq!(
            own_node.on_own_alert(alert).await,
            (
                AlertMessage::ForkAlert(signed_alert),
                Recipient::Everyone,
                alert_hash,
            ),
        );
    }

    #[tokio::test]
    async fn reacts_to_correctly_incoming_alert() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(1);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let mut own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof = make_fork_proof(forker_index, &forker_keychain, 0, n_members).await;
        let alert = Alert::new(own_index, fork_proof.clone(), vec![]);
        let alert_hash = Signable::hash(&alert);
        let signed_alert = Signed::sign(alert, own_node.keychain)
            .await
            .into_unchecked();
        assert_eq!(
            own_node.on_network_alert(signed_alert).await,
            Some((Some(ForkingNotification::Forker(fork_proof)), alert_hash)),
        );
    }

    #[tokio::test]
    async fn asks_about_unknown_alert() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let alerter_index = NodeIndex(1);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let alerter_keychain = Keychain::new(n_members, alerter_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let mut own_node: Alerter<Hasher64, Data, _> = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof = make_fork_proof(forker_index, &forker_keychain, 0, n_members).await;
        let alert = Alert::new(alerter_index, fork_proof.clone(), vec![]);
        let alert_hash = Signable::hash(&alert);
        let signed_alert_hash = Signed::sign_with_index(alert_hash, &alerter_keychain)
            .await
            .into_unchecked();
        let message =
            AlertMessage::RmcMessage(alerter_index, RmcMessage::SignedHash(signed_alert_hash));
        let response = own_node.on_message(message).await;
        assert_eq!(
            response,
            Some(AlerterResponse::AlertRequest(
                alert_hash,
                Recipient::Node(alerter_index),
            )),
        );
    }

    #[tokio::test]
    async fn ignores_wrong_alert() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let alerter_index = NodeIndex(1);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let alerter_keychain = Keychain::new(n_members, alerter_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let mut own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let valid_unit = Signed::sign(full_unit(n_members, alerter_index, 0, 0), &alerter_keychain)
            .await
            .into_unchecked();
        let wrong_fork_proof = (valid_unit.clone(), valid_unit);
        let wrong_alert = Alert::new(forker_index, wrong_fork_proof.clone(), vec![]);
        let signed_wrong_alert = Signed::sign(wrong_alert, &forker_keychain)
            .await
            .into_unchecked();
        assert_eq!(
            own_node
                .on_message(AlertMessage::ForkAlert(signed_wrong_alert))
                .await,
            None,
        );
    }

    #[tokio::test]
    async fn responds_to_alert_queries() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let mut own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let alert = Alert::new(
            own_index,
            make_fork_proof(forker_index, &forker_keychain, 0, n_members).await,
            vec![],
        );
        let alert_hash = Signable::hash(&alert);
        let signed_alert = Signed::sign(alert.clone(), &own_keychain)
            .await
            .into_unchecked();
        own_node
            .on_message(AlertMessage::ForkAlert(signed_alert.clone()))
            .await;
        for i in 1..n_members.0 {
            let node_id = NodeIndex(i);
            assert_eq!(
                own_node
                    .on_message(AlertMessage::AlertRequest(node_id, alert_hash))
                    .await,
                Some(AlerterResponse::ForkAlert(
                    signed_alert.clone(),
                    Recipient::Node(node_id),
                )),
            );
        }
    }

    #[tokio::test]
    async fn notifies_only_about_multisigned_alert() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let other_honest_node = NodeIndex(1);
        let double_committer = NodeIndex(5);
        let forker_index = NodeIndex(6);
        let keychains: Vec<_> = (0..n_members.0)
            .map(|i| Keychain::new(n_members, NodeIndex(i)))
            .collect();
        let mut own_node = Alerter::new(
            &keychains[own_index.0],
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof =
            make_fork_proof(forker_index, &keychains[forker_index.0], 0, n_members).await;
        let empty_alert = Alert::new(double_committer, fork_proof.clone(), vec![]);
        let empty_alert_hash = Signable::hash(&empty_alert);
        let signed_empty_alert = Signed::sign(empty_alert.clone(), &keychains[double_committer.0])
            .await
            .into_unchecked();
        let signed_empty_alert_hash =
            Signed::sign_with_index(empty_alert_hash, &keychains[double_committer.0])
                .await
                .into_unchecked();
        let multisigned_empty_alert_hash = signed_empty_alert_hash
            .check(&keychains[double_committer.0])
            .expect("the signature is correct")
            .into_partially_multisigned(&keychains[double_committer.0]);
        assert_eq!(
            own_node
                .on_message(AlertMessage::ForkAlert(signed_empty_alert))
                .await,
            Some(AlerterResponse::ForkResponse(
                Some(ForkingNotification::Forker(fork_proof.clone())),
                empty_alert_hash,
            )),
        );
        let message = RmcMessage::MultisignedHash(multisigned_empty_alert_hash.into_unchecked());
        assert_eq!(
            own_node
                .on_message(AlertMessage::RmcMessage(other_honest_node, message.clone()))
                .await,
            Some(AlerterResponse::RmcMessage(message)),
        );
        let forker_unit = fork_proof.0.clone();
        let nonempty_alert = Alert::new(
            double_committer,
            fork_proof.clone(),
            vec![forker_unit.clone()],
        );
        let nonempty_alert_hash = Signable::hash(&nonempty_alert);
        let signed_nonempty_alert =
            Signed::sign(nonempty_alert.clone(), &keychains[double_committer.0])
                .await
                .into_unchecked();
        let signed_nonempty_alert_hash =
            Signed::sign_with_index(nonempty_alert_hash, &keychains[double_committer.0])
                .await
                .into_unchecked();
        let mut multisigned_nonempty_alert_hash = signed_nonempty_alert_hash
            .check(&keychains[double_committer.0])
            .expect("the signature is correct")
            .into_partially_multisigned(&keychains[double_committer.0]);
        for i in 1..n_members.0 - 2 {
            let node_id = NodeIndex(i);
            let signed_nonempty_alert_hash =
                Signed::sign_with_index(nonempty_alert_hash, &keychains[node_id.0])
                    .await
                    .into_unchecked();
            multisigned_nonempty_alert_hash = multisigned_nonempty_alert_hash.add_signature(
                signed_nonempty_alert_hash
                    .check(&keychains[double_committer.0])
                    .expect("the signature is correct"),
                &keychains[double_committer.0],
            );
        }
        let message = RmcMessage::MultisignedHash(multisigned_nonempty_alert_hash.into_unchecked());
        assert_eq!(
            own_node
                .on_message(AlertMessage::ForkAlert(signed_nonempty_alert))
                .await,
            None,
        );
        assert_eq!(
            own_node
                .on_message(AlertMessage::RmcMessage(other_honest_node, message.clone()))
                .await,
            Some(AlerterResponse::RmcMessage(message)),
        );
    }

    #[tokio::test]
    async fn ignores_insufficiently_multisigned_alert() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let other_honest_node = NodeIndex(1);
        let double_committer = NodeIndex(5);
        let forker_index = NodeIndex(6);
        let keychains: Vec<_> = (0..n_members.0)
            .map(|i| Keychain::new(n_members, NodeIndex(i)))
            .collect();
        let mut own_node = Alerter::new(
            &keychains[own_index.0],
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof =
            make_fork_proof(forker_index, &keychains[forker_index.0], 0, n_members).await;
        let empty_alert = Alert::new(double_committer, fork_proof.clone(), vec![]);
        let empty_alert_hash = Signable::hash(&empty_alert);
        let signed_empty_alert = Signed::sign(empty_alert.clone(), &keychains[double_committer.0])
            .await
            .into_unchecked();
        assert_eq!(
            own_node
                .on_message(AlertMessage::ForkAlert(signed_empty_alert))
                .await,
            Some(AlerterResponse::ForkResponse(
                Some(ForkingNotification::Forker(fork_proof.clone())),
                empty_alert_hash,
            )),
        );
        let forker_unit = fork_proof.0.clone();
        let nonempty_alert = Alert::new(
            double_committer,
            fork_proof.clone(),
            vec![forker_unit.clone()],
        );
        let nonempty_alert_hash = Signable::hash(&nonempty_alert);
        let signed_nonempty_alert =
            Signed::sign(nonempty_alert.clone(), &keychains[double_committer.0])
                .await
                .into_unchecked();
        let signed_nonempty_alert_hash =
            Signed::sign_with_index(nonempty_alert_hash, &keychains[double_committer.0])
                .await
                .into_unchecked();
        let mut multisigned_nonempty_alert_hash = signed_nonempty_alert_hash
            .check(&keychains[double_committer.0])
            .expect("the signature is correct")
            .into_partially_multisigned(&keychains[double_committer.0]);
        for i in 1..3 {
            let node_id = NodeIndex(i);
            let signed_nonempty_alert_hash =
                Signed::sign_with_index(nonempty_alert_hash, &keychains[node_id.0])
                    .await
                    .into_unchecked();
            multisigned_nonempty_alert_hash = multisigned_nonempty_alert_hash.add_signature(
                signed_nonempty_alert_hash
                    .check(&keychains[double_committer.0])
                    .expect("the signature is correct"),
                &keychains[double_committer.0],
            );
        }
        let message = RmcMessage::MultisignedHash(multisigned_nonempty_alert_hash.into_unchecked());
        assert_eq!(
            own_node
                .on_message(AlertMessage::ForkAlert(signed_nonempty_alert))
                .await,
            None,
        );
        assert_eq!(
            own_node
                .on_message(AlertMessage::RmcMessage(other_honest_node, message.clone()))
                .await,
            None,
        );
    }

    #[tokio::test]
    async fn who_is_forking_ok() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof = make_fork_proof(forker_index, &forker_keychain, 0, n_members).await;
        assert_eq!(own_node.who_is_forking(&fork_proof), Some(forker_index));
    }

    #[tokio::test]
    async fn who_is_forking_wrong_session() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 1,
            },
        );
        let fork_proof = make_fork_proof(forker_index, &forker_keychain, 0, n_members).await;
        assert_eq!(own_node.who_is_forking(&fork_proof), None);
    }

    #[tokio::test]
    async fn who_is_forking_different_creators() {
        let n_members = NodeCount(7);
        let keychains: Vec<_> = (0..n_members.0)
            .map(|i| Keychain::new(n_members, NodeIndex(i)))
            .collect();
        let own_node = Alerter::new(
            &keychains[0],
            AlertConfig {
                n_members,
                session_id: 1,
            },
        );
        let fork_proof = {
            let unit_0 = full_unit(n_members, NodeIndex(6), 0, 0);
            let unit_1 = full_unit(n_members, NodeIndex(5), 0, 0);
            let signed_unit_0 = Signed::sign(unit_0, &keychains[6]).await.into_unchecked();
            let signed_unit_1 = Signed::sign(unit_1, &keychains[5]).await.into_unchecked();
            (signed_unit_0, signed_unit_1)
        };
        assert_eq!(own_node.who_is_forking(&fork_proof), None);
    }

    #[tokio::test]
    async fn who_is_forking_different_rounds() {
        let n_members = NodeCount(7);
        let own_index = NodeIndex(0);
        let forker_index = NodeIndex(6);
        let own_keychain = Keychain::new(n_members, own_index);
        let forker_keychain = Keychain::new(n_members, forker_index);
        let own_node = Alerter::new(
            &own_keychain,
            AlertConfig {
                n_members,
                session_id: 0,
            },
        );
        let fork_proof = {
            let unit_0 = full_unit(n_members, forker_index, 0, 0);
            let unit_1 = full_unit(n_members, forker_index, 1, 0);
            let signed_unit_0 = Signed::sign(unit_0, &forker_keychain)
                .await
                .into_unchecked();
            let signed_unit_1 = Signed::sign(unit_1, &forker_keychain)
                .await
                .into_unchecked();
            (signed_unit_0, signed_unit_1)
        };
        assert_eq!(own_node.who_is_forking(&fork_proof), None);
    }
}
