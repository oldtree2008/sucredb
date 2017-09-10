use std::time::{Instant, Duration};
use std::collections::hash_map::Entry as HMEntry;
use version_vector::*;
use storage::*;
use database::*;
use command::CommandError;
use bincode;
use inflightmap::InFlightMap;
use fabric::*;
use vnode_sync::*;
use hash::hash_slot;
use rand::{Rng, thread_rng};
use byteorder::{WriteBytesExt, ReadBytesExt, BigEndian};
use bytes::Bytes;
use utils::{IdHashMap, IdHasherBuilder, IdHashSet};

const ZOMBIE_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum VNodeStatus {
    /* TODO: consider adding an status for a node that just came back up and
        is still part of the cluster, so it potentially has highly stale data */
    // steady state
    Ready,
    // streaming data from another node, can only accept replicated writes in this state
    Bootstrap,
    // another node took over this vnode, this will stay in zombie until it times out
    // and syncs are completed, etc.
    Zombie,
    // no actual data is present
    Absent,
}

pub struct VNode {
    state: VNodeState,
    syncs: IdHashMap<Cookie, Synchronization>,
    requests: InFlightMap<Cookie, ReqState, Instant, IdHasherBuilder>,
}

pub struct VNodeState {
    num: u16,
    status: VNodeStatus,
    last_status_change: Instant,
    pub clocks: BitmappedVersionVector,
    pub logs: VNodeLogs,
    pub storage: Storage,
    // state for syncs
    pub pending_bootstrap: bool,
    pub sync_nodes: IdHashSet<NodeId>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedVNodeState {
    clocks: BitmappedVersionVector,
    clean_shutdown: bool,
}

// #[derive(Debug)]
pub struct VNodeLogs {
    // Track knowledge by physical node id instead of node id,
    // this is what we actually want to track and it also greatly
    // simplifies VNodeState::update_peers.
    // Only track continuous dots (bv base) for simplicity.
    // knowledge: IdHashMap<PhysicalNodeId, Version>,
    tail: IdHashMap<NodeId, Version>,
    storage: Storage,
}

struct ReqState {
    replies: u8,
    succesfull: u8,
    required: u8,
    total: u8,
    is_delete: bool,
    reply_result: bool,
    container: DottedCausalContainer<Bytes>,
    token: Token,
}

impl VNodeLogs {
    fn new(storage: Storage) -> VNodeLogs {
        VNodeLogs {
            // knowledge: Default::default(),
            tail: Default::default(),
            storage: storage,
        }
    }

    #[allow(unused_variables)]
    fn advance_knowledge(&mut self, node: NodeId, peer: NodeId, until: Version) {
        // match self.knowledge.entry(split_u64(peer).0) {
        //     HMEntry::Vacant(v) => {
        //         v.insert(until);
        //     }
        //     HMEntry::Occupied(mut o) => {
        //         if *o.get() < until {
        //             *o.get_mut() = until;
        //         } else {
        //             return; // don't bother to gc
        //         }
        //     }
        // }
        // self.gc();
    }

    pub fn log(&mut self, node:NodeId, version: Version, key: &[u8]) {
        let tail = self.tail.get(&node).cloned().unwrap_or(0);
        if version > tail {
            let mut buffer = [0u8; 16];
            (&mut buffer[0..8]).write_u64::<BigEndian>(node).unwrap();
            (&mut buffer[8..16]).write_u64::<BigEndian>(version).unwrap();
            self.storage.set(&buffer[..], key);
        }
    }

    pub fn iter_log<F: FnMut(Version, &[u8])>(&mut self, node: NodeId, version: Version, mut f: F) {
        let mut buffer = [0u8; 16];
        (&mut buffer[0..8]).write_u64::<BigEndian>(node).unwrap();
        (&mut buffer[8..16]).write_u64::<BigEndian>(version).unwrap();
        let mut iterator = self.storage.iterator();
        // iterator.seek(&buffer[..]);
        for (k, dot_key) in iterator.iter() {
            println!("{:?}", ::std::str::from_utf8(k));
            if k[0..8] != buffer[0..8] {
                break;
            }
            let dot = (&k[8..16]).read_u64::<BigEndian>().unwrap();
            f(dot, dot_key);
        }
    }

