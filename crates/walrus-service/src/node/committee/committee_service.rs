// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
#![allow(clippy::mutable_key_type)]

//! Committee lookup and management.

use std::{
    collections::HashMap,
    num::NonZeroU16,
    sync::{Arc, Mutex as SyncMutex},
};

use futures::TryFutureExt;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::sync::{watch, Mutex as TokioMutex};
use tower::ServiceExt as _;
use walrus_core::{
    encoding::EncodingConfig,
    ensure,
    keys::ProtocolKeyPair,
    merkle::MerkleProof,
    messages::InvalidBlobCertificate,
    metadata::VerifiedBlobMetadataWithId,
    BlobId,
    Epoch,
    InconsistencyProof as InconsistencyProofEnum,
    PublicKey,
    ShardIndex,
    Sliver,
    SliverPairIndex,
    SliverType,
};
use walrus_sui::types::Committee;

use super::{
    node_service::{NodeService, NodeServiceError, RemoteStorageNode, Request, Response},
    request_futures::{GetAndVerifyMetadata, GetInvalidBlobCertificate, RecoverSliver},
    BeginCommitteeChangeError,
    CommitteeLookupService,
    CommitteeService,
    DefaultNodeServiceFactory,
    EndCommitteeChangeError,
    NodeServiceFactory,
};
use crate::{
    common::active_committees::{
        ActiveCommittees,
        ChangeNotInProgress,
        CommitteeTracker,
        NextCommitteeAlreadySet,
        StartChangeError,
    },
    node::{config::CommitteeServiceConfig, errors::SyncShardClientError},
};

pub(crate) struct NodeCommitteeServiceBuilder<T> {
    service_factory: Box<dyn NodeServiceFactory<Service = T>>,
    local_identity: Option<PublicKey>,
    rng: StdRng,
    config: CommitteeServiceConfig,
}

impl Default for NodeCommitteeServiceBuilder<RemoteStorageNode> {
    fn default() -> Self {
        Self {
            service_factory: Box::new(DefaultNodeServiceFactory::default()),
            local_identity: None,
            rng: StdRng::seed_from_u64(rand::thread_rng().gen()),
            config: CommitteeServiceConfig::default(),
        }
    }
}

impl<T> NodeCommitteeServiceBuilder<T>
where
    T: NodeService,
{
    pub fn node_service_factory<F>(
        self,
        service_factory: F,
    ) -> NodeCommitteeServiceBuilder<F::Service>
    where
        F: NodeServiceFactory + 'static,
    {
        NodeCommitteeServiceBuilder {
            local_identity: self.local_identity,
            rng: self.rng,
            config: self.config,
            service_factory: Box::new(service_factory),
        }
    }

    pub fn local_identity(mut self, id: PublicKey) -> Self {
        self.local_identity = Some(id);
        self
    }

    pub fn config(mut self, config: CommitteeServiceConfig) -> Self {
        self.config = config;
        self
    }

    #[cfg(test)]
    pub fn randomness(mut self, rng: StdRng) -> Self {
        self.rng = rng;
        self
    }

    pub async fn build<S>(self, lookup_service: S) -> Result<NodeCommitteeService<T>, anyhow::Error>
    where
        S: CommitteeLookupService + std::fmt::Debug + 'static,
    {
        // TODO(jsmith): Allow setting the local service factory.
        let committee_tracker: CommitteeTracker =
            lookup_service.get_active_committees().await?.into();
        let encoding_config = Arc::new(EncodingConfig::new(
            committee_tracker
                .committees()
                .current_committee()
                .n_shards(),
        ));

        let inner = NodeCommitteeServiceInner::new(
            committee_tracker,
            self.service_factory,
            self.config,
            encoding_config,
            self.local_identity,
            self.rng,
        )
        .await?;

        Ok(NodeCommitteeService {
            inner,
            committee_lookup: Box::new(lookup_service),
        })
    }
}

