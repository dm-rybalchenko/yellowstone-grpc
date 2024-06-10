use {
    super::{
        consumer_group_store::ConsumerGroupStore,
        consumer_source::{ConsumerSource, FromBlockchainEvent},
        consumer_supervisor::{ConsumerSourceSupervisor, ConsumerSourceSupervisorHandle},
        leader::{
            create_leader_state_log, observe_consumer_group_state, observe_leader_changes,
            try_become_leader, ConsumerGroupLeaderNode, ConsumerGroupState, IdleState, LeaderInfo,
        },
        lock::{ConsumerLock, ConsumerLocker},
        producer_queries::ProducerQueries,
        shard_iterator::{ShardFilter, ShardIterator},
    },
    crate::scylladb::{
        etcd_utils::{lease::ManagedLease, Revision},
        types::{
            BlockchainEventType, CommitmentLevel, ConsumerGroupId, ConsumerId, ShardOffsetMap,
        },
        yellowstone_log::common::SeekLocation,
    },
    etcd_client::LeaderKey,
    futures::{
        future::{self, select_all, try_join_all, BoxFuture},
        Future, FutureExt,
    },
    scylla::Session,
    std::{
        collections::{BTreeMap, HashMap},
        convert::identity,
        net::IpAddr,
        pin::Pin,
        sync::Arc,
        time::Duration,
    },
    tokio::{
        sync::{mpsc, oneshot, watch},
        task::JoinHandle,
    },
    tracing::{error, info, warn},
};

pub struct ConsumerGroupCoordinatorBackend {
    rx: mpsc::Receiver<CoordinatorCommand>,
    etcd: etcd_client::Client,
    session: Arc<Session>,
    instance_locker: ConsumerLocker,
    consumer_group_store: ConsumerGroupStore,
    producer_queries: ProducerQueries,
    leader_ifname: String,

    background_leader_attempt: BTreeMap<ConsumerGroupId, ElectionHandle>,

    leader_handles: BTreeMap<ConsumerGroupId, LeaderHandle>,
    consumer_handles: Vec<ConsumerSourceSupervisorHandle>,
    leader_election_watch_map: HashMap<ConsumerGroupId, watch::Receiver<Option<LeaderInfo>>>,
    leader_state_watch_map:
        HashMap<ConsumerGroupId, watch::Receiver<(Revision, ConsumerGroupState)>>,
}

pub struct JoinGroupArgs {
    consumer_group_id: ConsumerGroupId,
    consumer_id: ConsumerId,
    filter: Option<ShardFilter>,
}

pub struct JoinPermit {
    coordinator_callback: oneshot::Sender<ConsumerSourceSupervisorHandle>,
    supervisor: ConsumerSourceSupervisor,
    filter: Option<ShardFilter>,
}

struct LeaderHandle {
    consumer_group_id: ConsumerGroupId,
    tx_terminate: oneshot::Sender<()>,
    inner: JoinHandle<anyhow::Result<()>>,
}

impl LeaderHandle {
    fn kill(self) -> () {
        self.inner.abort();
    }
}

type ElectionResult = anyhow::Result<Option<(LeaderKey, ManagedLease)>>;

/// ElectionFuture wraps a future that attempt to become a consumer group leader while remembering the consumer group id.
struct ElectionHandle {
    consumer_group_id: ConsumerGroupId,
    inner: JoinHandle<ElectionResult>,
}

impl ElectionHandle {
    fn wrap(consumer_group_id: ConsumerGroupId, handle: JoinHandle<ElectionResult>) -> Self {
        ElectionHandle {
            consumer_group_id,
            inner: handle,
        }
    }
}

impl Future for ElectionHandle {
    type Output = (ConsumerGroupId, ElectionResult);

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let consumer_group_id = self.consumer_group_id.clone();
        self.inner.poll_unpin(cx).map(|res| {
            let folded_res = res.map_err(anyhow::Error::new).and_then(identity);
            (consumer_group_id, folded_res)
        })
    }
}

impl Future for LeaderHandle {
    type Output = (ConsumerGroupId, anyhow::Result<()>);

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.poll_unpin(cx).map(|res| {
            let folded_res = res.map_err(anyhow::Error::new).and_then(identity);
            (self.consumer_group_id.clone(), folded_res)
        })
    }
}

