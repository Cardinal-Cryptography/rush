use crate::{
    alerts::Alert,
    units::{UncheckedSignedUnit, UnitCoord},
    Data, Hasher, Keychain, MultiKeychain, Multisigned, NodeIndex, Receiver, Round, Sender,
    SessionId, Terminator,
};

use codec::{Decode, Encode, Error as CodecError};
use futures::{channel::oneshot, FutureExt, StreamExt};
use itertools::{Either, Itertools};
use log::{debug, error, info, warn};
use std::{
    collections::HashSet,
    fmt,
    fmt::Debug,
    io::{Read, Write},
    marker::PhantomData,
};

const LOG_TARGET: &str = "AlephBFT-backup";

#[derive(Clone, Debug, Decode, Encode, PartialEq)]
pub enum BackupItem<H: Hasher, D: Data, MK: MultiKeychain> {
    Unit(UncheckedSignedUnit<H, D, MK::Signature>),
    AlertData(AlertData<H, D, MK>),
}

#[derive(Clone, Debug, Decode, Encode, PartialEq)]
pub enum AlertData<H: Hasher, D: Data, MK: MultiKeychain> {
    OwnAlert(Alert<H, D, MK::Signature>),
    NetworkAlert(Alert<H, D, MK::Signature>),
    MultisignedHash(Multisigned<H::Hash, MK>),
}

/// Backup read error. Could be either caused by io error from `BackupReader`, or by decoding.
#[derive(Debug)]
enum ReadError {
    IO(std::io::Error),
    Codec(CodecError),
}

#[derive(Debug)]
enum IncorrectBackupError {
    InconsistentData(UnitCoord),
    WrongSession(UnitCoord, SessionId, SessionId),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::IO(err) => {
                write!(
                    f,
                    "received IO error while reading from backup source: {}",
                    err
                )
            }
            ReadError::Codec(err) => {
                write!(f, "received Codec error while decoding backup: {}", err)
            }
        }
    }
}

impl fmt::Display for IncorrectBackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IncorrectBackupError::InconsistentData(coord) => {
                write!(
                    f,
                    "inconsistent backup data. Unit from round {:?} of creator {:?} is missing a parent in backup.",
                    coord.round(), coord.creator()
                )
            }
            IncorrectBackupError::WrongSession(coord, expected_session, actual_session) => {
                write!(
                    f,
                    "unit from round {:?} of creator {:?} has a wrong session id in backup. Expected: {:?} got: {:?}",
                    coord.round(), coord.creator(), expected_session, actual_session
                )
            }
        }
    }
}

impl From<std::io::Error> for ReadError {
    fn from(err: std::io::Error) -> Self {
        Self::IO(err)
    }
}

impl From<CodecError> for ReadError {
    fn from(err: CodecError) -> Self {
        Self::Codec(err)
    }
}

pub type LoadedData<H, D, MK> = (
    Vec<UncheckedSignedUnit<H, D, <MK as Keychain>::Signature>>,
    Vec<AlertData<H, D, MK>>,
);

pub struct BackupLoader<H: Hasher, D: Data, MK: MultiKeychain, R: Read> {
    backup: R,
    index: NodeIndex,
    session_id: SessionId,
    _phantom: PhantomData<(H, D, MK)>,
}

impl<H: Hasher, D: Data, MK: MultiKeychain, R: Read> BackupLoader<H, D, MK, R> {
    pub fn new(backup: R, index: NodeIndex, session_id: SessionId) -> BackupLoader<H, D, MK, R> {
        BackupLoader {
            backup,
            index,
            session_id,
            _phantom: PhantomData,
        }
    }

    fn load(&mut self) -> Result<Vec<BackupItem<H, D, MK>>, ReadError> {
        let mut buf = Vec::new();
        self.backup.read_to_end(&mut buf)?;
        let input = &mut &buf[..];
        let mut result = Vec::new();
        while !input.is_empty() {
            result.push(<BackupItem<H, D, MK>>::decode(input)?);
        }
        Ok(result)
    }