/// Default committee service used for communicating between nodes.
///
/// Requests the current committee state using a [`CommitteeLookupService`].
pub(crate) struct NodeCommitteeService<T = RemoteStorageNode> {
    inner: NodeCommitteeServiceInner<T>,
    committee_lookup: Box<dyn super::CommitteeLookupService>,
}

impl NodeCommitteeService<RemoteStorageNode> {
    pub fn builder() -> NodeCommitteeServiceBuilder<RemoteStorageNode> {
        Default::default()
    }

    pub async fn new<S>(
        lookup_service: S,
        local_identity: PublicKey,
        config: CommitteeServiceConfig,
    ) -> Result<Self, anyhow::Error>
    where
        S: CommitteeLookupService + std::fmt::Debug + 'static,
    {
        Self::builder()
            .local_identity(local_identity)
            .config(config)
            .build(lookup_service)
            .await
    }
}

impl<T> NodeCommitteeService<T>
where
    T: NodeService,
{
    async fn sync_shard_as_of_epoch(
        &self,
        shard: ShardIndex,
        starting_blob_id: BlobId,
        sliver_count: u64,
        sliver_type: SliverType,
        current_epoch: Epoch,
        key_pair: &ProtocolKeyPair,
    ) -> Result<Vec<(BlobId, Sliver)>, SyncShardClientError> {
        let committee = self
            .inner
            .committee_tracker
            .borrow()
            .committees()
            .committee_for_epoch(current_epoch - 1)
            .ok_or(SyncShardClientError::NoSyncClient)?
            .clone();

        let node_info = committee
            .member_index_for_shard(shard)
            .map(|index| &committee.members()[index])
            .expect("shard is valid for the committee");

        let service =
            if let Some(service) = self.inner.get_node_service_by_id(&node_info.public_key) {
                service
            } else {
                // TODO(jsmith): Cache this service to avoid rebuilding.
                tracing::trace!("service is unavailable for node, recreating it");
                let mut service_factory = self.inner.service_factory.lock().await;
                service_factory
                    .make_service(node_info, &self.inner.encoding_config)
                    .await
                    .map_err(|_| SyncShardClientError::NoSyncClient)?
            };

        let slivers = service
            .oneshot(Request::SyncShardAsOfEpoch {
                shard,
                starting_blob_id,
                sliver_count,
                sliver_type,
                current_epoch,
                key_pair: key_pair.clone(),
            })
            .map_ok(Response::into_value)
            .map_err(|error| match error {
                NodeServiceError::Node(error) => SyncShardClientError::RequestError(error),
                NodeServiceError::Other(other) => anyhow::anyhow!(other).into(),
            })
            .await?;

        Ok(slivers)
    }

    async fn begin_committee_change_to(
        &self,
        next_committee: Committee,
    ) -> Result<(), BeginCommitteeChangeError> {
        let mut service_factory = self.inner.service_factory.lock().await;

        // Begin by creating the needed services, placing them into a temporary map.
        // This allows us to keep the critical section where we modify the active committee and
        // service set that is being used, small.
        let new_services = create_services_from_committee(
            &mut service_factory,
            &next_committee,
            &self.inner.encoding_config,
        )
        .await
        .map_err(BeginCommitteeChangeError::AllServicesFailed)?;

        let mut services = self
            .inner
            .services
            .lock()
            .expect("thread did not panic with mutex");

        let mut modify_result = Ok(());
        let modify_tracker = |tracker: &mut CommitteeTracker| {
            // Guaranteed by the caller.
            assert_eq!(tracker.next_epoch(), next_committee.epoch);
            if let Err(NextCommitteeAlreadySet(next_committee)) =
                tracker.set_committee_for_next_epoch(next_committee)
            {
                let stored_next_committee = tracker
                    .committees()
                    .next_committee()
                    .expect("committee is already set");
                assert_eq!(
                    next_committee, **stored_next_committee,
                    "committee for the next epoch cannot change after being fetched"
                );
            }

            modify_result = tracker.start_change().map_err(|error| match error {
                StartChangeError::UnknownNextCommittee => unreachable!("committee set above"),
                StartChangeError::ChangeInProgress => {
                    BeginCommitteeChangeError::ChangeAlreadyInProgress
                }
            });

            modify_result.is_ok()
        };

        self.inner
            .committee_tracker
            .send_if_modified(modify_tracker);

        if let Err(error) = modify_result {
            Err(error)
        } else {
            services.extend(new_services);
            Ok(())
        }
    }

    fn end_committee_change_to(&self, epoch: Epoch) -> Result<(), EndCommitteeChangeError> {
        let current_epoch = self.get_epoch();

        ensure!(
            epoch <= current_epoch,
            EndCommitteeChangeError::ProvidedEpochIsInTheFuture {
                provided: epoch,
                expected: current_epoch,
            }
        );
        ensure!(
            epoch >= current_epoch,
            EndCommitteeChangeError::ProvidedEpochIsInThePast {
                provided: epoch,
                expected: current_epoch,
            }
        );
        debug_assert_eq!(epoch, current_epoch);

        let mut services = self
            .inner
            .services
            .lock()
            .expect("thread did not panic with mutex");

        let mut maybe_committees = None;
        self.inner.committee_tracker.send_if_modified(|tracker| {
            let current = tracker.committees().current_committee().clone();
            maybe_committees = match tracker.end_change() {
                Ok(outgoing) => Some((outgoing.clone(), current)),
                Err(ChangeNotInProgress) => None,
            };
            maybe_committees.is_some()
        });

        let (outgoing_committee, current_committee) =
            maybe_committees.ok_or(EndCommitteeChangeError::EpochChangeAlreadyDone)?;

        // We already added services for the new committee members, which may have overlapped with
        // the old. Only remove those services corresponding to members in the old committee that
        // are not in the new.
        for outgoing_member in outgoing_committee.members() {
            if !current_committee.contains(&outgoing_member.public_key) {
                services.remove(&outgoing_member.public_key);
            }
        }

        Ok(())
    }
}

