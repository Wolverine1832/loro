//! [LogStore] stores all the [Change]s and [Op]s. It's also a [DAG][crate::dag];
//!
//!
mod encoding;
mod import;
mod iter;

use crate::{version::Frontiers, LoroValue};
pub(crate) use encoding::{decode_oplog, encode_oplog};
pub use encoding::{EncodeMode, LoroEncoder};
pub(crate) use import::ImportContext;
use std::{
    cmp::Ordering,
    marker::PhantomPinned,
    sync::{atomic::AtomicBool, Arc, Mutex, MutexGuard, RwLock, Weak},
};

use fxhash::FxHashMap;

use rle::{HasLength, RleVec, Sliceable};

use crate::{
    change::Change,
    configure::Configure,
    container::{
        registry::{ContainerIdx, ContainerInstance, ContainerRegistry},
        ContainerID,
    },
    dag::Dag,
    id::{Counter, PeerID},
    op::RemoteOp,
    span::{HasCounterSpan, HasIdSpan, IdSpan},
    ContainerType, Lamport, Op, Timestamp, VersionVector, ID,
};

// use self::import::ChangeWithNegStartCounter;

const _YEAR: u64 = 365 * 24 * 60 * 60;
const MONTH: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy)]
pub struct GcConfig {
    pub gc: bool,
    pub snapshot_interval: u64,
}

impl Default for GcConfig {
    fn default() -> Self {
        GcConfig {
            gc: true,
            snapshot_interval: 6 * MONTH,
        }
    }
}

impl GcConfig {
    #[inline(always)]
    pub fn with_gc(self, gc: bool) -> Self {
        Self { gc, ..self }
    }
}

pub(crate) type ClientChanges = FxHashMap<PeerID, RleVec<[Change; 0]>>;
pub(crate) type RemoteClientChanges<'a> = FxHashMap<PeerID, Vec<Change<RemoteOp<'a>>>>;

#[derive(Debug)]
/// LogStore stores the full history of Loro
///
/// This is a self-referential structure. So it need to be pinned.
///
/// `frontier`s are the Changes without children in the DAG (there is no dep pointing to them)
///
/// TODO: Refactor we need to move the things about the current state out of LogStore (container, latest_lamport, ..)
pub struct LogStore {
    changes: ClientChanges,
    vv: VersionVector,
    cfg: Configure,
    latest_lamport: Lamport,
    latest_timestamp: Timestamp,
    frontiers: Frontiers,
    pub(crate) this_client_id: PeerID,
    /// CRDT container manager
    pub(crate) reg: ContainerRegistry,
    pending_changes: RemoteClientChanges<'static>,
    /// if local ops are not exposed yet, new ops can be merged to the existing change
    can_merge_local_op: AtomicBool,
    _pin: PhantomPinned,
}

type ContainerGuard<'a> = MutexGuard<'a, ContainerInstance>;

impl LogStore {
    pub(crate) fn new(cfg: Configure, client_id: Option<PeerID>) -> Arc<RwLock<Self>> {
        let this_client_id = client_id.unwrap_or_else(|| cfg.rand.next_u64());
        Arc::new(RwLock::new(Self {
            cfg,
            this_client_id,
            changes: FxHashMap::default(),
            latest_lamport: 0,
            latest_timestamp: 0,
            frontiers: Default::default(),
            vv: Default::default(),
            reg: ContainerRegistry::new(),
            pending_changes: Default::default(),
            can_merge_local_op: AtomicBool::new(true),
            _pin: PhantomPinned,
        }))
    }

    #[inline]
    pub fn lookup_change(&self, id: ID) -> Option<&Change> {
        if id.peer == self.this_client_id {
            self.expose_local_change();
        }
        self.changes.get(&id.peer).and_then(|changes| {
            if id.counter <= changes.last().unwrap().id_last().counter {
                Some(changes.get_by_atom_index(id.counter).unwrap().element)
            } else {
                None
            }
        })
    }

    pub fn export(
        &self,
        remote_vv: &VersionVector,
    ) -> FxHashMap<PeerID, Vec<Change<RemoteOp<'static>>>> {
        self.expose_local_change();
        let mut ans: FxHashMap<PeerID, Vec<Change<RemoteOp<'static>>>> = Default::default();
        let self_vv = self.vv();
        for span in self_vv.sub_iter(remote_vv) {
            let changes = self.get_changes_slice(span.id_span());
            for change in changes.iter() {
                let vec = ans.entry(change.id.peer).or_insert_with(Vec::new);
                vec.push(self.change_to_export_format(change));
            }
        }

