use crate::{
    alerts::AlertMessage,
    member::UnitMessage,
    signed::{PartialMultisignature, Signature},
    Data, Hasher, Network, Receiver, Recipient, Sender,
};
use codec::{Decode, Encode};
use futures::{channel::oneshot, FutureExt, StreamExt};
use log::{error, info, warn};
use std::fmt::Debug;

#[derive(Encode, Decode, Clone, Debug)]
pub(crate) enum NetworkDataInner<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> {
    Units(UnitMessage<H, D, S>),
    Alert(AlertMessage<H, D, S, MS>),
}

impl<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> NetworkDataInner<H, D, S, MS> {
    pub(crate) fn included_data(&self) -> Vec<D> {
        match self {
            Self::Units(message) => message.included_data(),
            Self::Alert(message) => message.included_data(),
        }
    }
}

/// NetworkData is the opaque format for all data that a committee member needs to send to other nodes.
#[derive(Clone, Debug)]
pub struct NetworkData<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature>(
    pub(crate) NetworkDataInner<H, D, S, MS>,
);

impl<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> Encode
    for NetworkData<H, D, S, MS>
{
    fn size_hint(&self) -> usize {
        self.0.size_hint()
    }

    fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
        self.0.encode_to(dest)
    }

    fn encode(&self) -> Vec<u8> {
        self.0.encode()
    }

    fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
        self.0.using_encoded(f)
    }
}

impl<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> Decode
    for NetworkData<H, D, S, MS>
{
    fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
        Ok(Self(NetworkDataInner::decode(input)?))
    }
}

impl<H: Hasher, D: Data, S: Signature, MS: PartialMultisignature> NetworkData<H, D, S, MS> {
    /// Returns all the Data in the network message that might end up in the ordering as a result
    /// of accepting this message. Useful for ensuring data availability, if Data only represents
    /// the objects the user wants to order, and facilitates access to the Data before it is
    /// ordered for optimization purposes.
    pub fn included_data(&self) -> Vec<D> {
        self.0.included_data()
    }
}

struct NetworkHub<
    H: Hasher,
    D: Data,
    S: Signature,
    MS: PartialMultisignature,
    N: Network<NetworkData<H, D, S, MS>>,
> {
    network: N,
    units_to_send: Receiver<(UnitMessage<H, D, S>, Recipient)>,
    units_received: Sender<UnitMessage<H, D, S>>,
    alerts_to_send: Receiver<(AlertMessage<H, D, S, MS>, Recipient)>,
    alerts_received: Sender<AlertMessage<H, D, S, MS>>,
}

impl<
        H: Hasher,
        D: Data,
        S: Signature,
        MS: PartialMultisignature,
        N: Network<NetworkData<H, D, S, MS>>,
    > NetworkHub<H, D, S, MS, N>
{
    fn new(
        network: N,
        units_to_send: Receiver<(UnitMessage<H, D, S>, Recipient)>,
        units_received: Sender<UnitMessage<H, D, S>>,
        alerts_to_send: Receiver<(AlertMessage<H, D, S, MS>, Recipient)>,
        alerts_received: Sender<AlertMessage<H, D, S, MS>>,
    ) -> Self {
        NetworkHub {
            network,
            units_to_send,
            units_received,
            alerts_to_send,
            alerts_received,
        }
    }

    fn send(&self, data: NetworkData<H, D, S, MS>, recipient: Recipient) {
        self.network.send(data, recipient);
    }

    fn handle_incoming(&self, network_data: NetworkData<H, D, S, MS>) {
        let NetworkData(network_data) = network_data;
        use NetworkDataInner::*;
        match network_data {
            Units(unit_message) => {
                if let Err(e) = self.units_received.unbounded_send(unit_message) {
                    warn!(target: "AlephBFT-network-hub", "Error when sending units to consensus {:?}", e);
                }
            }

            Alert(alert_message) => {
                if let Err(e) = self.alerts_received.unbounded_send(alert_message) {
                    warn!(target: "AlephBFT-network-hub", "Error when sending alerts to consensus {:?}", e);
                }
            }
        }
    }

    async fn run(mut self, mut exit: oneshot::Receiver<()>) {
        loop {
            use NetworkDataInner::*;
            futures::select! {
                unit_message = self.units_to_send.next() => match unit_message {
                    Some((unit_message, recipient)) => self.send(NetworkData(Units(unit_message)), recipient),
                    None => {
                        error!(target: "AlephBFT-network-hub", "Outgoing units stream closed.");
                        break;
                    }
                },
                alert_message = self.alerts_to_send.next() => match alert_message {
                    Some((alert_message, recipient)) => self.send(NetworkData(Alert(alert_message)), recipient),
                    None => {
                        error!(target: "AlephBFT-network-hub", "Outgoing alerts stream closed.");
                        break;
                    }
                },
                incoming_message = self.network.next_event().fuse() => match incoming_message {
                    Some(incoming_message) => self.handle_incoming(incoming_message),
                    None => {
                        error!(target: "AlephBFT-network-hub", "Network stopped working.");
                        break;
                    }
                },
                _ = &mut exit => break,
            }
        }
        info!(target: "AlephBFT-network-hub", "Network ended.");
    }
}

