// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Test utilities for `walrus-sui`.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::anyhow;
use sui_types::{base_types::ObjectID, event::EventID};
use tokio::sync::broadcast::{self, Sender};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use walrus_core::{
    messages::{ConfirmationCertificate, InvalidBlobCertificate, InvalidBlobIdMsg},
    test_utils,
    BlobId,
    EncodingType,
    Epoch,
};

const DIGEST_LEN: usize = 32;

use super::event_id_for_testing;
use crate::{
    client::{ContractClient, ReadClient, SuiClientResult},
    test_utils::EventForTesting,
    types::{
        Blob,
        BlobCertified,
        BlobEvent,
        BlobRegistered,
        Committee,
        EpochStatus,
        InvalidBlobId,
        StorageResource,
        SystemObject,
    },
};

/// Mock `ReadClient` for testing.
///
/// All events added to a `MockSuiReadClient` are provided in all event streams returned from any
/// clone of the `MockSuiReadClient`. This includes events added using the `add_event` function
/// after creating the client.
#[derive(Debug, Clone)]
pub struct MockSuiReadClient {
    events: Arc<Mutex<Vec<BlobEvent>>>,
    events_channel: Sender<BlobEvent>,
    committee: Option<Committee>,
}

impl MockSuiReadClient {
    /// Create a new mock client that returns the provided events as the initial sequence of
    /// events in the event streams.
    /// The provided `committee` is returned when calling `current_committee` on
    /// the client.
    pub fn new_with_events(events: Vec<BlobEvent>, committee: Option<Committee>) -> Self {
        // A channel capacity of 1024 should be enough capacity to not feel backpressure for testing
        let (events_channel, _) = broadcast::channel(1024);
        Self {
            events: Arc::new(Mutex::new(events)),
            events_channel,
            committee,
        }
    }

    /// Create a new mock client that returns registered and certified events for
    /// the given `blob_ids` as the initial sequence of events in the event streams.
    /// The provided `committee` is returned when calling `current_committee` on
    /// the client.
    pub fn new_with_blob_ids(
        blob_ids: impl IntoIterator<Item = BlobId>,
        committee: Option<Committee>,
    ) -> Self {
        let events = blob_ids
            .into_iter()
            .flat_map(|blob_id| {
                [
                    BlobRegistered::for_testing(blob_id).into(),
                    BlobCertified::for_testing(blob_id).into(),
                ]
            })
            .collect();
        Self::new_with_events(events, committee)
    }

    /// Add a `BlobEvent` to the event streams provided by this client and its clones.
    pub fn add_event(&self, event: BlobEvent) {
        // ignore unsuccessful sends, we might have new receivers in the future
        let _ = self.events_channel.send(event.clone());
        // unwrap `LockResult` since we are not expecting
        // threads to ever fail while holding the lock.
        (*self.events.lock().unwrap()).push(event);
    }
}

impl ReadClient for MockSuiReadClient {
    async fn price_per_unit_size(&self) -> SuiClientResult<u64> {
        Ok(10)
    }

    async fn blob_events(
        &self,
        polling_interval: Duration,
        _cursor: Option<EventID>,
    ) -> SuiClientResult<impl Stream<Item = BlobEvent>> {
        let rx = self.events_channel.subscribe();

        let events_guard = self.events.lock().unwrap();
        let old_event_stream = tokio_stream::iter((*events_guard).clone());
        // release lock
        drop(events_guard);
        Ok(old_event_stream.chain(
            BroadcastStream::from(rx)
                .filter_map(|res| res.ok())
                .throttle(polling_interval),
        ))
    }

    async fn get_blob_event(&self, event_id: EventID) -> SuiClientResult<BlobEvent> {
        Ok(BlobEvent::Certified(BlobCertified {
            epoch: 0,
            blob_id: test_utils::random_blob_id(),
            end_epoch: 1,
            event_id,
        }))
    }

    async fn get_system_object(&self) -> SuiClientResult<SystemObject> {
        Ok(system_object_from_committee(
            self.current_committee().await?,
        ))
    }

    async fn current_committee(&self) -> SuiClientResult<Committee> {
        Ok(self
            .committee
            .as_ref()
            .ok_or_else(|| anyhow!("no committee set in mock client"))?
            .to_owned())
    }
}

/// Mock `ContractClient` for testing.
///
/// Currently only covers the happy case, i.e. every call succeeds.  Calling its functions will add
/// corresponding events to the event streams returned by its read client (returned by
/// `read_client()`) as well as by any clones thereof.
#[derive(Debug)]
pub struct MockContractClient {
    /// Client to read Walrus on-chain state
    read_client: MockSuiReadClient,
    current_epoch: Epoch,
}

impl MockContractClient {
    /// Construct a [`MockContractClient`] with a provided [`MockSuiReadClient`].
    pub fn new(current_epoch: Epoch, read_client: MockSuiReadClient) -> Self {
        Self {
            read_client,
            current_epoch,
        }
    }
}

impl ContractClient for MockContractClient {
    async fn reserve_space(
        &self,
        encoded_size: u64,
        epochs_ahead: u64,
    ) -> SuiClientResult<StorageResource> {
        Ok(StorageResource {
            id: ObjectID::random(),
            start_epoch: self.current_epoch,
            end_epoch: self.current_epoch + epochs_ahead,
            storage_size: encoded_size,
        })
    }