    pub fn get(&self, node: NodeId, version: Version) -> Option<Bytes> {
        let mut buffer = [0u8; 16];
        (&mut buffer[0..8]).write_u64::<BigEndian>(node).unwrap();
        (&mut buffer[8..16]).write_u64::<BigEndian>(version).unwrap();
        self.storage.get(&buffer[..], |x| Bytes::from(x))
    }

    pub fn clear(&mut self) {
        self.storage.clear();
        self.tail.clear();
        // self.knowledge.clear();
    }

    pub fn gc(&mut self) {
        // get the highest dot that is known by all peers
        // then only keep [highest + 1..]
        // let horizon = 1 + self.knowledge.values().cloned().min().unwrap_or(0);
        // let new_log = self.log.split_off(&horizon);
        // debug!(
        //     "gc horizon {}, {} cleaned, {} left",
        //     horizon,
        //     self.log.len(),
        //     new_log.len()
        // );
        // self.log = new_log;
    }
}

#[cfg(test)]
macro_rules! assert_any {
    ($value: expr, $($status:pat)|*) => {
        match $value {
            $($status)|* => (),
            _ => panic!("{:?} is not any of {}", $value, stringify!($($status)|*))
        }
    }
}

macro_rules! check_status {
    ($this: expr, $($status:pat)|*, $db: expr, $from: expr, $msg: expr, $emsg: ident, $col: ident) => {
        match $this.status() {
            $($status)|* => (),
            state => {
                let cookie = $msg.cookie;
                debug!("Incorrect state for {}[{:?}] expected {} was {:?}",
                    stringify!($col), cookie, stringify!($($status)|*), state);
                let _ = fabric_send_error!($db, $from, $msg, $emsg, FabricMsgError::BadVNodeStatus);
                return;
            },
        }
    }
}

macro_rules! forward {
    ($this: expr, $db: expr, $from: expr, $msg: expr, $emsg: ident, $col: ident, $f: ident) => {
        match $this.$col.entry($msg.cookie) {
            HMEntry::Occupied(mut o) => {
                o.get_mut().$f($db, &mut $this.state, $msg);
            },
            _ => {
                let _ = fabric_send_error!($db, $from, $msg, $emsg, FabricMsgError::CookieNotFound);
            }
        }
    };
    ($this: expr, $($status:pat)|*, $db: expr, $from: expr, $msg: expr, $emsg: ident, $col: ident, $f: ident) => {
        check_status!($this, $($status)|*, $db, $from, $msg, $emsg, $col);
        forward!($this, $db, $from, $msg, $emsg, $col, $f)
    };
}

impl ReqState {
    fn new(
        token: Token,
        nodes: usize,
        consistency: ConsistencyLevel,
        is_delete: bool,
        reply_result: bool,
    ) -> Self {
        ReqState {
            required: consistency.required(nodes as u8),
            total: nodes as u8,
            replies: 0,
            succesfull: 0,
            is_delete: is_delete,
            reply_result: reply_result,
            container: DottedCausalContainer::new(),
            token: token,
        }
    }

    fn done(&self) -> bool {
        self.satisfied() || self.replies == self.total
    }

    fn satisfied(&self) -> bool {
        self.succesfull >= self.required
    }
}

impl VNode {
    pub fn new(db: &Database, num: u16, status: VNodeStatus) -> VNode {
        let state = VNodeState::load(num, db, status);
        state.save(db, false);

        let mut vnode = VNode {
            state: state,
            requests: InFlightMap::new(),
            syncs: Default::default(),
        };
        if vnode.status() != VNodeStatus::Absent {
            vnode.state.update_peers(db);
        }

        match vnode.status() {
            VNodeStatus::Ready | VNodeStatus::Absent => (),
            VNodeStatus::Bootstrap => {
                // mark pending if it doesn't start
                vnode.start_bootstrap(db);
            }
            status => panic!("{:?} isn't a valid state after load", status),
        }

        vnode
    }

    pub fn save(&mut self, db: &Database, shutdown: bool) {
        self.state.save(db, shutdown);
    }

    pub fn status(&self) -> VNodeStatus {
        self.state.status
    }

    #[cfg(test)]
    pub fn _log_len(&self, node: NodeId) -> usize {
        self.state.logs.storage.iterator().iter().count()
    }