pub(crate) async fn run<
    H: Hasher,
    D: Data,
    S: Signature,
    MS: PartialMultisignature,
    N: Network<NetworkData<H, D, S, MS>>,
>(
    network: N,
    units_to_send: Receiver<(UnitMessage<H, D, S>, Recipient)>,
    units_received: Sender<UnitMessage<H, D, S>>,
    alerts_to_send: Receiver<(AlertMessage<H, D, S, MS>, Recipient)>,
    alerts_received: Sender<AlertMessage<H, D, S, MS>>,
    exit: oneshot::Receiver<()>,
) {
    NetworkHub::new(
        network,
        units_to_send,
        units_received,
        alerts_to_send,
        alerts_received,
    )
    .run(exit)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        testing::mock::{self, Data, Hasher64, PartialMultisignature, Signature},
        units::{ControlHash, FullUnit, PreUnit, UncheckedSignedUnit, UnitCoord},
        NodeIndex, NodeSubset, Round, UncheckedSigned,
    };

    fn test_unchecked_unit(
        creator: NodeIndex,
        round: Round,
        variant: u32,
    ) -> UncheckedSignedUnit<Hasher64, Data, Signature> {
        let control_hash = ControlHash {
            parents_mask: NodeSubset::with_size(7.into()),
            combined_hash: 0.using_encoded(Hasher64::hash),
        };
        let pu = PreUnit::new(creator, round, control_hash);
        let data = Data::new(UnitCoord::new(7, 13.into()), variant);
        UncheckedSigned::new(FullUnit::new(pu, data, 0), Signature {})
    }

    #[test]
    fn decoding_network_data_units_new_unit() {
        use NetworkDataInner::Units;
        use UnitMessage::NewUnit;

        let uu = test_unchecked_unit(5.into(), 43, 1729);
        let included_data = vec![uu.as_signable().data().clone()];
        let nd = NetworkData::<Hasher64, Data, Signature, PartialMultisignature>(Units(NewUnit(
            uu.clone(),
        )));
        let decoded = mock::NetworkData::decode(&mut &nd.encode()[..]);
        assert!(decoded.is_ok(), "Bug in encode/decode for NewUnit");
        let decoded = decoded.unwrap();
        assert_eq!(
            decoded.included_data(),
            included_data,
            "data decoded incorrectly"
        );
        if let Units(NewUnit(decoded_unchecked)) = decoded.0 {
            assert_eq!(
                uu.as_signable(),
                decoded_unchecked.as_signable(),
                "decoded should equal encoded"
            );
        } else {
            panic!("Decoded NewUnit as something else");
        }
    }

    #[test]
    fn decoding_network_data_units_request_coord() {
        use NetworkDataInner::Units;
        use UnitMessage::RequestCoord;

        let ni = 7.into();
        let uc = UnitCoord::new(3, 13.into());
        let nd = NetworkData::<Hasher64, Data, Signature, PartialMultisignature>(Units(
            RequestCoord(ni, uc),
        ));
        let decoded = mock::NetworkData::decode(&mut &nd.encode()[..]);
        assert!(decoded.is_ok(), "Bug in encode/decode for RequestCoord");
        let decoded = decoded.unwrap();
        assert!(
            decoded.included_data().is_empty(),
            "data returned from a coord request"
        );
        if let Units(RequestCoord(dni, duc)) = decoded.0 {
            assert!(ni == dni && uc == duc, "decoded should equal encoded");
        } else {
            panic!("Decoded RequestCoord as something else");
        }
    }

    #[test]
    fn decoding_network_data_units_response_coord() {
        use NetworkDataInner::Units;
        use UnitMessage::ResponseCoord;

        let uu = test_unchecked_unit(5.into(), 43, 1729);
        let included_data = vec![uu.as_signable().data().clone()];
        let nd = NetworkData::<Hasher64, Data, Signature, PartialMultisignature>(Units(
            ResponseCoord(uu.clone()),
        ));
        let decoded = mock::NetworkData::decode(&mut &nd.encode()[..]);
        assert!(decoded.is_ok(), "Bug in encode/decode for ResponseCoord");
        let decoded = decoded.unwrap();
        assert_eq!(
            decoded.included_data(),
            included_data,
            "data decoded incorrectly"
        );
        if let Units(ResponseCoord(decoded_unchecked)) = decoded.0 {
            assert_eq!(
                uu.as_signable(),
                decoded_unchecked.as_signable(),
                "decoded should equal encoded"
            );
        } else {
            panic!("Decoded ResponseCoord as something else");
        }
    }

    #[test]
    fn decoding_network_data_units_request_parents() {
        use NetworkDataInner::Units;
        use UnitMessage::RequestParents;

        let ni = 7.into();
        let h = 43.using_encoded(Hasher64::hash);
        let nd = NetworkData::<Hasher64, Data, Signature, PartialMultisignature>(Units(
            RequestParents(ni, h),
        ));
        let decoded = mock::NetworkData::decode(&mut &nd.encode()[..]);
        assert!(decoded.is_ok(), "Bug in encode/decode for RequestParents");
        let decoded = decoded.unwrap();
        assert!(
            decoded.included_data().is_empty(),
            "data returned from a parent request"
        );
        if let Units(RequestParents(dni, dh)) = decoded.0 {
            assert!(ni == dni && h == dh, "decoded should equal encoded");
        } else {
            panic!("Decoded RequestParents as something else");
        }
    }

    #[test]
    fn decoding_network_data_units_response_parents() {
        use NetworkDataInner::Units;
        use UnitMessage::ResponseParents;

        let h = 43.using_encoded(Hasher64::hash);
        let p1 = test_unchecked_unit(5.into(), 43, 1729);
        let p2 = test_unchecked_unit(13.into(), 43, 1729);
        let p3 = test_unchecked_unit(17.into(), 43, 1729);
        let included_data = vec![
            p1.as_signable().data().clone(),
            p2.as_signable().data().clone(),
            p3.as_signable().data().clone(),
        ];
        let parents = vec![p1, p2, p3];

        let nd = NetworkData::<Hasher64, Data, Signature, PartialMultisignature>(Units(
            ResponseParents(h, parents.clone()),
        ));
        let decoded = mock::NetworkData::decode(&mut &nd.encode()[..]);
        assert!(decoded.is_ok(), "Bug in encode/decode for ResponseParents");
        let decoded = decoded.unwrap();
        assert_eq!(
            decoded.included_data(),
            included_data,
            "data decoded incorrectly"
        );
        if let Units(ResponseParents(dh, dparents)) = decoded.0 {
            assert_eq!(h, dh, "decoded should equal encoded");
            assert_eq!(
                parents.len(),
                dparents.len(),
                "decoded should equal encoded"
            );
            for (p, dp) in parents.iter().zip(dparents.iter()) {
                assert_eq!(
                    p.as_signable(),
                    dp.as_signable(),
                    "decoded should equal encoded"
                );
            }
        } else {
            panic!("Decoded ResponseParents as something else");
        }
    }

    #[test]
    fn decoding_network_data_alert_fork_alert() {
        use AlertMessage::ForkAlert;
        use NetworkDataInner::Alert;

        let forker = 9.into();
        let f1 = test_unchecked_unit(forker, 10, 0);
        let f2 = test_unchecked_unit(forker, 10, 1);
        let lu1 = test_unchecked_unit(forker, 11, 0);
        let lu2 = test_unchecked_unit(forker, 12, 0);
        let included_data = vec![
            lu1.as_signable().data().clone(),
            lu2.as_signable().data().clone(),
        ];
        let alert = crate::alerts::Alert::new(7.into(), (f1, f2), vec![lu1, lu2]);

        let nd = NetworkData::<Hasher64, Data, Signature, PartialMultisignature>(Alert(ForkAlert(
            UncheckedSigned::new(alert.clone(), Signature {}),
        )));
        let decoded = mock::NetworkData::decode(&mut &nd.encode()[..]);
        assert!(decoded.is_ok(), "Bug in encode/decode for ForkAlert");
        let decoded = decoded.unwrap();
        assert_eq!(
            decoded.included_data(),
            included_data,
            "data decoded incorrectly"
        );
        if let Alert(ForkAlert(unchecked_alert)) = decoded.0 {
            assert_eq!(
                &alert,
                unchecked_alert.as_signable(),
                "decoded should equal encoded"
            )
        } else {
            panic!("Decoded ForkAlert as something else");
        }
    }
}