    fn verify_units(
        &self,
        units: &Vec<UncheckedSignedUnit<H, D, MK::Signature>>,
    ) -> Result<(), IncorrectBackupError> {
        let mut already_loaded_coords = HashSet::new();

        for unit in units {
            let full_unit = unit.as_signable();
            let coord = full_unit.coord();

            if full_unit.session_id() != self.session_id {
                return Err(IncorrectBackupError::WrongSession(
                    coord,
                    self.session_id,
                    full_unit.session_id(),
                ));
            }

            let parent_ids = &full_unit.as_pre_unit().control_hash().parents_mask;

            // Sanity check: verify that all unit's parents appeared in backup before it.
            for parent_id in parent_ids.elements() {
                let parent = UnitCoord::new(coord.round() - 1, parent_id);
                if !already_loaded_coords.contains(&parent) {
                    return Err(IncorrectBackupError::InconsistentData(coord));
                }
            }

            already_loaded_coords.insert(coord);
        }

        Ok(())
    }

    fn on_shutdown(&self, starting_round: oneshot::Sender<Option<Round>>) {
        if starting_round.send(None).is_err() {
            warn!(target: LOG_TARGET, "Could not send `None` starting round.");
        }
    }

    pub async fn run(
        &mut self,
        loaded_data: oneshot::Sender<LoadedData<H, D, MK>>,
        starting_round: oneshot::Sender<Option<Round>>,
        next_round_collection: oneshot::Receiver<Round>,
    ) {
        let items = match self.load() {
            Ok(items) => items,
            Err(e) => {
                error!(target: LOG_TARGET, "unable to load backup data: {}", e);
                self.on_shutdown(starting_round);
                return;
            }
        };

        let (units, alert_data): (Vec<_>, Vec<_>) =
            items.into_iter().partition_map(|item| match item {
                BackupItem::Unit(unit) => Either::Left(unit),
                BackupItem::AlertData(data) => Either::Right(data),
            });

        if let Err(e) = self.verify_units(&units) {
            error!(target: LOG_TARGET, "incorrect backup data: {}", e);
            self.on_shutdown(starting_round);
            return;
        }

        let next_round_backup: Round = units
            .iter()
            .filter(|u| u.as_signable().creator() == self.index)
            .map(|u| u.as_signable().round())
            .max()
            .map(|round| round + 1)
            .unwrap_or(0);

        info!(
            target: LOG_TARGET,
            "Loaded {:?} units from backup. Able to continue from round: {:?}.",
            units.len(),
            next_round_backup
        );

        if loaded_data.send((units, alert_data)).is_err() {
            error!(target: LOG_TARGET, "Could not send loaded items");
            self.on_shutdown(starting_round);
            return;
        }

        let next_round_collection = match next_round_collection.await {
            Ok(round) => round,
            Err(e) => {
                error!(
                    target: LOG_TARGET,
                    "Unable to receive response from unit collection: {}", e
                );
                self.on_shutdown(starting_round);
                return;
            }
        };

        info!(
            target: LOG_TARGET,
            "Next round inferred from collection: {:?}", next_round_collection
        );

        if next_round_backup < next_round_collection {
            // Our newest unit doesn't appear in the backup. This indicates a serious issue, for example
            // a different node running with the same pair of keys. It's safer not to continue.
            error!(
            target: LOG_TARGET, "Backup state behind unit collection state. Next round inferred from: collection: {:?}, backup: {:?}",
            next_round_collection,
            next_round_backup,
        );
            self.on_shutdown(starting_round);
            return;
        };

        if next_round_collection < next_round_backup {
            // Our newest unit didn't reach any peer, but it resides in our backup. One possible reason
            // is that our node was taken down after saving the unit, but before broadcasting it.
            warn!(
                target: LOG_TARGET, "Backup state ahead of than unit collection state. Next round inferred from: collection: {:?}, backup: {:?}",
                next_round_backup,
                next_round_collection
            );
        }

        if let Err(e) = starting_round.send(Some(next_round_backup)) {
            error!(target: LOG_TARGET, "Could not send starting round: {:?}", e);
        }
    }
}