    pub fn syncs_inflight(&self) -> (usize, usize) {
        let pend = if self.state.pending_bootstrap { 1 } else { 0 };
        self.syncs.values().fold(
            (pend, 0),
            |(inc, out), s| match *s {
                Synchronization::BootstrapReceiver { .. } |
                Synchronization::SyncReceiver { .. } => (inc + 1, out),
                Synchronization::BootstrapSender { .. } |
                Synchronization::SyncSender { .. } => (inc, out + 1),
            },
        )
    }

    fn gen_cookie(&self) -> Cookie {
        let mut rng = thread_rng();
        Cookie::new(rng.gen(), rng.gen())
    }

    // DHT Changes
    pub fn handler_dht_change(&mut self, db: &Database, x_status: VNodeStatus) {
        match x_status {
            VNodeStatus::Absent | VNodeStatus::Ready => (),
            status => panic!("Invalid final status {:?}", status),
        }
        let status = self.status();
        match (status, x_status) {
            (VNodeStatus::Ready, VNodeStatus::Absent) |
            (VNodeStatus::Bootstrap, VNodeStatus::Absent) => {
                {
                    // cancel incomming syncs
                    let state = &mut self.state;
                    let canceled = self.syncs
                        .iter_mut()
                        .filter_map(|(&cookie, m)| if let SyncDirection::Incomming =
                            m.direction()
                        {
                            m.on_cancel(db, state);
                            Some(cookie)
                        } else {
                            None
                        })
                        .collect::<Vec<_>>();
                    for cookie in canceled {
                        self.syncs.remove(&cookie).unwrap().on_remove(db, state);
                    }
                }

                // vnode goes into zombie unless it was bootstraping
                let new_status = if status == VNodeStatus::Bootstrap {
                    VNodeStatus::Absent
                } else {
                    VNodeStatus::Zombie
                };

                self.state.set_status(db, new_status);
            }
            (VNodeStatus::Zombie, VNodeStatus::Absent) => {
                // do nothing, zombie will timeout and switch to absent eventually
            }
            (VNodeStatus::Zombie, VNodeStatus::Ready) => {
                // fast-recomission!
                self.state.set_status(db, VNodeStatus::Ready);
            }
            (VNodeStatus::Absent, VNodeStatus::Ready) => {
                // recomission by bootstrap
                self.state.set_status(db, VNodeStatus::Bootstrap);
                self.start_bootstrap(db);
            }
            (VNodeStatus::Bootstrap, VNodeStatus::Ready) => {
                // check if there's a pending bootstrap we need to start
                if self.state.pending_bootstrap {
                    self.start_bootstrap(db);
                }
            }
            (a, b) if a == b => (), // nothing to do
            (a, b) => panic!("Invalid status change from dht {:?} -> {:?}", a, b),
        }

        if self.status() != VNodeStatus::Absent {
            self.state.update_peers(db);
        }
    }

    // TICK
    pub fn handler_tick(&mut self, db: &Database, _time: Instant) {
        let terminated_syncs = {
            let state = &mut self.state;
            self.syncs
                .iter_mut()
                .filter_map(|(&cookie, s)| match s.on_tick(db, state) {
                    SyncResult::Continue => None,
                    result => Some((cookie, result)),
                })
                .collect::<Vec<_>>()
        };
        for (cookie, result) in terminated_syncs {
            self.syncs.remove(&cookie).unwrap().on_remove(
                db,
                &mut self.state,
            );
            if self.status() == VNodeStatus::Bootstrap {
                self.handle_bootstrap_result(db, result);
            }
        }

        let now = Instant::now();
        while let Some((cookie, req)) = self.requests.pop_expired(now) {
            debug!("Request cookie:{:?} token:{} timed out", cookie, req.token);
            db.respond_error(req.token, CommandError::Timeout);
        }

        if self.state.pending_bootstrap {
            // check if there's a pending bootstrap we need to start
            self.start_bootstrap(db);
        } else if self.status() == VNodeStatus::Zombie && self.requests.is_empty() &&
                   self.syncs.is_empty() &&
                   self.state.last_status_change.elapsed() >
                       Duration::from_millis(ZOMBIE_TIMEOUT_MS)
        {
            // go absent when zombie timeout
            self.state.set_status(db, VNodeStatus::Absent);
        }
    }