        ans
    }

    pub fn expose_local_change(&self) {
        self.can_merge_local_op
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn get_changes_slice(&self, id_span: IdSpan) -> Vec<Change> {
        if let Some(changes) = self.changes.get(&id_span.client_id) {
            let mut ans = Vec::with_capacity(id_span.atom_len() / 30);
            for change in changes.slice_iter(id_span.counter.min(), id_span.counter.norm_end()) {
                let change = change.value.slice(change.start, change.end);
                ans.push(change);
            }

            ans
        } else {
            vec![]
        }
    }

    fn change_to_imported_format(
        &mut self,
        change: &Change<RemoteOp>,
        containers: &mut FxHashMap<ContainerID, ContainerGuard>,
    ) -> Change {
        let mut new_ops = RleVec::new();
        for op in change.ops.iter() {
            let container = containers.get_mut(&op.container).unwrap();
            let container_idx = self.get_container_idx(&op.container).unwrap();
            for op in op.clone().convert(container, container_idx) {
                new_ops.push(op);
            }
        }

        Change {
            ops: new_ops,
            deps: change.deps.clone(),
            id: change.id,
            lamport: change.lamport,
            timestamp: change.timestamp,
        }
    }

    pub(crate) fn change_to_export_format(&self, change: &Change) -> Change<RemoteOp<'static>> {
        let mut ops = RleVec::new();
        for op in change.ops.iter() {
            ops.push(self.to_remote_op(op));
        }
        Change {
            ops,
            deps: change.deps.clone(),
            id: change.id,
            lamport: change.lamport,
            timestamp: change.timestamp,
        }
    }

    fn to_remote_op(&self, op: &Op) -> RemoteOp<'static> {
        let container = self.reg.get_by_idx(&op.container).unwrap();
        let container = container.upgrade().unwrap();
        let mut container = container.try_lock().unwrap();
        op.clone()
            .convert(&mut container, self.cfg.gc.gc)
            .into_static()
    }

    pub(crate) fn create_container(
        &mut self,
        container_type: ContainerType,
    ) -> (ContainerID, ContainerIdx) {
        let id = self.next_id();
        let container_id = ContainerID::new_normal(id, container_type);
        let idx = self.reg.register(&container_id);
        (container_id, idx)
    }

    #[inline(always)]
    pub fn next_lamport(&self) -> Lamport {
        self.latest_lamport + 1
    }

    #[inline(always)]
    pub fn next_id(&self) -> ID {
        ID {
            peer: self.this_client_id,
            counter: self.get_next_counter(self.this_client_id),
        }
    }

    #[inline(always)]
    pub fn next_id_for(&self, client: PeerID) -> ID {
        ID {
            peer: client,
            counter: self.get_next_counter(client),
        }
    }

    #[inline(always)]
    pub fn this_client_id(&self) -> PeerID {
        self.this_client_id
    }

    #[inline(always)]
    pub fn frontiers(&self) -> &Frontiers {
        &self.frontiers
    }

    pub fn cmp_frontiers(&self, frontiers: &Frontiers) -> Ordering {
        if &self.frontiers == frontiers {
            Ordering::Equal
        } else if frontiers.iter().all(|id| self.includes_id(*id)) {
            Ordering::Greater
        } else {
            Ordering::Less
        }
    }

    pub fn includes_id(&self, id: ID) -> bool {
        let Some(changes) = self.changes.get(&id.peer) else {
            return false
        };
        changes.last().unwrap().id_last().counter >= id.counter
    }

    /// this method would not get the container and apply op
    pub fn append_local_ops(&mut self, ops: &[Op]) {
        if ops.is_empty() {
            return;
        }

        let lamport = self.next_lamport();
        let timestamp = (self.cfg.get_time)();
        let id = ID {
            peer: self.this_client_id,
            counter: self.get_next_counter(self.this_client_id),
        };
        let last = ops.last().unwrap();
        let last_ctr = last.ctr_last();
        let last_id = ID::new(self.this_client_id, last_ctr);
        let change = Change {
            id,
            deps: std::mem::replace(&mut self.frontiers, Frontiers::from_id(last_id)),
            ops: ops.into(),
            lamport,
            timestamp,
        };

        self.latest_lamport = lamport + change.content_len() as u32 - 1;
        self.latest_timestamp = timestamp;
        self.vv.set_end(change.id_end());
        let can_merge = self
            .can_merge_local_op
            .load(std::sync::atomic::Ordering::Acquire);
        let changes = self.changes.entry(self.this_client_id).or_default();
        if can_merge
            && changes
                .vec()
                .last()
                .map(|x| x.can_merge_right(&change))
                .unwrap_or(false)
        {
            let last_change_ops = &mut changes.vec_mut().last_mut().unwrap().ops;
            for op in change.ops {
                last_change_ops.push(op)
            }
        } else {
            changes.push(change);
            self.can_merge_local_op
                .store(true, std::sync::atomic::Ordering::Release)
        }
    }

    #[inline]
    pub fn contains_container(&self, id: &ContainerID) -> bool {
        self.reg.contains(id)
    }

    #[inline]
    pub fn contains_container_idx(&self, id: &ContainerIdx) -> bool {
        self.reg.contains_idx(id)
    }

    #[inline]
    pub fn contains_id(&self, id: ID) -> bool {
        self.changes
            .get(&id.peer)
            .map_or(0, |changes| changes.atom_len())
            > id.counter
    }

    #[inline]
    fn get_next_counter(&self, client_id: PeerID) -> Counter {
        self.changes
            .get(&client_id)
            .map(|changes| changes.atom_len())
            .unwrap_or(0) as Counter
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn iter_client_op(&self, client_id: PeerID) -> iter::ClientOpIter<'_> {
        iter::ClientOpIter {
            change_index: 0,
            op_index: 0,
            changes: self.changes.get(&client_id),
        }
    }

    pub(crate) fn iter_ops_at_id_span(&self, id_span: IdSpan) -> iter::OpSpanIter<'_> {
        iter::OpSpanIter::new(&self.changes, id_span)
    }

    #[inline(always)]
    pub fn get_vv(&self) -> &VersionVector {
        &self.vv
    }

    pub(crate) fn gc(&mut self, gc: bool) {
        self.cfg.gc.gc = gc;
    }

    pub(crate) fn snapshot_interval(&mut self, snapshot_interval: u64) {
        self.cfg.gc.snapshot_interval = snapshot_interval;
    }

    #[cfg(feature = "test_utils")]
    pub fn debug_inspect(&mut self) {
        println!(
            "LogStore:\n- Clients={}\n- Changes={}\n- Ops={}\n- Atoms={}",
            self.changes.len(),
            self.changes
                .values()
                .map(|v| format!("{}", v.len()))
                .collect::<Vec<_>>()
                .join(", "),
            self.changes
                .values()
                .map(|v| format!("{}", v.iter().map(|x| x.ops.len()).sum::<usize>()))
                .collect::<Vec<_>>()
                .join(", "),
            self.changes
                .values()
                .map(|v| format!("{}", v.atom_len()))
                .collect::<Vec<_>>()
                .join(", "),
        );

        self.reg.debug_inspect();
    }

    // TODO: remove
    #[inline(always)]
    pub(crate) fn get_container_idx(&self, container: &ContainerID) -> Option<ContainerIdx> {
        self.reg.get_idx(container)
    }

    pub fn get_or_create_container(
        &mut self,
        container: &ContainerID,
    ) -> Weak<Mutex<ContainerInstance>> {
        self.reg.get_or_create(container)
    }

    #[inline(always)]
    pub fn get_container(&self, container: &ContainerID) -> Option<Weak<Mutex<ContainerInstance>>> {
        self.reg.get(container)
    }

    #[inline(always)]
    pub fn get_container_by_idx(
        &self,
        container: &ContainerIdx,
    ) -> Option<Weak<Mutex<ContainerInstance>>> {
        self.reg.get_by_idx(container)
    }

    pub fn to_json(&self) -> LoroValue {
        self.reg.to_json()
    }

    #[cfg(feature = "test_utils")]
    pub(crate) fn changes(&self) -> &ClientChanges {
        &self.changes
    }
}

impl Dag for LogStore {
    type Node = Change;

    fn get(&self, id: ID) -> Option<&Self::Node> {
        self.changes
            .get(&id.peer)
            .and_then(|x| x.get_by_atom_index(id.counter).map(|x| x.element))
    }

    fn frontier(&self) -> &[ID] {
        &self.frontiers
    }

    fn vv(&self) -> crate::VersionVector {
        self.vv.clone()
    }
}