/// Component responsible for saving units and alert data into backup.
/// It waits for items to appear on its receivers, and writes them to backup.
/// It announces a successful write through an appropriate response sender.
pub struct BackupSaver<H: Hasher, D: Data, MK: MultiKeychain, W: Write> {
    units_from_runway: Receiver<UncheckedSignedUnit<H, D, MK::Signature>>,
    data_from_alerter: Receiver<AlertData<H, D, MK>>,
    responses_for_runway: Sender<UncheckedSignedUnit<H, D, MK::Signature>>,
    responses_for_alerter: Sender<AlertData<H, D, MK>>,
    backup: W,
}

impl<H: Hasher, D: Data, MK: MultiKeychain, W: Write> BackupSaver<H, D, MK, W> {
    pub fn new(
        units_from_runway: Receiver<UncheckedSignedUnit<H, D, MK::Signature>>,
        data_from_alerter: Receiver<AlertData<H, D, MK>>,
        responses_for_runway: Sender<UncheckedSignedUnit<H, D, MK::Signature>>,
        responses_for_alerter: Sender<AlertData<H, D, MK>>,
        backup: W,
    ) -> BackupSaver<H, D, MK, W> {
        BackupSaver {
            units_from_runway,
            data_from_alerter,
            responses_for_runway,
            responses_for_alerter,
            backup,
        }
    }

    pub fn save_item(&mut self, item: BackupItem<H, D, MK>) -> Result<(), std::io::Error> {
        self.backup.write_all(&item.encode())?;
        self.backup.flush()?;
        Ok(())
    }