///
/// Using JoinPermit we can postpone the need to carry generic type `T` so we don't have to add `T` in
/// `CoordinatorCommand` enum.
///  
impl JoinPermit {
    pub async fn spawn<T: FromBlockchainEvent + Send + 'static>(
        self,
        sink: mpsc::Sender<T>,
    ) -> anyhow::Result<()> {
        let filter = self.filter;
        let handle = self
            .supervisor
            .spawn_with(move |ctx| {
                let sink2 = sink.clone();
                let filter2 = filter.to_owned();
                async move {
                    let mut shard_offset_map_by_ev_types = BTreeMap::new();
                    for ev_type in ctx.subscribed_event_types.iter().cloned() {
                        let (_revision, shard_offset_map) =
                            ctx.get_shard_offset_map(ev_type).await?;
                        shard_offset_map_by_ev_types.insert(ev_type, shard_offset_map);
                    }

                    ConsumerSource::new(ctx, shard_offset_map_by_ev_types, sink2, None, filter2)
                        .await
                }
                .boxed()
            })
            .await?;
        self.coordinator_callback
            .send(handle)
            .map_err(|_| anyhow::anyhow!("failed to grap supervisor handle"))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct CreateConsumerGroupArgs {
    seek_loc: SeekLocation,
    subscribed_blockchain_events: Vec<BlockchainEventType>,
    commitment_level: CommitmentLevel,
    consumer_id_list: Vec<ConsumerId>,
    requestor_ipaddr: Option<IpAddr>,
}

type CommandCallback<T> = oneshot::Sender<T>;

pub enum CoordinatorCommand {
    CreateConsumerGroup(
        CreateConsumerGroupArgs,
        CommandCallback<anyhow::Result<ConsumerGroupId>>,
    ),
    JoinGroup(JoinGroupArgs, CommandCallback<anyhow::Result<JoinPermit>>),
}

#[derive(Clone)]
pub struct ConsumerGroupCoordinator {
    sender: mpsc::Sender<CoordinatorCommand>,
}

impl ConsumerGroupCoordinator {
    pub async fn create_consumer_group(
        &self,
        seek_loc: SeekLocation,
        subscribed_blockchain_events: Vec<BlockchainEventType>,
        consumer_id_list: Vec<ConsumerId>,
        commitment_level: CommitmentLevel,
        requestor_ipaddr: Option<IpAddr>,
    ) -> anyhow::Result<ConsumerGroupId> {
        let args = CreateConsumerGroupArgs {
            seek_loc,
            subscribed_blockchain_events,
            commitment_level,
            consumer_id_list,
            requestor_ipaddr,
        };

        let (tx, rx) = oneshot::channel();

        if let Err(e) = self
            .sender
            .send(CoordinatorCommand::CreateConsumerGroup(args, tx))
            .await
        {
            panic!("ConsumerGroupCoordinatorBackend channel is closed")
        }
        rx.await?
    }

    pub async fn try_join_consumer_group<T: FromBlockchainEvent + Send + 'static>(
        &self,
        consumer_group_id: ConsumerGroupId,
        consumer_id: ConsumerId,
        filter: Option<ShardFilter>,
        sink: mpsc::Sender<T>,
    ) -> anyhow::Result<()> {
        let args = JoinGroupArgs {
            consumer_group_id,
            consumer_id,
            filter,
        };
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(CoordinatorCommand::JoinGroup(args, tx))
            .await?;

        let join_permit = rx.await??;

        join_permit.spawn(sink).await
    }
}

impl ConsumerGroupCoordinatorBackend {
    pub fn spawn(
        etcd: etcd_client::Client,
        session: Arc<Session>,
        consumer_group_store: ConsumerGroupStore,
        producer_queries: ProducerQueries,
        leader_ifname: String,
    ) -> (ConsumerGroupCoordinator, JoinHandle<anyhow::Result<()>>) {
        let (tx, rx) = mpsc::channel(10);
        let mut backend = ConsumerGroupCoordinatorBackend {
            rx,
            etcd: etcd.clone(),
            session: Arc::clone(&session),
            instance_locker: ConsumerLocker(etcd.clone()),
            consumer_group_store: consumer_group_store,
            producer_queries: producer_queries,
            leader_ifname,
            background_leader_attempt: Default::default(),
            leader_handles: Default::default(),
            consumer_handles: Default::default(),
            leader_election_watch_map: Default::default(),
            leader_state_watch_map: Default::default(),
        };

        let h = tokio::spawn(async move { backend.run().await });

        (ConsumerGroupCoordinator { sender: tx }, h)
    }

    fn try_become_leader_bg(&mut self, consumer_group_id: ConsumerGroupId) {
        let etcd = self.etcd.clone();
        let leader_ifname = self.leader_ifname.clone();
        let fut = async move {
            try_become_leader(
                etcd,
                consumer_group_id,
                Duration::from_secs(5),
                leader_ifname,
            )
            .await
        };

        self.background_leader_attempt
            .entry(consumer_group_id)
            .or_insert_with(|| ElectionHandle::wrap(consumer_group_id, tokio::spawn(fut)));
    }