    // CLIENT CRUD
    pub fn do_get(
        &mut self,
        db: &Database,
        token: Token,
        key: &[u8],
        consistency: ConsistencyLevel,
    ) {
        debug!(
            "vnode:{:?} do_get ({:?}) {:?}",
            self.state.num(),
            token,
            consistency
        );
        // TODO: lots of optimizations to be done here
        let nodes = db.dht.nodes_for_vnode(self.state.num, false, true);
        let cookie = self.gen_cookie();
        let expire = Instant::now() + Duration::from_millis(db.config.request_timeout as _);
        let req = ReqState::new(token, nodes.len(), consistency, false, true);
        assert!(self.requests.insert(cookie, req, expire).is_none());

        for node in nodes {
            if node == db.dht.node() {
                let container = self.state.storage_get(key);
                self.process_get(db, cookie, Some(container));
            } else {
                let ok = db.fabric
                    .send_msg(
                        node,
                        MsgRemoteGet {
                            cookie: cookie,
                            vnode: self.state.num,
                            key: key.into(),
                        },
                    )
                    .is_ok();
                if !ok {
                    self.process_get(db, cookie, None);
                }
            }
        }
    }

    fn respond_cant_coordinate(
        &mut self,
        db: &Database,
        token: Token,
        status: VNodeStatus,
        key: &[u8],
    ) {
        let mut nodes = db.dht.write_members_for_vnode(self.state.num());
        thread_rng().shuffle(&mut nodes);
        for (node, (_, addr)) in nodes {
            if node != db.dht.node() {
                let hash_slot = hash_slot(key);
                match status {
                    VNodeStatus::Absent | VNodeStatus::Zombie => {
                        db.respond_moved(token, hash_slot, addr);
                    }
                    VNodeStatus::Bootstrap => {
                        db.respond_ask(token, hash_slot, addr);
                    }
                    VNodeStatus::Ready => unreachable!(),
                }
                return;
            }
        }

        db.respond_error(token, CommandError::Unavailable);
    }

    pub fn do_set(
        &mut self,
        db: &Database,
        token: Token,
        key: &[u8],
        value_opt: Option<&[u8]>,
        vv: VersionVector,
        consistency: ConsistencyLevel,
        reply_result: bool,
    ) {
        match self.status() {
            VNodeStatus::Ready => (),
            status => return self.respond_cant_coordinate(db, token, status, key),
        }

        let nodes = db.dht.nodes_for_vnode(self.state.num, true, true);
        let cookie = self.gen_cookie();
        let expire = Instant::now() + Duration::from_millis(db.config.request_timeout as _);
        let mut req = ReqState::new(
            token,
            nodes.len(),
            consistency,
            value_opt.is_none(),
            reply_result,
        );

        let dcc = match self.state.storage_set_local(db, key, value_opt, &vv) {
            Ok(dcc) => dcc,
            Err(_) => return db.respond_error(token, CommandError::TooManyVersions),
        };

        req.container = dcc.clone();
        assert!(self.requests.insert(cookie, req, expire).is_none());

        self.process_set(db, cookie, true);
        let reply = consistency != ConsistencyLevel::One;
        for node in nodes {
            if node != db.dht.node() {
                let ok = db.fabric
                    .send_msg(
                        node,
                        MsgRemoteSet {
                            cookie: cookie,
                            vnode: self.state.num,
                            key: key.into(),
                            container: dcc.clone(),
                            reply: reply,
                        },
                    )
                    .is_ok();
                if !ok {
                    self.process_set(db, cookie, false);
                }
            }
        }
    }

    // OTHER
    fn process_get(
        &mut self,
        db: &Database,
        cookie: Cookie,
        container_opt: Option<DottedCausalContainer<Bytes>>,
    ) {
        if let HMEntry::Occupied(mut o) = self.requests.entry(cookie) {
            debug!("process_get {:?}", cookie);
            let done = {
                let state = o.get_mut();
                state.replies += 1;
                if let Some(container) = container_opt {
                    state.container.sync(container);
                    state.succesfull += 1;
                }
                state.done()
            };
            if done {
                let state = o.remove();
                if !state.satisfied() {
                    db.respond_error(state.token, CommandError::Unavailable);
                } else {
                    db.respond_dcc(state.token, state.container);
                }
            }
        } else {
            debug!("process_get cookie not found {:?}", cookie);
        }
    }