    pub async fn run(&mut self, mut terminator: Terminator) {
        let mut terminator_exit = false;
        loop {
            futures::select! {
                unit = self.units_from_runway.next() => {
                    let unit = match unit {
                        Some(unit) => unit,
                        None => {
                            error!(target: LOG_TARGET, "receiver of units to save closed early");
                            break;
                        },
                    };
                    let item = BackupItem::Unit(unit.clone());
                    if let Err(e) = self.save_item(item) {
                        error!(target: LOG_TARGET, "couldn't save item to backup: {:?}", e);
                        break;
                    }
                    if self.responses_for_runway.unbounded_send(unit).is_err() {
                        error!(target: LOG_TARGET, "couldn't respond with saved unit to runway");
                        break;
                    }
                },
                data = self.data_from_alerter.next() => {
                    let data = match data {
                        Some(data) => data,
                        None => {
                            error!(target: LOG_TARGET, "receiver of alert data to save closed early");
                            break;
                        },
                    };
                    let item = BackupItem::AlertData(data.clone());
                    if let Err(e) = self.save_item(item) {
                        error!(target: LOG_TARGET, "couldn't save item to backup: {:?}", e);
                        break;
                    }
                    if self.responses_for_alerter.unbounded_send(data).is_err() {
                        error!(target: LOG_TARGET, "couldn't respond with saved alert data to runway");
                        break;
                    }
                }
                _ = terminator.get_exit().fuse() => {
                    debug!(target: LOG_TARGET, "backup saver received exit signal.");
                    terminator_exit = true;
                }
            }

            if terminator_exit {
                debug!(target: LOG_TARGET, "backup saver decided to exit.");
                terminator.terminate_sync().await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        runway::backup::{BackupItem, BackupLoader},
        units::{
            create_units, creator_set, preunit_to_unchecked_signed_unit, preunit_to_unit,
            UncheckedSignedUnit as GenericUncheckedSignedUnit,
        },
        NodeCount, NodeIndex, Round, SessionId,
    };
    use aleph_bft_mock::{Data, Hasher64, Keychain, Loader, Signature};
    use codec::Encode;
    use futures::channel::oneshot;

    use crate::runway::backup::LoadedData;

    type UncheckedSignedUnit = GenericUncheckedSignedUnit<Hasher64, Data, Signature>;
    type TestBackupItem = BackupItem<Hasher64, Data, Keychain>;
    struct PrepareTestResponse<F: futures::Future> {
        task: F,
        loaded_data_rx: oneshot::Receiver<LoadedData<Hasher64, Data, Keychain>>,
        highest_response_tx: oneshot::Sender<Round>,
        starting_round_rx: oneshot::Receiver<Option<Round>>,
    }

    const SESSION_ID: SessionId = 43;
    const NODE_ID: NodeIndex = NodeIndex(0);
    const N_MEMBERS: NodeCount = NodeCount(4);

    fn produce_units(rounds: usize, session_id: SessionId) -> Vec<Vec<UncheckedSignedUnit>> {
        let mut creators = creator_set(N_MEMBERS);
        let keychains: Vec<_> = (0..N_MEMBERS.0)
            .map(|id| Keychain::new(N_MEMBERS, NodeIndex(id)))
            .collect();

        let mut units_per_round = Vec::with_capacity(rounds);

        for round in 0..rounds {
            let pre_units = create_units(creators.iter(), round as Round);

            let units: Vec<_> = pre_units
                .iter()
                .map(|(pre_unit, _)| preunit_to_unit(pre_unit.clone(), session_id))
                .collect();
            for creator in creators.iter_mut() {
                creator.add_units(&units);
            }

            let mut unchecked_signed_units = Vec::with_capacity(pre_units.len());
            for ((pre_unit, _), keychain) in pre_units.into_iter().zip(keychains.iter()) {
                unchecked_signed_units.push(preunit_to_unchecked_signed_unit(
                    pre_unit, session_id, keychain,
                ))
            }

            units_per_round.push(unchecked_signed_units);
        }

        // units_per_round[i][j] is the unit produced in round i by creator j
        units_per_round
    }

    fn units_of_creator(
        units: Vec<Vec<UncheckedSignedUnit>>,
        creator: NodeIndex,
    ) -> Vec<UncheckedSignedUnit> {
        units
            .into_iter()
            .map(|units_per_round| units_per_round[creator.0].clone())
            .collect()
    }

    fn encode_all(items: Vec<TestBackupItem>) -> Vec<Vec<u8>> {
        items.iter().map(|u| u.encode()).collect()
    }

    fn prepare_test(encoded_items: Vec<u8>) -> PrepareTestResponse<impl futures::Future> {
        let (loaded_data_tx, loaded_data_rx) = oneshot::channel();
        let (starting_round_tx, starting_round_rx) = oneshot::channel();
        let (highest_response_tx, highest_response_rx) = oneshot::channel();

        let task = {
            let mut backup_loader =
                BackupLoader::new(Loader::new(encoded_items), NODE_ID, SESSION_ID);

            async move {
                backup_loader
                    .run(loaded_data_tx, starting_round_tx, highest_response_rx)
                    .await
            }
        };

        PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        }
    }

    #[tokio::test]
    async fn nothing_loaded_nothing_collected_succeeds() {
        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(Vec::new());

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(Some(0)));
        assert_eq!(loaded_data_rx.await, Ok((Vec::new(), Vec::new())));
    }