    async fn register_blob(
        &self,
        storage: &StorageResource,
        blob_id: BlobId,
        _root_digest: [u8; DIGEST_LEN],
        blob_size: u64,
        erasure_code_type: EncodingType,
    ) -> SuiClientResult<Blob> {
        self.read_client.add_event(
            BlobRegistered {
                epoch: self.current_epoch,
                blob_id,
                size: blob_size,
                erasure_code_type,
                end_epoch: storage.end_epoch,
                event_id: event_id_for_testing(),
            }
            .into(),
        );
        Ok(Blob {
            id: ObjectID::random(),
            stored_epoch: self.current_epoch,
            blob_id,
            size: blob_size,
            erasure_code_type,
            certified_epoch: None,
            storage: storage.clone(),
        })
    }

    async fn certify_blob(
        &self,
        blob: Blob,
        _certificate: &ConfirmationCertificate,
    ) -> SuiClientResult<Blob> {
        self.read_client.add_event(
            BlobCertified {
                epoch: self.current_epoch,
                blob_id: blob.blob_id,
                end_epoch: blob.storage.end_epoch,
                event_id: event_id_for_testing(),
            }
            .into(),
        );
        let mut blob = blob.clone();
        blob.certified_epoch = Some(self.current_epoch);
        Ok(blob)
    }

    async fn invalidate_blob_id(
        &self,
        certificate: &InvalidBlobCertificate,
    ) -> SuiClientResult<()> {
        let msg: InvalidBlobIdMsg = bcs::from_bytes(&certificate.serialized_message)
            .map_err(|_| anyhow!("could not deserialize invalid blob message"))?;
        self.read_client.add_event(
            InvalidBlobId {
                epoch: msg.as_ref().epoch(),
                blob_id: *msg.as_ref().contents(),
                event_id: event_id_for_testing(),
            }
            .into(),
        );
        Ok(())
    }

    fn read_client(&self) -> &impl ReadClient {
        &self.read_client
    }
}

fn system_object_from_committee(committee: Committee) -> SystemObject {
    SystemObject {
        id: ObjectID::from_single_byte(42),
        current_committee: committee,
        epoch_status: EpochStatus::Done,
        total_capacity_size: 1_000_000_000_000_000,
        used_capacity_size: 0,
        price_per_unit_size: 10,
        past_committees_object: ObjectID::from_single_byte(37),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use anyhow::bail;
    use fastcrypto::bls12381::min_pk::BLS12381AggregateSignature;

    use super::*;

    #[tokio::test]
    async fn test_register_mock_clients() -> anyhow::Result<()> {
        let read_client = MockSuiReadClient::new_with_blob_ids([], None);
        // Pass a clone of `read_client` to test that events are replicated between clones
        let walrus_client = MockContractClient::new(0, read_client.clone());

        // Get event streams for the events
        let polling_duration = std::time::Duration::from_millis(1);
        let mut events = pin!(read_client.blob_events(polling_duration, None).await?);

        let resource_size = 10_000_000;
        let size = 10_000;
        let storage_resource = walrus_client.reserve_space(resource_size, 3).await?;
        assert_eq!(storage_resource.start_epoch, 0);
        assert_eq!(storage_resource.end_epoch, 3);
        assert_eq!(storage_resource.storage_size, resource_size);
        #[rustfmt::skip]
        let blob_id = BlobId([
            1, 2, 3, 4, 5, 6, 7, 8,
            1, 2, 3, 4, 5, 6, 7, 8,
            1, 2, 3, 4, 5, 6, 7, 8,
            1, 2, 3, 4, 5, 6, 7, 8,
        ]);
        let blob_obj = walrus_client
            .register_blob(
                &storage_resource,
                blob_id,
                [0; 32],
                size,
                EncodingType::RedStuff,
            )
            .await?;
        assert_eq!(blob_obj.blob_id, blob_id);
        assert_eq!(blob_obj.size, size);
        assert_eq!(blob_obj.certified_epoch, None);
        assert_eq!(blob_obj.storage, storage_resource);
        assert_eq!(blob_obj.stored_epoch, 0);

        // Make sure that we got the expected event
        let BlobEvent::Registered(blob_registered) = events.next().await.unwrap() else {
            bail!("unexpected event type");
        };
        assert_eq!(blob_registered.blob_id, blob_id);
        assert_eq!(blob_registered.epoch, blob_obj.stored_epoch);
        assert_eq!(
            blob_registered.erasure_code_type,
            blob_obj.erasure_code_type
        );
        assert_eq!(blob_registered.end_epoch, storage_resource.end_epoch);
        assert_eq!(blob_registered.size, blob_obj.size);

        let blob_obj = walrus_client
            .certify_blob(
                blob_obj,
                // Dummy certificate, currently not checked by the mock client
                &ConfirmationCertificate::new(
                    vec![],
                    vec![],
                    BLS12381AggregateSignature::default(),
                ),
            )
            .await?;
        assert_eq!(blob_obj.certified_epoch, Some(0));

        // Make sure that we got the expected event
        let BlobEvent::Certified(blob_certified) = events.next().await.unwrap() else {
            bail!("unexpected event type");
        };
        assert_eq!(blob_certified.blob_id, blob_id);
        assert_eq!(Some(blob_registered.epoch), blob_obj.certified_epoch);
        assert_eq!(blob_certified.end_epoch, storage_resource.end_epoch);

        // Get new event stream to check if we receive previous events
        let mut events = pin!(read_client.blob_events(polling_duration, None).await?);

        // Make sure that we got the expected event
        let blob_event = events.next().await.unwrap();
        assert_eq!(blob_event.blob_id(), blob_id);

        Ok(())
    }
}