    fn process_set(&mut self, db: &Database, cookie: Cookie, succesfull: bool) {
        if let HMEntry::Occupied(mut o) = self.requests.entry(cookie) {
            let done = {
                let state = o.get_mut();
                state.replies += 1;
                if succesfull {
                    state.succesfull += 1;
                }
                state.done()
            };
            if done {
                let state = o.remove();
                if !state.satisfied() {
                    db.respond_error(state.token, CommandError::Unavailable);
                } else if state.reply_result {
                    db.respond_dcc(state.token, state.container);
                } else if state.is_delete {
                    db.respond_int(state.token, 1);
                } else {
                    db.respond_ok(state.token);
                }
            }
        } else {
            debug!("process_set cookie not found {:?}", cookie);
        }
    }

    // CRUD HANDLERS
    pub fn handler_get_remote_ack(&mut self, db: &Database, _from: NodeId, msg: MsgRemoteGetAck) {
        self.process_get(db, msg.cookie, msg.result.ok());
    }

    pub fn handler_get_remote(&mut self, db: &Database, from: NodeId, msg: MsgRemoteGet) {
        // accept zombie to reduce chance of timeouts due to races on cluster change
        check_status!(
            self,
            VNodeStatus::Ready | VNodeStatus::Zombie,
            db,
            from,
            msg,
            MsgRemoteGetAck,
            inflight_get
        );
        let dcc = self.state.storage_get(&msg.key);
        let _ = db.fabric.send_msg(
            from,
            MsgRemoteGetAck {
                cookie: msg.cookie,
                vnode: msg.vnode,
                result: Ok(dcc),
            },
        );
    }

    pub fn handler_set_remote(&mut self, db: &Database, from: NodeId, msg: MsgRemoteSet) {
        check_status!(
            self,
            VNodeStatus::Ready | VNodeStatus::Bootstrap,
            db,
            from,
            msg,
            MsgRemoteSetAck,
            inflight_set
        );
        let MsgRemoteSet {
            key,
            container,
            vnode,
            cookie,
            reply,
        } = msg;
        let result = self.state.storage_set_remote(db, &key, container);
        if reply {
            let _ = db.fabric.send_msg(
                from,
                MsgRemoteSetAck {
                    vnode: vnode,
                    cookie: cookie,
                    result: Ok(result),
                },
            );
        }
    }

    pub fn handler_set_remote_ack(&mut self, db: &Database, _from: NodeId, msg: MsgRemoteSetAck) {
        self.process_set(db, msg.cookie, msg.result.is_ok());
    }

    // SYNC
    pub fn handler_sync_start(&mut self, db: &Database, from: NodeId, msg: MsgSyncStart) {
        if !(self.state.status == VNodeStatus::Ready ||
                 (self.state.status == VNodeStatus::Zombie &&
                      self.state.last_status_change.elapsed() <
                          Duration::from_millis(ZOMBIE_TIMEOUT_MS)))
        {
            debug!("Can't start sync when {:?}", self.state.status);
            let _ = fabric_send_error!(db, from, msg, MsgSyncFin, FabricMsgError::BadVNodeStatus);
        } else if !self.syncs.contains_key(&msg.cookie) {
            if !db.signal_sync_start(SyncDirection::Outgoing) {
                debug!("Aborting handler_sync_start");
                let _ = fabric_send_error!(db, from, msg, MsgSyncFin, FabricMsgError::NotReady);
                return;
            }

            let cookie = msg.cookie;
            let sync = match msg.target {
                None => {
                    info!("starting bootstrap sender {:?} peer:{}", cookie, from);
                    Synchronization::new_bootstrap_sender(db, &mut self.state, from, msg)
                }
                Some(target) => {
                    assert_eq!(target, db.dht.node());
                    self.state.update_peer_knowledge(from, &msg.clocks_in_peer);
                    info!("starting sync sender {:?} peer:{}", cookie, from);
                    Synchronization::new_sync_sender(db, &mut self.state, from, msg)
                }
            };
            match self.syncs.entry(cookie) {
                HMEntry::Vacant(v) => {
                    v.insert(sync).on_start(db, &mut self.state);
                }
                HMEntry::Occupied(_) => unreachable!(),
            }
        }
    }