    #[tokio::test]
    async fn something_loaded_nothing_collected_succeeds() {
        let units: Vec<_> = produce_units(5, SESSION_ID).into_iter().flatten().collect();
        let items: Vec<_> = units.clone().into_iter().map(BackupItem::Unit).collect();
        let encoded_items = encode_all(items.clone()).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(Some(5)));
        assert_eq!(loaded_data_rx.await, Ok((units, Vec::new())));
    }

    #[tokio::test]
    async fn something_loaded_something_collected_succeeds() {
        let units: Vec<_> = produce_units(5, SESSION_ID).into_iter().flatten().collect();
        let items: Vec<_> = units.clone().into_iter().map(BackupItem::Unit).collect();
        let encoded_items = encode_all(items.clone()).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(5).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(Some(5)));
        assert_eq!(loaded_data_rx.await, Ok((units, Vec::new())));
    }

    #[tokio::test]
    async fn nothing_loaded_something_collected_fails() {
        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(Vec::new());

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(1).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert_eq!(loaded_data_rx.await, Ok((Vec::new(), Vec::new())));
    }

    #[tokio::test]
    async fn loaded_smaller_then_collected_fails() {
        let units: Vec<_> = produce_units(3, SESSION_ID).into_iter().flatten().collect();
        let items: Vec<_> = units.clone().into_iter().map(BackupItem::Unit).collect();
        let encoded_items = encode_all(items.clone()).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(4).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert_eq!(loaded_data_rx.await, Ok((units, Vec::new())));
    }

    #[tokio::test]
    async fn dropped_collection_fails() {
        let units: Vec<_> = produce_units(3, SESSION_ID).into_iter().flatten().collect();
        let items: Vec<_> = units.clone().into_iter().map(BackupItem::Unit).collect();
        let encoded_items = encode_all(items.clone()).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);

        let handle = tokio::spawn(async {
            task.await;
        });

        drop(highest_response_tx);
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert_eq!(loaded_data_rx.await, Ok((units, Vec::new())));
    }

    #[tokio::test]
    async fn backup_with_corrupted_encoding_fails() {
        let units: Vec<_> = produce_units(5, SESSION_ID).into_iter().flatten().collect();
        let items: Vec<_> = units.into_iter().map(BackupItem::Unit).collect();
        let mut item_encodings = encode_all(items);
        let unit2_encoding_len = item_encodings[2].len();
        item_encodings[2].resize(unit2_encoding_len - 1, 0); // remove the last byte
        let encoded_items = item_encodings.into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);
        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert!(loaded_data_rx.await.is_err());
    }

    #[tokio::test]
    async fn backup_with_missing_parent_fails() {
        let units: Vec<_> = produce_units(5, SESSION_ID).into_iter().flatten().collect();
        let mut items: Vec<_> = units.into_iter().map(BackupItem::Unit).collect();
        items.remove(2); // it is a parent of all units of round 3
        let encoded_items = encode_all(items).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);
        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert!(loaded_data_rx.await.is_err());
    }

    #[tokio::test]
    async fn backup_with_duplicate_unit_succeeds() {
        let mut units: Vec<_> = produce_units(5, SESSION_ID).into_iter().flatten().collect();
        let unit2_duplicate = units[2].clone();
        units.insert(3, unit2_duplicate);
        let items: Vec<_> = units.clone().into_iter().map(BackupItem::Unit).collect();
        let encoded_units = encode_all(items.clone()).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_units);

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(Some(5)));
        assert_eq!(loaded_data_rx.await, Ok((units, Vec::new())));
    }

    #[tokio::test]
    async fn backup_with_units_of_one_creator_fails() {
        let units = units_of_creator(produce_units(5, SESSION_ID), NodeIndex(NODE_ID.0 + 1));
        let items: Vec<_> = units.into_iter().map(BackupItem::Unit).collect();
        let encoded_items = encode_all(items).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert!(loaded_data_rx.await.is_err());
    }

    #[tokio::test]
    async fn backup_with_wrong_session_fails() {
        let units: Vec<_> = produce_units(5, SESSION_ID + 1)
            .into_iter()
            .flatten()
            .collect();
        let items: Vec<_> = units.into_iter().map(BackupItem::Unit).collect();
        let encoded_items = encode_all(items).into_iter().flatten().collect();

        let PrepareTestResponse {
            task,
            loaded_data_rx,
            highest_response_tx,
            starting_round_rx,
        } = prepare_test(encoded_items);

        let handle = tokio::spawn(async {
            task.await;
        });

        highest_response_tx.send(0).unwrap();
        handle.await.unwrap();

        assert_eq!(starting_round_rx.await, Ok(None));
        assert!(loaded_data_rx.await.is_err());
    }
}