pub(super) struct NodeCommitteeServiceInner<T> {
    /// The set of active committees, which can be observed for changes.
    pub committee_tracker: watch::Sender<CommitteeTracker>,
    /// Services for members of the active read and write committees.
    pub services: SyncMutex<HashMap<PublicKey, T>>,
    /// Timeouts and other configuration for requests.
    pub config: CommitteeServiceConfig,
    /// System wide encoding parameters
    pub encoding_config: Arc<EncodingConfig>,
    /// Shared randomness.
    pub rng: SyncMutex<StdRng>,
    /// The identity of the local storage node within and across committees.
    local_identity: Option<PublicKey>,
    /// Function used to construct new services.
    service_factory: TokioMutex<Box<dyn NodeServiceFactory<Service = T>>>,
}

impl<T> NodeCommitteeServiceInner<T>
where
    T: NodeService,
{
    pub async fn new(
        committee_tracker: CommitteeTracker,
        mut service_factory: Box<dyn NodeServiceFactory<Service = T>>,
        config: CommitteeServiceConfig,
        encoding_config: Arc<EncodingConfig>,
        local_identity: Option<PublicKey>,
        rng: StdRng,
    ) -> Result<Self, anyhow::Error> {
        let committees = committee_tracker.committees();
        let mut services = create_services_from_committee(
            &mut service_factory,
            committees.current_committee(),
            &encoding_config,
        )
        .await?;
        add_members_from_committee(
            &mut services,
            &mut service_factory,
            committees.current_committee(),
            &encoding_config,
        )
        .await?;

        let this = Self {
            committee_tracker: watch::Sender::new(committee_tracker),
            services: SyncMutex::new(services),
            service_factory: TokioMutex::new(service_factory),
            local_identity,
            config,
            rng: SyncMutex::new(rng),
            encoding_config,
        };

        Ok(this)
    }

    pub(super) fn is_local(&self, id: &PublicKey) -> bool {
        self.local_identity
            .as_ref()
            .map(|key| key == id)
            .unwrap_or(false)
    }

    pub(super) fn get_node_service_by_id(&self, id: &PublicKey) -> Option<T> {
        self.services
            .lock()
            .expect("thread did not panic with mutex")
            .get(id)
            .cloned()
    }

    pub(super) fn subscribe_to_committee_changes(&self) -> watch::Receiver<CommitteeTracker> {
        self.committee_tracker.subscribe()
    }
}