    pub fn handler_sync_send(&mut self, db: &Database, from: NodeId, msg: MsgSyncSend) {
        forward!(
            self,
            VNodeStatus::Ready | VNodeStatus::Bootstrap,
            db,
            from,
            msg,
            MsgSyncFin,
            syncs,
            on_msg_send
        );
    }

    pub fn handler_sync_ack(&mut self, db: &Database, from: NodeId, msg: MsgSyncAck) {
        forward!(
            self,
            VNodeStatus::Ready | VNodeStatus::Zombie,
            db,
            from,
            msg,
            MsgSyncFin,
            syncs,
            on_msg_ack
        );
    }

    pub fn handler_sync_fin(&mut self, db: &Database, from: NodeId, msg: MsgSyncFin) {
        check_status!(
            self,
            VNodeStatus::Ready | VNodeStatus::Zombie | VNodeStatus::Bootstrap,
            db,
            from,
            msg,
            MsgSyncFin,
            syncs
        );
        let cookie = msg.cookie;
        let result = if let HMEntry::Occupied(mut o) = self.syncs.entry(cookie) {
            let result = o.get_mut().on_msg_fin(db, &mut self.state, msg);
            match result {
                SyncResult::Done | SyncResult::Error => {
                    info!("Removing sync/bootstrap {:?}", cookie);
                    o.remove().on_remove(db, &mut self.state);
                }
                SyncResult::Continue => (),
            }
            trace!("handler_sync_fin {:?}: {:?}", cookie, result);
            result
        } else {
            trace!("Can't find cookie {:?} for msg sync fin", cookie);
            // only send error if Ok, otherwise the message will be sent back and forth forever
            if msg.result.is_ok() {
                let _ =
                    fabric_send_error!(db, from, msg, MsgSyncFin, FabricMsgError::CookieNotFound);
            }
            return;
        };

        if self.status() == VNodeStatus::Bootstrap {
            self.handle_bootstrap_result(db, result);
        }
    }

    fn handle_bootstrap_result(&mut self, db: &Database, result: SyncResult) {
        match result {
            SyncResult::Error => {
                info!("Retrying bootstrap");
                self.start_bootstrap(db);
            }
            SyncResult::Done => {
                match db.dht.promote_pending_node(db.dht.node(), self.state.num()) {
                    Ok(_) => {
                        // now we're ready!
                        self.state.set_status(db, VNodeStatus::Ready);
                    }
                    Err(e) => {
                        // it's not clear what happened
                        // go absent and wait for a dht callback to fix it
                        self.state.set_status(db, VNodeStatus::Absent);
                        warn!(
                            "Can't promote node {} vnode {}: {}",
                            db.dht.node(),
                            self.state.num(),
                            e
                        );
                    }
                }
            }
            SyncResult::Continue => (),
        }
    }

    fn start_bootstrap(&mut self, db: &Database) {
        debug!(
            "start_bootstrap vn:{} p:{:?}",
            self.state.num,
            self.state.pending_bootstrap
        );
        assert_eq!(self.state.status, VNodeStatus::Bootstrap);
        assert_eq!(self.syncs.len(), 0);
        self.state.pending_bootstrap = false;
        let cookie = self.gen_cookie();
        let mut nodes = db.dht.nodes_for_vnode(self.state.num, false, true);
        if nodes.is_empty() || nodes[0] == db.dht.node() {
            // nothing to boostrap from
            self.state.set_status(db, VNodeStatus::Ready);
            return;
        }

        thread_rng().shuffle(&mut nodes);
        for node in nodes {
            if node == db.dht.node() {
                continue;
            }
            if !db.signal_sync_start(SyncDirection::Incomming) {
                debug!("Bootstrap not allowed to start, go pending");
                self.state.pending_bootstrap = true;
                return;
            }
            info!("starting bootstrap receiver {:?} peer:{}", cookie, node);
            let sync = Synchronization::new_bootstrap_receiver(db, &mut self.state, node, cookie);
            match self.syncs.entry(cookie) {
                HMEntry::Vacant(v) => {
                    v.insert(sync).on_start(db, &mut self.state);
                }
                HMEntry::Occupied(_) => unreachable!(),
            }
            return;
        }
        unreachable!();
    }