    async fn get_leader_state_watch(
        &mut self,
        consumer_group_id: ConsumerGroupId,
    ) -> anyhow::Result<watch::Receiver<(i64, ConsumerGroupState)>> {
        match self.leader_state_watch_map.entry(consumer_group_id.clone()) {
            std::collections::hash_map::Entry::Occupied(o) => Ok(o.get().clone()),
            std::collections::hash_map::Entry::Vacant(v) => {
                let watch = observe_consumer_group_state(&self.etcd, consumer_group_id).await?;
                Ok(v.insert(watch).clone())
            }
        }
    }

    async fn get_leader_election_watch(
        &mut self,
        consumer_group_id: ConsumerGroupId,
    ) -> anyhow::Result<watch::Receiver<Option<LeaderInfo>>> {
        match self
            .leader_election_watch_map
            .entry(consumer_group_id.clone())
        {
            std::collections::hash_map::Entry::Occupied(o) => Ok(o.get().clone()),
            std::collections::hash_map::Entry::Vacant(v) => {
                let watch = observe_leader_changes(&self.etcd, consumer_group_id).await?;
                Ok(v.insert(watch).clone())
            }
        }
    }

    fn register_consumer_handle(&mut self, consumer_handle: ConsumerSourceSupervisorHandle) {
        self.consumer_handles.push(consumer_handle);
    }

    async fn try_spawn_consumer_member(
        &mut self,
        join_args: JoinGroupArgs,
    ) -> anyhow::Result<(
        oneshot::Receiver<ConsumerSourceSupervisorHandle>,
        JoinPermit,
    )> {
        let consumer_group_id = join_args.consumer_group_id;
        let consumer_id = &join_args.consumer_id;
        let consumer_lock = self
            .instance_locker
            .try_lock_instance_id(consumer_group_id, consumer_id)
            .await?;

        let mut leader_election_watch = self.get_leader_election_watch(consumer_group_id).await?;

        let mut leader_state_watch = self.get_leader_state_watch(consumer_group_id).await?;

        leader_election_watch.mark_changed();
        leader_state_watch.mark_changed();

        let maybe_leader_info = { leader_election_watch.borrow_and_update().to_owned() };

        if maybe_leader_info.is_none() {
            info!("will attempt to be come leader for consumer group {consumer_group_id:?}");
            self.try_become_leader_bg(consumer_group_id);
        }

        let supervisor = ConsumerSourceSupervisor::new(
            consumer_lock,
            self.etcd.clone(),
            Arc::clone(&self.session),
            self.consumer_group_store.clone(),
            leader_state_watch.clone(),
        );

        let (tx, rx) = oneshot::channel();
        let permit = JoinPermit {
            coordinator_callback: tx,
            supervisor,
            filter: join_args.filter,
        };
        Ok((rx, permit))
    }

    async fn create_consumer_group(
        &self,
        args: CreateConsumerGroupArgs,
    ) -> anyhow::Result<ConsumerGroupId> {
        let consumer_group_info = self
            .consumer_group_store
            .create_static_consumer_group(
                &args.consumer_id_list,
                args.commitment_level,
                &args.subscribed_blockchain_events,
                args.seek_loc,
                args.requestor_ipaddr,
            )
            .await
            .map_err(|e| {
                error!("create_static_consumer_group: {e:?}");
                tonic::Status::internal("failed to create consumer group")
            })?;

        create_leader_state_log(&self.etcd, &consumer_group_info)
            .await
            .map_err(|e| {
                error!("create_static_consumer_group: {e:?}");
                tonic::Status::internal("failed to create consumer group")
            })?;

        Ok(consumer_group_info.consumer_group_id)
    }

    async fn interpret_command(&mut self, cmd: CoordinatorCommand) -> anyhow::Result<()> {
        match cmd {
            CoordinatorCommand::JoinGroup(args, callback) => {
                info!("processing JoinGroup command");
                let result = self.try_spawn_consumer_member(args).await;

                match result {
                    Ok((rx, permit)) => {
                        if callback.send(Ok(permit)).is_err() {
                            warn!("JoinGroup finished before client exerced its join permission");
                        }
                        if let Ok(consumer_handle) = rx.await {
                            self.register_consumer_handle(consumer_handle);
                        }
                    }
                    Err(e) => {
                        let _ = callback.send(Err(e));
                    }
                }
                Ok(())
            }
            CoordinatorCommand::CreateConsumerGroup(args, callback) => {
                info!("processing CreateConsumerGroup command");
                let result = self.create_consumer_group(args).await;
                let _ = callback.send(result);
                Ok(())
            }
        }
    }