#[async_trait::async_trait]
impl<T> CommitteeService for NodeCommitteeService<T>
where
    T: NodeService,
{
    fn get_epoch(&self) -> Epoch {
        self.inner.committee_tracker.borrow().committees().epoch()
    }

    fn get_shard_count(&self) -> NonZeroU16 {
        self.inner.encoding_config.n_shards()
    }

    fn encoding_config(&self) -> &Arc<EncodingConfig> {
        &self.inner.encoding_config
    }

    fn committee(&self) -> Arc<Committee> {
        self.inner
            .committee_tracker
            .borrow()
            .committees()
            .current_committee()
            .clone()
    }

    fn active_committees(&self) -> ActiveCommittees {
        self.inner.committee_tracker.borrow().committees().clone()
    }

    #[tracing::instrument(name = "get_and_verify_metadata committee", skip_all)]
    async fn get_and_verify_metadata(
        &self,
        blob_id: BlobId,
        certified_epoch: Epoch,
    ) -> VerifiedBlobMetadataWithId {
        GetAndVerifyMetadata::new(blob_id, certified_epoch, &self.inner)
            .run()
            .await
    }

    #[tracing::instrument(name = "recover_sliver committee", skip_all)]
    async fn recover_sliver(
        &self,
        metadata: Arc<VerifiedBlobMetadataWithId>,
        sliver_id: SliverPairIndex,
        sliver_type: SliverType,
        certified_epoch: Epoch,
    ) -> Result<Sliver, InconsistencyProofEnum<MerkleProof>> {
        RecoverSliver::new(
            metadata,
            sliver_id,
            sliver_type,
            certified_epoch,
            &self.inner,
        )
        .run()
        .await
    }

    #[tracing::instrument(name = "get_invalid_blob_certificate committee", skip_all)]
    async fn get_invalid_blob_certificate(
        &self,
        blob_id: BlobId,
        inconsistency_proof: &InconsistencyProofEnum,
    ) -> InvalidBlobCertificate {
        tracing::trace!("creating future to get invalid blob certificate");
        GetInvalidBlobCertificate::new(blob_id, inconsistency_proof, &self.inner)
            .run()
            .await
    }

    #[tracing::instrument(name = "sync_shard_before_epoch committee", skip_all)]
    async fn sync_shard_before_epoch(
        &self,
        shard: ShardIndex,
        starting_blob_id: BlobId,
        sliver_type: SliverType,
        sliver_count: u64,
        epoch: Epoch,
        key_pair: &ProtocolKeyPair,
    ) -> Result<Vec<(BlobId, Sliver)>, SyncShardClientError> {
        self.sync_shard_as_of_epoch(
            shard,
            starting_blob_id,
            sliver_count,
            sliver_type,
            epoch,
            key_pair,
        )
        .await
    }

    fn is_walrus_storage_node(&self, public_key: &PublicKey) -> bool {
        let committee_tracker = self.inner.committee_tracker.borrow();

        committee_tracker
            .committees()
            .current_committee()
            .contains(public_key)
            || committee_tracker
                .committees()
                .previous_committee()
                .map(|committee| committee.contains(public_key))
                .unwrap_or(false)
            || committee_tracker
                .committees()
                .next_committee()
                .map(|committee| committee.contains(public_key))
                .unwrap_or(false)
    }

    async fn begin_committee_change(
        &self,
        new_epoch: Epoch,
    ) -> Result<(), BeginCommitteeChangeError> {
        let expected_next_epoch = self.inner.committee_tracker.borrow().next_epoch();

        if new_epoch > expected_next_epoch {
            return Err(BeginCommitteeChangeError::EpochIsNotSequential {
                expected: expected_next_epoch,
                actual: new_epoch,
            });
        } else if new_epoch == expected_next_epoch - 1 {
            return Err(BeginCommitteeChangeError::EpochIsTheSameAsCurrent);
        } else if new_epoch < expected_next_epoch - 1 {
            return Err(BeginCommitteeChangeError::EpochIsLess {
                expected: expected_next_epoch,
                actual: new_epoch,
            });
        }
        debug_assert_eq!(new_epoch, expected_next_epoch);

        let latest = self
            .committee_lookup
            .get_active_committees()
            .await
            .map_err(BeginCommitteeChangeError::LookupError)?;
        let current_committee: Committee = (**latest.current_committee()).clone();

        ensure!(
            current_committee.epoch == expected_next_epoch,
            BeginCommitteeChangeError::LatestCommitteeEpochDiffers {
                latest_committee: current_committee,
                expected_epoch: expected_next_epoch
            },
        );

        self.begin_committee_change_to(current_committee).await
    }

    fn end_committee_change(&self, epoch: Epoch) -> Result<(), EndCommitteeChangeError> {
        self.end_committee_change_to(epoch)
    }
}