    pub fn start_sync_if_ready(&mut self, db: &Database) -> bool {
        match self.state.status {
            VNodeStatus::Ready => self.do_start_sync(db),
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn _start_sync(&mut self, db: &Database) -> bool {
        assert_any!(self.state.status, VNodeStatus::Ready);
        self.do_start_sync(db)
    }

    fn do_start_sync(&mut self, db: &Database) -> bool {
        debug!("do_start_sync vn:{}", self.state.num);
        let mut nodes = db.dht.nodes_for_vnode(self.state.num, false, true);
        thread_rng().shuffle(&mut nodes);
        for node in nodes {
            if node == db.dht.node() {
                continue;
            }
            if !db.signal_sync_start(SyncDirection::Incomming) {
                debug!("Aborting do_start_sync");
                continue;
            }

            let cookie = self.gen_cookie();
            self.state.sync_nodes.insert(node);
            info!("starting sync receiver {:?} peer:{}", cookie, node);
            let sync = Synchronization::new_sync_receiver(db, &mut self.state, node, cookie);
            match self.syncs.entry(cookie) {
                HMEntry::Vacant(v) => {
                    v.insert(sync).on_start(db, &mut self.state);
                }
                HMEntry::Occupied(_) => unreachable!(),
            }
            return true;
        }
        false
    }
}

impl Drop for VNode {
    fn drop(&mut self) {
        // clean up any references to the storage
        self.requests.clear();
        self.syncs.clear();
    }
}

impl VNodeState {
    pub fn num(&self) -> u16 {
        self.num
    }

    pub fn status(&self) -> VNodeStatus {
        self.status
    }

    pub fn clear(&mut self) {
        self.clocks.clear();
        self.logs.clear();
        self.storage.clear();
    }

    /// Updates knowledge of a peer based on it's clocks vector
    pub fn update_peer_knowledge(&mut self, peer: NodeId, bvv: &BitmappedVersionVector) {
        debug!("update_peer_knowledge {} {:?}", peer, bvv);
        for (&node, bv) in bvv.iter() {
            self.logs
                .advance_knowledge(node, peer, bv.base());
        }
    }

    #[allow(unused_variables)]
    pub fn update_peers(&mut self, db: &Database) {
        // let peers = db.dht.nodes_for_vnode(self.num, true, true);
        // let physical_peers: LinearSet<_> = peers.iter().cloned().map(|n| split_u64(n).0).collect();
        // let mut to_remove = Vec::new();
        // for &peer in &peers {
        //     if peer == db.dht.node() {
        //         continue; // skip itself
        //     }
        //     let log = self.logs.entry(peer).or_insert_with(Default::default);
        //     for &physical_peer in &physical_peers {
        //         log.knowledge.entry(physical_peer).or_insert(0);
        //     }
        //     // we just added all known peers
        //     // so we only need to remove if knowledge len is larger
        //     if physical_peers.len() != log.knowledge.len() {
        //         to_remove.extend(
        //             log.knowledge
        //                 .keys()
        //                 .filter(|n| !physical_peers.contains(n))
        //                 .cloned(),
        //         );
        //         for peer in to_remove.drain(..) {
        //             log.knowledge.remove(&peer);
        //         }
        //         log.gc();
        //     }
        // }
    }

    pub fn set_status(&mut self, db: &Database, new: VNodeStatus) {
        if new == self.status {
            return;
        }
        info!(
            "VNode {} status change {:?} -> {:?}",
            self.num,
            self.status,
            new
        );
        match new {
            VNodeStatus::Bootstrap => {
                assert!(!self.pending_bootstrap);
                assert_eq!(self.sync_nodes.len(), 0);
                self.clear();
            }
            VNodeStatus::Absent => {
                assert_eq!(self.sync_nodes.len(), 0);
                self.clear();
            }
            VNodeStatus::Ready | VNodeStatus::Zombie => {}
        }

        self.last_status_change = Instant::now();
        self.pending_bootstrap = false;
        self.status = new;
        // not important in all cases but nice to do
        self.save(db, false);
    }

    fn new_empty(num: u16, db: &Database, status: VNodeStatus) -> Self {
        db.meta_storage.del(num.to_string().as_bytes());
        let storage = db.storage_manager.open(num).unwrap();
        let log_storage = db.storage_manager.open_log(num).unwrap();
        storage.clear();
        log_storage.clear();

        VNodeState {
            num: num,
            status: status,
            last_status_change: Instant::now(),
            clocks: BitmappedVersionVector::new(),
            logs: VNodeLogs::new(log_storage),
            storage: storage,
            pending_bootstrap: false,
            sync_nodes: Default::default(),
        }
    }

    fn load(num: u16, db: &Database, status: VNodeStatus) -> Self {
        info!("Loading vnode {} state", num);
        let saved_state_opt = db.meta_storage.get(num.to_string().as_bytes(), |bytes| {
            bincode::deserialize(bytes).unwrap()
        });
        if status == VNodeStatus::Absent || saved_state_opt.is_none() {
            info!("No saved state");
            return Self::new_empty(num, db, status);
        };

        assert_eq!(status, VNodeStatus::Ready);
        let SavedVNodeState {
            mut clocks,
            clean_shutdown,
        } = saved_state_opt.unwrap();

        let storage = db.storage_manager.open(num).unwrap();
        let log_storage = db.storage_manager.open_log(num).unwrap();
        let mut logs = VNodeLogs::new(log_storage);

        if !clean_shutdown {
            info!("Unclean shutdown, recovering from the storage");
            for (&node, bv) in clocks.iter_mut() {
                logs.iter_log(node, bv.base() + 1, |dot, _| {
                    bv.add(dot);
                });
            }
        }

        VNodeState {
            num: num,
            status: status,
            last_status_change: Instant::now(),
            clocks: clocks,
            storage: storage,
            logs: logs,
            sync_nodes: Default::default(),
            pending_bootstrap: false,
        }
    }

    pub fn save(&self, db: &Database, shutdown: bool) {
        let saved_state = SavedVNodeState {
            clocks: self.clocks.clone(),
            clean_shutdown: shutdown,
        };
        debug!("Saving state for vnode {:?} {:?}", self.num, saved_state);
        let serialized_saved_state = bincode::serialize(&saved_state, bincode::Infinite).unwrap();
        db.meta_storage.set(
            self.num.to_string().as_bytes(),
            &serialized_saved_state,
        );
    }

    // STORAGE
    pub fn storage_get(&self, key: &[u8]) -> DottedCausalContainer<Bytes> {
        let mut dcc = self.storage
            .get(key, |bytes| bincode::deserialize(bytes).unwrap())
            .unwrap_or_else(DottedCausalContainer::new);
        dcc.fill(&self.clocks);
        dcc
    }

    pub fn storage_set_local(
        &mut self,
        db: &Database,
        key: &[u8],
        value_opt: Option<&[u8]>,
        vv: &VersionVector,
    ) -> Result<DottedCausalContainer<Bytes>, ()> {
        let mut dcc = self.storage_get(key);
        dcc.discard(vv);

        if dcc.values().len() >= db.config.value_version_max as usize {
            return Err(());
        }

        let dot = self.clocks.event(db.dht.node());
        if let Some(value) = value_opt {
            dcc.add(db.dht.node(), dot, value.into());
        }
        dcc.strip(&self.clocks);

        if dcc.is_dcc_empty() {
            self.storage.del(key);
        } else {
            let bytes = bincode::serialize(&dcc, bincode::Infinite).unwrap();
            self.storage.set(key, &bytes);
        }

        // this needs to go in a writebatch with the above
        self.logs
            .log(db.dht.node(), dot, key.into());

        // FIXME: we striped above so we have to fill again :(
        dcc.fill(&self.clocks);
        Ok(dcc)
    }

    pub fn storage_set_remote(
        &mut self,
        _db: &Database,
        key: &[u8],
        mut new_dcc: DottedCausalContainer<Bytes>,
    ) {
        let old_dcc = self.storage_get(key);
        // it's important to only add_dots after fetching (and fill()) the old dcc
        self.clocks.add_dots(&new_dcc);
        new_dcc.sync(old_dcc);
        new_dcc.strip(&self.clocks);

        if new_dcc.is_dcc_empty() {
            self.storage.del(key);
        } else {
            let dcc_bytes = bincode::serialize(&new_dcc, bincode::Infinite).unwrap();
            self.storage.set(key, &dcc_bytes);
        }

        // these need to go in a writebatch with the above
        for (&(node, version), _) in new_dcc.iter() {
            self.logs.log(
                node,
                version,
                key.into(),
            );
        }
    }
}