    fn create_leader_node(
        &mut self,
        consumer_group_id: ConsumerGroupId,
        leader_key: LeaderKey,
        leader_lease: ManagedLease,
    ) {
        let etcd = self.etcd.clone();
        let consumer_group_store = self.consumer_group_store.clone();
        let producer_queries = self.producer_queries.clone();

        let (tx, rx) = oneshot::channel();
        let h = tokio::spawn(async move {
            let mut leader = ConsumerGroupLeaderNode::new(
                etcd,
                leader_key,
                leader_lease,
                consumer_group_store,
                producer_queries,
            )
            .await?;

            leader.leader_loop(rx).await
        });

        let leader_handle = LeaderHandle {
            consumer_group_id: consumer_group_id.clone(),
            tx_terminate: tx,
            inner: h,
        };

        let maybe_old_leader_handle = self.leader_handles.insert(consumer_group_id, leader_handle);
        if let Some(old_leader_handle) = maybe_old_leader_handle {
            old_leader_handle.kill();
        };
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let wait_for_election_result = if !self.background_leader_attempt.is_empty() {
                let iter = self
                    .background_leader_attempt
                    .values_mut()
                    .map(|h| Pin::new(h));
                select_all(iter).boxed()
            } else {
                future::pending().boxed()
            };

            let wait_for_consumer_to_quit = if !self.consumer_handles.is_empty() {
                let iter = self.consumer_handles.iter_mut().map(|h| Pin::new(h));
                select_all(iter).boxed()
            } else {
                future::pending().boxed()
            };

            let wait_for_leader_to_quit = if !self.leader_handles.is_empty() {
                select_all(self.leader_handles.values_mut().map(|v| Pin::new(v))).boxed()
            } else {
                future::pending().boxed()
            };

            tokio::select! {
                Some(cmd) = self.rx.recv() => {
                    info!("receive a command");
                    self.interpret_command(cmd).await?;
                },
                ((cg_id, result), _, _) = wait_for_election_result => {
                    self.background_leader_attempt.remove(&cg_id);
                    let cg_id_text = String::from_utf8(cg_id.to_vec())?;
                    match result {
                        Ok(Some((leader_key, leader_lease))) => {
                            info!("won leader election for cg-{cg_id_text}");
                            self.create_leader_node(cg_id, leader_key, leader_lease);
                        },
                        Ok(None) => warn!("attempt to be leader failed"),
                        Err(e) => warn!("a leader attempt failed with: {e:?}"),
                    }
                }
                (result, i, _remaining_futs) = wait_for_consumer_to_quit => {
                    let resolved_handle = self.consumer_handles.remove(i);
                    if let Err(supervisor_error) = result? {
                        error!("supervisor failed with : {supervisor_error:?}");
                    }
                    info!("group={}, instance={} finished", String::from_utf8(resolved_handle.consumer_group_id.to_vec())?, resolved_handle.consumer_id);
                },
                ((cg_id, result), _, _) = wait_for_leader_to_quit => {
                    let cg_id_text = String::from_utf8(cg_id.to_vec())?;
                    match result {
                        Ok(_) => info!("leader {cg_id_text} closed gracefully"),
                        Err(e) => error!("leader {cg_id_text}a "),
                    }
                }
            }
        }
    }
}

// pub async fn build_consumer_source<T: FromBlockchainEvent + Send + 'static>(
//     consumer_id: ConsumerId,
//     state: &IdleState,
//     session: Arc<Session>,
//     instance_lock: &ConsumerLock,
//     shard_offsets: ShardOffsetMap,
//     acc_update_filter: Option<ShardFilter>,
//     sink: mpsc::Sender<T>,
// ) -> anyhow::Result<ConsumerSource<T>> {

//     let mut shard_iterators = Vec::with_capacity(64);
//     for ev_type in state
//         .header
//         .subscribed_blockchain_event_types
//         .iter()
//         .cloned()
//     {
//         let shard_iterator_subset = try_join_all(shard_offsets.iter().map(
//             |(shard_id, (offset, _slot))| {
//                 ShardIterator::new(
//                     Arc::clone(&session),
//                     state.producer_id,
//                     shard_id,
//                     offset,
//                     ev_type,
//                     acc_update_filter.clone(),
//                 )
//             },
//         ))
//         .await?;
//         shard_iterators.extend(shard_iterator_subset);
//     }

//     let consumer_source = ConsumerSource::new(
//         Arc::clone(&session),
//         state.header.consumer_group_id.clone(),
//         consumer_id,
//         state.producer_id,
//         state.execution_id,
//         state.header.subscribed_blockchain_event_types.clone(),
//         sink,
//         shard_iterators,
//         instance_lock.get_fencing_token_gen(),
//         None,
//     )
//     .await?;
//     Ok(consumer_source)
// }