impl<T> std::fmt::Debug for NodeCommitteeServiceInner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCommitteeServiceInner")
            .field("committee_tracker", &self.committee_tracker)
            .field("config", &self.config)
            .field("local_identity", &self.local_identity)
            .field(
                "encoding_config.n_shards",
                &self.encoding_config.n_shards().get(),
            )
            .finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for NodeCommitteeService<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCommitteeService")
            .field("inner", &self.inner)
            .field("committee_lookup", &self.committee_lookup)
            .finish_non_exhaustive()
    }
}

async fn create_services_from_committee<T: NodeService>(
    service_factory: &mut Box<dyn NodeServiceFactory<Service = T>>,
    committee: &Committee,
    encoding_config: &Arc<EncodingConfig>,
) -> Result<HashMap<PublicKey, T>, anyhow::Error> {
    let mut services = HashMap::default();
    add_members_from_committee(&mut services, service_factory, committee, encoding_config)
        .await
        .map(|_| services)
}

#[tracing::instrument(skip_all, fields(walrus.epoch = committee.epoch))]
async fn add_members_from_committee<T: NodeService>(
    services: &mut HashMap<PublicKey, T>,
    service_factory: &mut Box<dyn NodeServiceFactory<Service = T>>,
    committee: &Committee,
    encoding_config: &Arc<EncodingConfig>,
) -> Result<(), anyhow::Error> {
    let mut n_created = 0usize;

    for member in committee.members() {
        let public_key = &member.public_key;
        match service_factory.make_service(member, encoding_config).await {
            Ok(service) => {
                n_created += 1;

                if services.insert(public_key.clone(), service).is_some() {
                    tracing::debug!(
                        walrus.node.public_key = %public_key,
                        "replaced the service for a storage node"
                    );
                } else {
                    tracing::debug!(
                        walrus.node.public_key = %public_key,
                        "added a service for a storage node"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    walrus.node.public_key = %public_key, %error,
                    "failed to create service for committee member"
                );
            }
        }
    }

    walrus_core::ensure!(
        n_created != 0,
        "failed to create any service from the committee"
    );
    Ok(())
}

#[cfg(test)]
#[path = "test_committee_service.rs"]
mod tests;
