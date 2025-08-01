// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    fs,
    sync::{Arc, mpsc::channel},
    thread,
    time::Duration,
};

use engine_traits::{CF_DEFAULT, CF_RAFT, CF_WRITE, Peekable, RaftEngineReadOnly};
use keys::data_key;
use kvproto::{
    metapb, pdpb,
    raft_cmdpb::*,
    raft_serverpb::{ExtraMessageType, RaftMessage, RegionLocalState},
};
use pd_client::PdClient;
use raft::eraftpb::MessageType;
use raftstore::{
    Result,
    store::{Bucket, BucketRange, Callback, WriteResponse},
};
use raftstore_v2::router::QueryResult;
use test_raftstore::*;
use test_raftstore_macro::test_case;
use test_util::eventually;
use tikv::storage::{Snapshot, kv::SnapshotExt};
use tikv_util::{config::*, future::block_on_timeout};
use txn_types::{Key, LastChange, PessimisticLock};

use crate::tikv_util::HandyRwLock;

pub const REGION_MAX_SIZE: u64 = 50000;
pub const REGION_SPLIT_SIZE: u64 = 30000;

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_server_base_split_region() {
    let test_base_split_region = |right_derive| {
        let count = 5;
        let mut cluster = new_cluster(0, count);

        cluster.cfg.raft_store.right_derive_when_split = right_derive;
        cluster.run();

        let pd_client = Arc::clone(&cluster.pd_client);

        let tbls = vec![
            (b"k22", b"k11", b"k33"),
            (b"k11", b"k00", b"k11"),
            (b"k33", b"k22", b"k33"),
        ];

        for (split_key, left_key, right_key) in tbls {
            cluster.must_put(left_key, b"v1");
            cluster.must_put(right_key, b"v3");

            // Left and right key must be in same region before split.
            let region = pd_client.get_region(left_key).unwrap();
            let region2 = pd_client.get_region(right_key).unwrap();
            assert_eq!(region.get_id(), region2.get_id());

            // Split with split_key, so left_key must in left, and right_key in right.
            cluster.must_split(&region, split_key);

            let left = pd_client.get_region(left_key).unwrap();
            let right = pd_client.get_region(right_key).unwrap();

            assert_eq!(
                region.get_id(),
                if right_derive {
                    right.get_id()
                } else {
                    left.get_id()
                }
            );
            assert_eq!(region.get_start_key(), left.get_start_key());
            assert_eq!(left.get_end_key(), right.get_start_key());
            assert_eq!(region.get_end_key(), right.get_end_key());

            cluster.must_put(left_key, b"vv1");
            assert_eq!(cluster.get(left_key).unwrap(), b"vv1".to_vec());

            cluster.must_put(right_key, b"vv3");
            assert_eq!(cluster.get(right_key).unwrap(), b"vv3".to_vec());

            let epoch = left.get_region_epoch().clone();
            let get = new_request(left.get_id(), epoch, vec![new_get_cmd(right_key)], false);
            debug!("requesting {:?}", get);
            let resp = cluster
                .call_command_on_leader(get, Duration::from_secs(5))
                .unwrap();
            assert!(resp.get_header().has_error(), "{:?}", resp);
            assert!(
                resp.get_header().get_error().has_key_not_in_region(),
                "{:?}",
                resp
            );
        }
    };

    // left derive
    test_base_split_region(false);
    // right derive
    test_base_split_region(true);
}

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_server_split_region_twice() {
    let count = 5;
    let mut cluster = new_cluster(0, count);
    cluster.run();
    let pd_client = Arc::clone(&cluster.pd_client);

    let (split_key, left_key, right_key) = (b"k22", b"k11", b"k33");
    cluster.must_put(left_key, b"v1");
    cluster.must_put(right_key, b"v3");

    // Left and right key must be in same region before split.
    let region = pd_client.get_region(left_key).unwrap();
    let region2 = pd_client.get_region(right_key).unwrap();
    assert_eq!(region.get_id(), region2.get_id());

    let (tx, rx) = channel();
    let key = split_key.to_vec();
    let c = Box::new(move |write_resp: WriteResponse| {
        let mut resp = write_resp.response;
        let admin_resp = resp.mut_admin_response();
        let split_resp = admin_resp.mut_splits();
        let mut regions: Vec<_> = split_resp.take_regions().into();
        let mut d = regions.drain(..);
        let (left, right) = (d.next().unwrap(), d.next().unwrap());
        assert_eq!(left.get_end_key(), key.as_slice());
        assert_eq!(region2.get_start_key(), left.get_start_key());
        assert_eq!(left.get_end_key(), right.get_start_key());
        assert_eq!(region2.get_end_key(), right.get_end_key());
        tx.send(right).unwrap();
    });
    cluster.split_region(&region, split_key, Callback::write(c));
    let region3 = rx.recv_timeout(Duration::from_secs(5)).unwrap();

    cluster.must_put(split_key, b"v2");

    let (tx1, rx1) = channel();
    let c = Box::new(move |write_resp: WriteResponse| {
        assert!(write_resp.response.has_header());
        assert!(write_resp.response.get_header().has_error());
        assert!(!write_resp.response.has_admin_response());
        tx1.send(()).unwrap();
    });
    cluster.split_region(&region3, split_key, Callback::write(c));
    rx1.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore::new_incompatible_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_auto_split_region() {
    let count = 5;
    let mut cluster = new_cluster(0, count);
    cluster.cfg.raft_store.split_region_check_tick_interval = ReadableDuration::millis(100);
    cluster.cfg.coprocessor.region_max_size = Some(ReadableSize(REGION_MAX_SIZE));
    cluster.cfg.coprocessor.region_split_size = Some(ReadableSize(REGION_SPLIT_SIZE));

    let check_size_diff = cluster.cfg.raft_store.region_split_check_diff().0;
    let mut range = 1..;

    cluster.run();

    let pd_client = Arc::clone(&cluster.pd_client);

    let region = pd_client.get_region(b"").unwrap();

    let last_key = put_till_size(&mut cluster, REGION_SPLIT_SIZE, &mut range);

    // it should be finished in millis if split.
    thread::sleep(Duration::from_millis(300));

    let target = pd_client.get_region(&last_key).unwrap();

    assert_eq!(region, target);

    let max_key = put_cf_till_size(
        &mut cluster,
        CF_WRITE,
        REGION_MAX_SIZE - REGION_SPLIT_SIZE + check_size_diff,
        &mut range,
    );

    let left = pd_client.get_region(b"").unwrap();
    let right = pd_client.get_region(&max_key).unwrap();
    if left == right {
        cluster.wait_region_split(&region);
    }

    let left = pd_client.get_region(b"").unwrap();
    let right = pd_client.get_region(&max_key).unwrap();

    assert_ne!(left, right);
    assert_eq!(region.get_start_key(), left.get_start_key());
    assert_eq!(right.get_start_key(), left.get_end_key());
    assert_eq!(region.get_end_key(), right.get_end_key());
    assert_eq!(pd_client.get_region(&max_key).unwrap(), right);
    assert_eq!(pd_client.get_region(left.get_end_key()).unwrap(), right);

    let middle_key = left.get_end_key();
    let leader = cluster.leader_of_region(left.get_id()).unwrap();
    let store_id = leader.get_store_id();
    let mut size = 0;
    cluster
        .scan(
            store_id,
            CF_DEFAULT,
            &data_key(b""),
            &data_key(middle_key),
            false,
            |k, v| {
                size += k.len() as u64;
                size += v.len() as u64;
                Ok(true)
            },
        )
        .expect("");
    assert!(size <= REGION_SPLIT_SIZE);
    // although size may be smaller than REGION_SPLIT_SIZE, but the diff should
    // be small.
    assert!(size > REGION_SPLIT_SIZE - 1000);

    let epoch = left.get_region_epoch().clone();
    let get = new_request(left.get_id(), epoch, vec![new_get_cmd(&max_key)], false);
    let resp = cluster
        .call_command_on_leader(get, Duration::from_secs(5))
        .unwrap();
    assert!(resp.get_header().has_error());
    assert!(resp.get_header().get_error().has_key_not_in_region());
}

// A filter that disable commitment by heartbeat.
#[derive(Clone)]
struct EraseHeartbeatCommit;

impl Filter for EraseHeartbeatCommit {
    fn before(&self, msgs: &mut Vec<RaftMessage>) -> Result<()> {
        for msg in msgs {
            if msg.get_message().get_msg_type() == MessageType::MsgHeartbeat {
                msg.mut_message().set_commit(0);
            }
        }
        Ok(())
    }
}

macro_rules! check_cluster {
    ($cluster:expr, $k:expr, $v:expr, $all_committed:expr) => {
        let region = $cluster.pd_client.get_region($k).unwrap();
        let mut tried_cnt = 0;
        let leader = loop {
            match $cluster.leader_of_region(region.get_id()) {
                None => {
                    tried_cnt += 1;
                    if tried_cnt >= 3 {
                        panic!("leader should be elected");
                    }
                    continue;
                }
                Some(l) => break l,
            }
        };
        let mut missing_count = 0;
        for i in 1..=region.get_peers().len() as u64 {
            let engine = $cluster.get_engine(i);
            if $all_committed || i == leader.get_store_id() {
                must_get_equal(&engine, $k, $v);
            } else {
                // Note that a follower can still commit the log by an empty MsgAppend
                // when bcast commit is disabled. A heartbeat response comes to leader
                // before MsgAppendResponse will trigger MsgAppend.
                match engine.get_value(&keys::data_key($k)).unwrap() {
                    Some(res) => assert_eq!($v, &res[..]),
                    None => missing_count += 1,
                }
            }
        }
        assert!($all_committed || missing_count > 0);
    };
}

/// TiKV enables lazy broadcast commit optimization, which can delay split
/// on follower node. So election of new region will delay. We need to make
/// sure broadcast commit is disabled when split.
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_delay_split_region() {
    let mut cluster = new_cluster(0, 3);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(500);
    cluster.cfg.raft_store.merge_max_log_gap = 100;
    cluster.cfg.raft_store.raft_log_gc_threshold = 500;
    // To stable the test, we use a large hearbeat timeout 200ms(100ms * 2).
    // And to elect leader quickly, set election timeout to 1s(100ms * 10).
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));

    // We use three nodes for this test.
    cluster.run();

    let pd_client = Arc::clone(&cluster.pd_client);

    let region = pd_client.get_region(b"").unwrap();

    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");

    // Although skip bcast is enabled, but heartbeat will commit the log in period.
    check_cluster!(cluster, b"k1", b"v1", true);
    check_cluster!(cluster, b"k3", b"v3", true);
    cluster.must_transfer_leader(region.get_id(), new_peer(1, 1));

    cluster.add_send_filter(CloneFilterFactory(EraseHeartbeatCommit));

    cluster.must_put(b"k4", b"v4");
    sleep_ms(100);
    // skip bcast is enabled by default, so all followers should not commit
    // the log.
    check_cluster!(cluster, b"k4", b"v4", false);

    cluster.must_transfer_leader(region.get_id(), new_peer(3, 3));
    // New leader should flush old committed entries eagerly.
    check_cluster!(cluster, b"k4", b"v4", true);
    cluster.must_put(b"k5", b"v5");
    // New committed entries should be broadcast lazily.
    check_cluster!(cluster, b"k5", b"v5", false);
    cluster.add_send_filter(CloneFilterFactory(EraseHeartbeatCommit));

    let k2 = b"k2";
    // Split should be bcast eagerly, otherwise following must_put will fail
    // as no leader is available.
    cluster.must_split(&region, k2);
    cluster.must_put(b"k6", b"v6");

    sleep_ms(100);
    // After split, skip bcast is enabled again, so all followers should not
    // commit the log.
    check_cluster!(cluster, b"k6", b"v6", false);
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_node_split_overlap_snapshot() {
    let mut cluster = new_cluster(0, 3);
    // We use three nodes([1, 2, 3]) for this test.
    cluster.run();

    // guarantee node 1 is leader
    cluster.must_transfer_leader(1, new_peer(1, 1));
    cluster.must_put(b"k0", b"v0");
    assert_eq!(cluster.leader_of_region(1), Some(new_peer(1, 1)));

    let pd_client = Arc::clone(&cluster.pd_client);

    // isolate node 3 for region 1.
    cluster.add_send_filter(CloneFilterFactory(RegionPacketFilter::new(1, 3)));
    cluster.must_put(b"k1", b"v1");

    let region = pd_client.get_region(b"").unwrap();

    // split (-inf, +inf) -> (-inf, k2), [k2, +inf]
    cluster.must_split(&region, b"k2");

    cluster.must_put(b"k2", b"v2");

    // node 1 and node 2 must have k2, but node 3 must not.
    for i in 1..3 {
        let engine = cluster.get_engine(i);
        must_get_equal(&engine, b"k2", b"v2");
    }

    let engine3 = cluster.get_engine(3);
    must_get_none(&engine3, b"k2");

    thread::sleep(Duration::from_secs(1));
    let snap_dir = cluster.get_snap_dir(3);
    // no snaps should be sent.

    assert!(
        fs::read_dir(snap_dir)
            .unwrap()
            .map(|p| p.unwrap().path())
            .next()
            .is_none()
    );

    cluster.clear_send_filters();
    cluster.must_put(b"k3", b"v3");

    sleep_ms(3000);
    // node 3 must have k3.
    must_get_equal(&engine3, b"k3", b"v3");
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_apply_new_version_snapshot() {
    let mut cluster = new_cluster(0, 3);
    // truncate the log quickly so that we can force sending snapshot.
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::millis(20);
    cluster.cfg.raft_store.raft_log_gc_count_limit = Some(5);
    cluster.cfg.raft_store.merge_max_log_gap = 1;
    cluster.cfg.raft_store.raft_log_gc_threshold = 5;

    // We use three nodes([1, 2, 3]) for this test.
    cluster.run();

    // guarantee node 1 is leader
    cluster.must_transfer_leader(1, new_peer(1, 1));
    cluster.must_put(b"k0", b"v0");
    assert_eq!(cluster.leader_of_region(1), Some(new_peer(1, 1)));

    let pd_client = Arc::clone(&cluster.pd_client);

    // isolate node 3 for region 1.
    cluster.add_send_filter(CloneFilterFactory(RegionPacketFilter::new(1, 3)));
    cluster.must_put(b"k1", b"v1");

    let region = pd_client.get_region(b"").unwrap();

    // split (-inf, +inf) -> (-inf, k2), [k2, +inf]
    cluster.must_split(&region, b"k2");
    cluster.must_put(b"k2", b"v2");

    // node 1 and node 2 must have k2, but node 3 must not.
    for i in 1..3 {
        let engine = cluster.get_engine(i);
        must_get_equal(&engine, b"k2", b"v2");
    }

    let engine3 = cluster.get_engine(3);
    must_get_none(&engine3, b"k2");

    // transfer leader to ease the preasure of store 1.
    cluster.must_transfer_leader(1, new_peer(2, 2));

    for _ in 0..100 {
        // write many logs to force log GC for region 1 and region 2.
        cluster.must_put(b"k1", b"v1");
        cluster.must_put(b"k2", b"v2");
    }

    cluster.clear_send_filters();

    sleep_ms(3000);
    // node 3 must have k1, k2.
    must_get_equal(&engine3, b"k1", b"v1");
    must_get_equal(&engine3, b"k2", b"v2");
}

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_server_split_with_stale_peer() {
    let mut cluster = new_cluster(0, 3);
    // disable raft log gc.
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::secs(60);
    cluster.cfg.raft_store.peer_stale_state_check_interval = ReadableDuration::millis(500);

    let pd_client = Arc::clone(&cluster.pd_client);
    // Disable default max peer count check.
    pd_client.disable_default_operator();

    let r1 = cluster.run_conf_change();

    // add peer (2,2) to region 1.
    pd_client.must_add_peer(r1, new_peer(2, 2));

    // add peer (3,3) to region 1.
    pd_client.must_add_peer(r1, new_peer(3, 3));

    cluster.must_put(b"k0", b"v0");
    // check node 3 has k0.
    let engine3 = cluster.get_engine(3);
    must_get_equal(&engine3, b"k0", b"v0");

    // guarantee node 1 is leader.
    cluster.must_transfer_leader(r1, new_peer(1, 1));

    // isolate node 3 for region 1.
    // only filter MsgAppend to avoid election when recover.
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(1, 3).msg_type(MessageType::MsgAppend),
    ));

    let region = pd_client.get_region(b"").unwrap();

    // split (-inf, +inf) -> (-inf, k2), [k2, +inf]
    cluster.must_split(&region, b"k2");
    cluster.must_put(b"k2", b"v2");

    let region2 = pd_client.get_region(b"k2").unwrap();

    // remove peer3 in region 2.
    let peer3 = find_peer(&region2, 3).unwrap();
    pd_client.must_remove_peer(region2.get_id(), peer3.clone());

    // clear isolation so node 3 can split region 1.
    // now node 3 has a stale peer for region 2, but
    // it will be removed soon.
    cluster.clear_send_filters();
    cluster.must_put(b"k1", b"v1");

    // check node 3 has k1
    must_get_equal(&engine3, b"k1", b"v1");

    // split [k2, +inf) -> [k2, k3), [k3, +inf]
    cluster.must_split(&region2, b"k3");
    let region3 = pd_client.get_region(b"k3").unwrap();
    // region 3 can't contain node 3.
    assert_eq!(region3.get_peers().len(), 2);
    assert!(find_peer(&region3, 3).is_none());

    let new_peer_id = pd_client.alloc_id().unwrap();
    // add peer (3, new_peer_id) to region 3
    pd_client.must_add_peer(region3.get_id(), new_peer(3, new_peer_id));

    cluster.must_put(b"k3", b"v3");
    // node 3 must have k3.
    must_get_equal(&engine3, b"k3", b"v3");
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_split_region_diff_check() {
    let count = 1;
    let mut cluster = new_cluster(0, count);
    let region_max_size = 2000;
    let region_split_size = 1000;
    cluster.cfg.raft_store.split_region_check_tick_interval = ReadableDuration::millis(100);
    cluster.cfg.raft_store.region_split_check_diff = Some(ReadableSize(10));
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::secs(20);
    cluster.cfg.coprocessor.region_max_size = Some(ReadableSize(region_max_size));
    cluster.cfg.coprocessor.region_split_size = Some(ReadableSize(region_split_size));

    let mut range = 1..;

    cluster.run();

    let pd_client = Arc::clone(&cluster.pd_client);

    // The default size index distance is too large for small data, we flush
    // multiple times to generate more size index handles.
    for _ in 0..10 {
        put_till_size(&mut cluster, region_max_size, &mut range);
    }

    // Peer will split when size of region meet region_max_size, so assume the last
    // region_max_size of data is not involved in split, there will be at least
    // `(region_max_size * 10 - region_max_size) / region_split_size` regions.
    // But region_max_size of data should be split too, so there will be at
    // least 2 more regions.
    let min_region_cnt = (region_max_size * 10 - region_max_size) / region_split_size + 2;

    let mut try_cnt = 0;
    loop {
        sleep_ms(20);
        let region_cnt = pd_client.get_split_count() + 1;
        if region_cnt >= min_region_cnt as usize {
            return;
        }
        try_cnt += 1;
        if try_cnt == 500 {
            panic!(
                "expect split cnt {}, but got {}",
                min_region_cnt, region_cnt
            );
        }
    }
}

// Test steps
// set max region size/split size 2000 and put data till 1000
// set max region size/split size < 1000 and reboot
// verify the region is splitted.
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_node_split_region_after_reboot_with_config_change() {
    let count = 1;
    let mut cluster = new_cluster(0, count);
    let region_max_size = 2000;
    let region_split_size = 2000;
    cluster.cfg.raft_store.split_region_check_tick_interval = ReadableDuration::millis(50);
    cluster.cfg.raft_store.raft_log_gc_tick_interval = ReadableDuration::secs(20);
    cluster.cfg.coprocessor.enable_region_bucket = Some(true);
    cluster.cfg.coprocessor.region_max_size = Some(ReadableSize(region_max_size));
    cluster.cfg.coprocessor.region_split_size = Some(ReadableSize(region_split_size));
    cluster.cfg.coprocessor.region_bucket_size = ReadableSize(region_split_size);

    cluster.run();

    let pd_client = Arc::clone(&cluster.pd_client);

    let mut range = 1..;
    put_till_size(&mut cluster, region_max_size / 2, &mut range);

    // there should be 1 region
    sleep_ms(200);
    assert_eq!(pd_client.get_split_count(), 0);

    // change the config to make the region splitable
    cluster.cfg.coprocessor.region_max_size = Some(ReadableSize(region_max_size / 3));
    cluster.cfg.coprocessor.region_split_size = Some(ReadableSize(region_split_size / 3));
    cluster.cfg.coprocessor.region_bucket_size = ReadableSize(region_split_size / 3);
    cluster.stop_node(1);
    cluster.run_node(1).unwrap();

    let mut try_cnt = 0;
    loop {
        sleep_ms(20);
        if pd_client.get_split_count() > 0 {
            break;
        }
        try_cnt += 1;
        if try_cnt == 200 {
            panic!("expect get_split_count > 0 after 4s");
        }
    }
}

fn test_split_epoch_not_match<T: Simulator>(cluster: &mut Cluster<T>, right_derive: bool) {
    cluster.cfg.raft_store.right_derive_when_split = right_derive;
    cluster.run();
    let pd_client = Arc::clone(&cluster.pd_client);
    let old = pd_client.get_region(b"k1").unwrap();
    // Construct a get command using old region meta.
    let get_old = new_request(
        old.get_id(),
        old.get_region_epoch().clone(),
        vec![new_get_cmd(b"k1")],
        false,
    );
    cluster.must_split(&old, b"k2");
    let r = pd_client.get_region(b"k3").unwrap();
    let get_middle = new_request(
        r.get_id(),
        r.get_region_epoch().clone(),
        vec![new_get_cmd(b"k3")],
        false,
    );
    cluster.must_split(&r, b"k3");
    let r = pd_client.get_region(b"k4").unwrap();
    cluster.must_split(&r, b"k4");
    let regions: Vec<_> = [b"k0", b"k2", b"k3", b"k4"]
        .iter()
        .map(|&k| pd_client.get_region(k).unwrap())
        .collect();

    let new = regions[3].clone();
    // Newer epoch also triggers the EpochNotMatch error.
    let mut latest_epoch = new.get_region_epoch().clone();
    let latest_version = latest_epoch.get_version() + 1;
    latest_epoch.set_version(latest_version);
    let get_new = new_request(new.get_id(), latest_epoch, vec![new_get_cmd(b"k1")], false);

    let mut cases = vec![
        // All regions should be returned as request uses an oldest epoch.
        (get_old, regions.clone()),
        // Only new split regions should be returned.
        (get_middle, regions[1..].to_vec()),
        // Epoch is too new that TiKV can't offer any useful hint.
        (get_new, vec![regions[3].clone()]),
    ];
    if right_derive {
        // TiKV search backward when right derive.
        cases[0].1.reverse();
        cases[1].1.reverse();
    }
    for (get, exp) in cases {
        let resp = cluster
            .call_command_on_leader(get.clone(), Duration::from_secs(5))
            .unwrap();
        assert!(resp.get_header().has_error(), "{:?}", get);
        assert!(
            resp.get_header().get_error().has_epoch_not_match(),
            "{:?}",
            get
        );
        assert_eq!(
            resp.get_header()
                .get_error()
                .get_epoch_not_match()
                .get_current_regions(),
            &*exp,
            "{:?}",
            get
        );
    }
}

#[test]
fn test_server_split_epoch_not_match_left_derive() {
    let mut cluster = new_server_cluster(0, 3);
    test_split_epoch_not_match(&mut cluster, false);
}

#[test]
fn test_server_split_epoch_not_match_right_derive() {
    let mut cluster = new_server_cluster(0, 3);
    test_split_epoch_not_match(&mut cluster, true);
}

#[test]
fn test_node_split_epoch_not_match_left_derive() {
    let mut cluster = new_node_cluster(0, 3);
    test_split_epoch_not_match(&mut cluster, false);
}

#[test]
fn test_node_split_epoch_not_match_right_derive() {
    let mut cluster = new_node_cluster(0, 3);
    test_split_epoch_not_match(&mut cluster, true);
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_node_quick_election_after_split() {
    let mut cluster = new_cluster(0, 3);

    // For the peer which is the leader of the region before split, it should
    // campaigns immediately. and then this peer may take the leadership
    // earlier. `test_quick_election_after_split` is a helper function for testing
    // this feature.
    // Calculate the reserved time before a new campaign after split.
    let reserved_time =
        Duration::from_millis(cluster.cfg.raft_store.raft_base_tick_interval.as_millis() * 2);

    cluster.run();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");
    let region = cluster.get_region(b"k1");
    let old_leader = cluster.leader_of_region(region.get_id()).unwrap();

    cluster.must_split(&region, b"k2");

    // Wait for the peer of new region to start campaign.
    thread::sleep(reserved_time);

    // The campaign should always succeeds in the ideal test environment.
    let new_region = cluster.get_region(b"k3");
    // Ensure the new leader is established for the newly split region, and it
    // shares the same store with the leader of old region.
    let new_leader = cluster.query_leader(
        old_leader.get_store_id(),
        new_region.get_id(),
        Duration::from_secs(5),
    );
    assert!(new_leader.is_some());
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_node_split_region() {
    let count = 5;
    let mut cluster = new_cluster(0, count);
    // length of each key+value
    let item_len = 74;
    // make bucket's size to item_len, which means one row one bucket
    cluster.cfg.coprocessor.region_max_size = Some(ReadableSize(item_len) * 1024);
    let mut range = 1..;
    cluster.run();
    let pd_client = Arc::clone(&cluster.pd_client);
    let region = pd_client.get_region(b"").unwrap();
    let mid_key = put_till_size(&mut cluster, 11 * item_len, &mut range);
    let max_key = put_till_size(&mut cluster, 9 * item_len, &mut range);
    let target = pd_client.get_region(&max_key).unwrap();
    assert_eq!(region, target);
    pd_client.must_split_region(target, pdpb::CheckPolicy::Scan, vec![]);

    let left = pd_client.get_region(b"").unwrap();
    let right = pd_client.get_region(&max_key).unwrap();
    assert_eq!(region.get_start_key(), left.get_start_key());
    assert_eq!(mid_key.as_slice(), right.get_start_key());
    assert_eq!(right.get_start_key(), left.get_end_key());
    assert_eq!(region.get_end_key(), right.get_end_key());

    let region = pd_client.get_region(b"x").unwrap();
    pd_client.must_split_region(
        region,
        pdpb::CheckPolicy::Usekey,
        vec![b"x1".to_vec(), b"y2".to_vec()],
    );
    let x1 = pd_client.get_region(b"x1").unwrap();
    assert_eq!(x1.get_start_key(), b"x1");
    assert_eq!(x1.get_end_key(), b"y2");
    let y2 = pd_client.get_region(b"y2").unwrap();
    assert_eq!(y2.get_start_key(), b"y2");
    assert_eq!(y2.get_end_key(), b"");
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_node_split_update_region_right_derive() {
    let mut cluster = new_cluster(0, 3);
    // Election timeout and max leader lease is 1s.
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));

    cluster.run();

    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");

    let pd_client = Arc::clone(&cluster.pd_client);
    let region = pd_client.get_region(b"k1").unwrap();
    cluster.must_split(&region, b"k2");
    let right = pd_client.get_region(b"k2").unwrap();

    let origin_leader = cluster.leader_of_region(right.get_id()).unwrap();
    let new_leader = right
        .get_peers()
        .iter()
        .find(|&p| p.get_id() != origin_leader.get_id())
        .cloned()
        .unwrap();

    // Make sure split is done in the new_leader.
    // "k4" belongs to the right.
    cluster.must_put(b"k4", b"v4");
    must_get_equal(&cluster.get_engine(new_leader.get_store_id()), b"k4", b"v4");

    // Transfer leadership to another peer.
    cluster.must_transfer_leader(right.get_id(), new_leader);

    // Make sure the new_leader is in lease.
    cluster.must_put(b"k4", b"v5");

    // "k1" is not in the range of right.
    let get = new_request(
        right.get_id(),
        right.get_region_epoch().clone(),
        vec![new_get_cmd(b"k1")],
        false,
    );
    debug!("requesting {:?}", get);
    let resp = cluster
        .call_command_on_leader(get, Duration::from_secs(5))
        .unwrap();
    assert!(resp.get_header().has_error(), "{:?}", resp);
    assert!(
        resp.get_header().get_error().has_key_not_in_region(),
        "{:?}",
        resp
    );
}

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_split_with_epoch_not_match() {
    let mut cluster = new_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    cluster.run();

    cluster.must_transfer_leader(1, new_peer(1, 1));

    // Remove a peer to make conf version become 2.
    pd_client.must_remove_peer(1, new_peer(2, 2));
    let region = cluster.get_region(b"");

    let mut admin_req = AdminRequest::default();
    admin_req.set_cmd_type(AdminCmdType::BatchSplit);

    let mut batch_split_req = BatchSplitRequest::default();
    batch_split_req.mut_requests().push(SplitRequest::default());
    batch_split_req.mut_requests()[0].set_split_key(b"s".to_vec());
    batch_split_req.mut_requests()[0].set_new_region_id(1000);
    batch_split_req.mut_requests()[0].set_new_peer_ids(vec![1001, 1002]);
    batch_split_req.mut_requests()[0].set_right_derive(true);
    admin_req.set_splits(batch_split_req);

    let mut epoch = region.get_region_epoch().clone();
    epoch.conf_ver -= 1;
    let req = new_admin_request(1, &epoch, admin_req);
    let resp = cluster
        .call_command_on_leader(req, Duration::from_secs(3))
        .unwrap();
    assert!(resp.get_header().get_error().has_epoch_not_match());
}

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_split_with_in_memory_pessimistic_locks() {
    let peer_size_limit = 512 << 10;
    let instance_size_limit = 100 << 20;
    let mut cluster = new_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    cluster.run();

    cluster.must_transfer_leader(1, new_peer(1, 1));

    // Set two pessimistic locks in the original region.
    let txn_ext = cluster
        .must_get_snapshot_of_region(1)
        .ext()
        .get_txn_ext()
        .unwrap()
        .clone();
    let lock_a = PessimisticLock {
        primary: b"a".to_vec().into_boxed_slice(),
        start_ts: 10.into(),
        ttl: 3000,
        for_update_ts: 20.into(),
        min_commit_ts: 30.into(),
        last_change: LastChange::make_exist(5.into(), 3),
        is_locked_with_conflict: false,
    };
    let lock_c = PessimisticLock {
        primary: b"c".to_vec().into_boxed_slice(),
        start_ts: 20.into(),
        ttl: 3000,
        for_update_ts: 20.into(),
        min_commit_ts: 30.into(),
        last_change: LastChange::make_exist(5.into(), 3),
        is_locked_with_conflict: false,
    };
    {
        let mut locks = txn_ext.pessimistic_locks.write();
        locks
            .insert(
                vec![
                    (Key::from_raw(b"a"), lock_a.clone()),
                    (Key::from_raw(b"c"), lock_c.clone()),
                ],
                peer_size_limit,
                instance_size_limit,
            )
            .unwrap();
    }

    let region = cluster.get_region(b"");
    cluster.must_split(&region, b"b");

    // After splitting, each new region should contain one lock.

    let region = cluster.get_region(b"a");
    let txn_ext = cluster
        .must_get_snapshot_of_region(region.id)
        .ext()
        .get_txn_ext()
        .unwrap()
        .clone();
    assert_eq!(
        txn_ext.pessimistic_locks.read().get(&Key::from_raw(b"a")),
        Some(&(lock_a, false))
    );

    let region = cluster.get_region(b"c");
    let txn_ext = cluster
        .must_get_snapshot_of_region(region.id)
        .ext()
        .get_txn_ext()
        .unwrap()
        .clone();
    assert_eq!(
        txn_ext.pessimistic_locks.read().get(&Key::from_raw(b"c")),
        Some(&(lock_c, false))
    );
}

#[test]
fn test_refresh_region_bucket_keys() {
    let count = 5;
    let mut cluster = new_server_cluster(0, count);
    cluster.run();
    let pd_client = Arc::clone(&cluster.pd_client);

    // case: init bucket info
    cluster.must_put(b"k11", b"v1");
    let mut region = pd_client.get_region(b"k11").unwrap();
    let bucket = Bucket {
        keys: vec![b"k11".to_vec()],
        size: 1024 * 1024 * 200,
    };
    let mut expected_buckets = metapb::Buckets::default();
    expected_buckets.set_keys(bucket.clone().keys.into());
    expected_buckets
        .keys
        .insert(0, region.get_start_key().to_vec());
    expected_buckets.keys.push(region.get_end_key().to_vec());
    let buckets = vec![bucket];
    let bucket_version = cluster.refresh_region_bucket_keys(
        &region,
        buckets,
        Option::None,
        Some(expected_buckets.clone()),
    );

    // case: bucket range should refresh if epoch changed
    let conf_ver = region.get_region_epoch().get_conf_ver() + 1;
    region.mut_region_epoch().set_conf_ver(conf_ver);

    let bucket = Bucket {
        keys: vec![b"k12".to_vec()],
        size: 1024 * 1024 * 200,
    };
    expected_buckets.set_keys(bucket.clone().keys.into());
    expected_buckets
        .keys
        .insert(0, region.get_start_key().to_vec());
    expected_buckets.keys.push(region.get_end_key().to_vec());
    let buckets = vec![bucket];
    let bucket_version2 = cluster.refresh_region_bucket_keys(
        &region,
        buckets.clone(),
        Option::None,
        Some(expected_buckets.clone()),
    );
    assert_eq!(bucket_version2, bucket_version + 1);

    // case: stale epoch will not refresh buckets info
    let conf_ver = 0;
    region.mut_region_epoch().set_conf_ver(conf_ver);
    let bucket_version3 = cluster.refresh_region_bucket_keys(
        &region,
        buckets,
        Option::None,
        Some(expected_buckets.clone()),
    );
    assert_eq!(bucket_version3, bucket_version2);

    // case: bucket split
    // now the buckets is ["", "k12", ""]. further split ["", k12], [k12, ""]
    // buckets into more buckets
    let region = pd_client.get_region(b"k11").unwrap();
    let bucket_ranges = vec![
        BucketRange(vec![], b"k12".to_vec()),
        BucketRange(b"k12".to_vec(), vec![]),
    ];
    let buckets = vec![
        Bucket {
            keys: vec![b"k0".to_vec(), b"k10".to_vec(), b"k11".to_vec()],
            size: 1024 * 1024 * 200,
        },
        Bucket {
            keys: vec![b"k121".to_vec(), b"k122".to_vec()],
            size: 1024 * 1024 * 200,
        },
    ];
    expected_buckets.set_keys(
        vec![
            vec![],
            b"k0".to_vec(),
            b"k10".to_vec(),
            b"k11".to_vec(),
            b"k12".to_vec(),
            b"k121".to_vec(),
            b"k122".to_vec(),
            vec![],
        ]
        .into(),
    );
    let bucket_version4 = cluster.refresh_region_bucket_keys(
        &region,
        buckets,
        Some(bucket_ranges),
        Some(expected_buckets.clone()),
    );
    assert_eq!(bucket_version4, bucket_version3 + 1);

    // case: merge buckets
    // remove k11~k12, k12~k121, k122~[] bucket
    let buckets = vec![
        Bucket {
            keys: vec![],
            size: 1, // small enough to merge with left bucket
        },
        Bucket {
            keys: vec![],
            size: 1024 * 1024 * 65, // not small enough to merge with left
        },
        Bucket {
            keys: vec![],
            size: 1024 * 1024, // small enough to merge with left bucket
        },
    ];

    let bucket_ranges = vec![
        BucketRange(b"k11".to_vec(), b"k12".to_vec()),
        BucketRange(b"k121".to_vec(), b"k122".to_vec()),
        BucketRange(b"k122".to_vec(), vec![]),
    ];
    expected_buckets.set_keys(
        vec![
            vec![],
            b"k0".to_vec(),
            b"k10".to_vec(),
            b"k12".to_vec(),
            b"k121".to_vec(), // k121~k122 cannot be merged to left as it's too big
            vec![],
        ]
        .into(),
    );
    let bucket_version5 = cluster.refresh_region_bucket_keys(
        &region,
        buckets,
        Some(bucket_ranges),
        Some(expected_buckets.clone()),
    );

    assert_eq!(bucket_version5, bucket_version4 + 1);

    // case: split the region
    pd_client.must_split_region(region, pdpb::CheckPolicy::Usekey, vec![b"k11".to_vec()]);
    let mut buckets = vec![Bucket {
        keys: vec![b"k10".to_vec()],
        size: 1024 * 1024 * 65, // not small enough to merge with left
    }];

    expected_buckets.set_keys(vec![vec![], b"k10".to_vec(), b"k11".to_vec()].into());

    let mut region = pd_client.get_region(b"k10").unwrap();
    let left_id = region.get_id();
    let right = pd_client.get_region(b"k12").unwrap();
    if region.get_id() != 1 {
        region = right.clone();
        buckets = vec![Bucket {
            keys: vec![b"k12".to_vec()],
            size: 1024 * 1024 * 65, // not small enough to merge with left
        }];
        expected_buckets.set_keys(vec![b"k11".to_vec(), b"k12".to_vec(), vec![]].into());
    }

    let bucket_version6 =
        cluster.refresh_region_bucket_keys(&region, buckets, None, Some(expected_buckets.clone()));
    assert_eq!(bucket_version6, bucket_version5 + 1);

    // case: merge the region
    pd_client.must_merge(left_id, right.get_id());
    let region = pd_client.get_region(b"k10").unwrap();
    let buckets = vec![Bucket {
        keys: vec![b"k10".to_vec()],
        size: 1024 * 1024 * 65, // not small enough to merge with left
    }];

    expected_buckets.set_keys(vec![vec![], b"k10".to_vec(), vec![]].into());
    let bucket_version7 =
        cluster.refresh_region_bucket_keys(&region, buckets, None, Some(expected_buckets.clone()));
    assert_eq!(bucket_version7, bucket_version6 + 1);

    // case: nothing changed
    let bucket_version8 = cluster.refresh_region_bucket_keys(
        &region,
        vec![],
        Some(vec![]),
        Some(expected_buckets.clone()),
    );
    // no change on buckets, the bucket version is not changed.
    assert_eq!(bucket_version8, bucket_version7)
}

#[test]
fn test_gen_split_check_bucket_ranges() {
    let mut cluster = new_server_cluster(0, 1);
    let region_bucket_size = ReadableSize::kb(1);
    cluster.cfg.coprocessor.region_bucket_size = region_bucket_size;
    cluster.cfg.coprocessor.enable_region_bucket = Some(true);
    // disable report buckets; as it will reset the user traffic stats to randomize
    // the test result
    cluster.cfg.raft_store.check_leader_lease_interval = ReadableDuration::secs(5);
    // Make merge check resume quickly.
    cluster.cfg.raft_store.merge_check_tick_interval = ReadableDuration::millis(100);
    cluster.run();
    let pd_client = Arc::clone(&cluster.pd_client);

    let mut range = 1..;
    let mid_key = put_till_size(&mut cluster, region_bucket_size.0, &mut range);
    let second_key = put_till_size(&mut cluster, region_bucket_size.0, &mut range);
    let region = pd_client.get_region(&second_key).unwrap();

    let bucket = Bucket {
        keys: vec![mid_key.clone()],
        size: region_bucket_size.0 * 2,
    };
    let mut expected_buckets = metapb::Buckets::default();
    expected_buckets.set_keys(bucket.clone().keys.into());
    expected_buckets
        .keys
        .insert(0, region.get_start_key().to_vec());
    expected_buckets.keys.push(region.get_end_key().to_vec());
    let buckets = vec![bucket];

    // initialize fsm.peer.bucket_regions
    cluster.refresh_region_bucket_keys(
        &region,
        buckets.clone(),
        Option::None,
        Some(expected_buckets.clone()),
    );

    // put some data into the right buckets, so the bucket range will be check by
    // split check.
    let latest_key = put_till_size(&mut cluster, region_bucket_size.0 + 100, &mut range);
    let expected_bucket_ranges = vec![BucketRange(mid_key.clone(), vec![])];
    cluster.send_half_split_region_message(&region, Some(expected_bucket_ranges));

    // reset bucket stats.
    cluster.refresh_region_bucket_keys(
        &region,
        buckets,
        Option::None,
        Some(expected_buckets.clone()),
    );

    thread::sleep(Duration::from_millis(100));
    cluster.send_half_split_region_message(&region, Some(vec![]));

    // split the region
    pd_client.must_split_region(region, pdpb::CheckPolicy::Usekey, vec![second_key]);
    let left = pd_client.get_region(&mid_key).unwrap();
    let right = pd_client.get_region(&latest_key).unwrap();
    if right.get_id() == 1 {
        // the bucket_ranges should be None to refresh the bucket
        cluster.send_half_split_region_message(&right, None);
    } else {
        // the bucket_ranges should be None to refresh the bucket
        cluster.send_half_split_region_message(&left, None);
    }
    thread::sleep(Duration::from_millis(300));
    // merge the region
    pd_client.must_merge(left.get_id(), right.get_id());
    let region = pd_client.get_region(&mid_key).unwrap();
    cluster.send_half_split_region_message(&region, None);
}

#[test_case(test_raftstore::new_server_cluster)]
#[test_case(test_raftstore_v2::new_server_cluster)]
fn test_catch_up_peers_after_split() {
    let mut cluster = new_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    cluster.run();

    let left_key = b"k1";
    let right_key = b"k3";
    let split_key = b"k2";
    cluster.must_put(left_key, b"v1");
    cluster.must_put(right_key, b"v3");

    // Left and right key must be in same region before split.
    let region = pd_client.get_region(left_key).unwrap();
    let region2 = pd_client.get_region(right_key).unwrap();
    assert_eq!(region.get_id(), region2.get_id());

    // Split with split_key, so left_key must in left, and right_key in right.
    cluster.must_split(&region, split_key);

    // Get new split region by right_key because default right_derive is false.
    let right_region = pd_client.get_region(right_key).unwrap();

    let pending_peers = pd_client.get_pending_peers();

    // Ensure new split region has no pending peers.
    for p in right_region.get_peers() {
        assert!(!pending_peers.contains_key(&p.id))
    }
}

#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_split_region_keep_records() {
    let mut cluster = new_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    pd_client.must_add_peer(r1, new_peer(2, 2));
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    pd_client.must_remove_peer(r1, new_peer(2, 2));

    let leader = cluster.leader_of_region(r1).unwrap();
    cluster.add_send_filter_on_node(
        leader.get_store_id(),
        Box::new(DropMessageFilter::new(Arc::new(|m: &RaftMessage| {
            // Drop all gc peer requests and responses.
            !(m.has_extra_msg()
                && (m.get_extra_msg().get_type() == ExtraMessageType::MsgGcPeerRequest
                    || m.get_extra_msg().get_type() == ExtraMessageType::MsgGcPeerResponse))
        }))),
    );

    // Make sure split has applied.
    let region = pd_client.get_region(b"").unwrap();
    cluster.must_split(&region, b"k1");
    cluster.must_put(b"k2", b"v2");
    cluster.must_put(b"k0", b"v0");

    let region_state = cluster.region_local_state(r1, leader.get_store_id());
    assert!(
        !region_state.get_removed_records().is_empty(),
        "{:?}",
        region_state
    );
}

#[test_case(test_raftstore::new_node_cluster)]
#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_node_slow_split_does_not_cause_snapshot() {
    // We use three nodes([1, 2, 3]) for this test.
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, None, Some(5000));
    cluster.cfg.raft_store.snap_wait_split_duration = ReadableDuration::hours(1);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();

    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_peer(3, 3));
    cluster.must_transfer_leader(region_id, new_peer(3, 3));
    cluster.must_put(b"k2", b"v2");
    cluster.must_transfer_leader(region_id, new_peer(1, 1));

    // isolate node 3 for region 1.
    cluster.add_recv_filter_on_node(3, Box::new(RegionPacketFilter::new(1, 3)));

    let (notify_tx, notify_rx) = std::sync::mpsc::channel();
    cluster.add_send_filter_on_node(
        1,
        Box::new(MessageTypeNotifier::new(
            MessageType::MsgSnapshot,
            notify_tx,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )),
    );

    // split (-inf, +inf) -> (-inf, k1), [k1, +inf]
    let region = pd_client.get_region(b"").unwrap();
    cluster.must_split(&region, b"k1");

    // Leader must not send snapshot to new peer on node 3.
    notify_rx.recv_timeout(Duration::from_secs(3)).unwrap_err();
    cluster.must_put(b"k0", b"v0");
    // ... even after node 3 applied split.
    cluster.clear_recv_filter_on_node(3);

    let new_region = pd_client.get_region(b"").unwrap();
    let new_peer3 = find_peer(&new_region, 3).unwrap();
    cluster.must_transfer_leader(new_region.get_id(), new_peer3.clone());

    notify_rx.try_recv().unwrap_err();
}

#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_node_slow_split_does_not_prevent_snapshot() {
    // We use three nodes([1, 2, 3]) for this test.
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, None, Some(5000));
    cluster.cfg.raft_store.snap_wait_split_duration = ReadableDuration::secs(2);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();

    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_peer(3, 3));
    cluster.must_transfer_leader(region_id, new_peer(3, 3));
    cluster.must_put(b"k2", b"v2");
    cluster.must_transfer_leader(region_id, new_peer(1, 1));

    // isolate node 3 for region 1.
    cluster.add_recv_filter_on_node(3, Box::new(RegionPacketFilter::new(1, 3)));

    let (notify_tx, notify_rx) = std::sync::mpsc::channel();
    cluster.add_send_filter_on_node(
        1,
        Box::new(MessageTypeNotifier::new(
            MessageType::MsgSnapshot,
            notify_tx,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )),
    );

    // split (-inf, +inf) -> (-inf, k1), [k1, +inf]
    let region = pd_client.get_region(b"").unwrap();
    cluster.must_split(&region, b"k1");

    // Leader must not send snapshot to new peer on node 3.
    notify_rx
        .recv_timeout(cluster.cfg.raft_store.snap_wait_split_duration.0 / 2)
        .unwrap_err();

    // A follower can receive a snapshot from leader if split is really slow.
    thread::sleep(2 * cluster.cfg.raft_store.snap_wait_split_duration.0);
    let new_region = pd_client.get_region(b"").unwrap();
    let new_peer3 = find_peer(&new_region, 3).unwrap();
    cluster.must_transfer_leader(new_region.get_id(), new_peer3.clone());

    notify_rx.try_recv().unwrap();
}

#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_node_slow_split_does_not_prevent_leader_election() {
    // We use three nodes([1, 2, 3]) for this test.
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, None, Some(5000));
    cluster.cfg.raft_store.snap_wait_split_duration = ReadableDuration::hours(1);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();

    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_peer(3, 3));

    // Do not let node 2 and 3 split.
    cluster.add_recv_filter_on_node(2, Box::new(EraseHeartbeatCommit));
    cluster.add_recv_filter_on_node(3, Box::new(EraseHeartbeatCommit));

    let (notify_tx, notify_rx) = std::sync::mpsc::channel();
    cluster.add_recv_filter_on_node(
        1,
        Box::new(MessageTypeNotifier::new(
            MessageType::MsgRequestVoteResponse,
            notify_tx,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )),
    );

    // split (-inf, +inf) -> (-inf, k1), [k1, +inf]
    let region = pd_client.get_region(b"").unwrap();
    cluster.must_split(&region, b"k1");

    // Node 1 must receive request vote response twice.
    notify_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    notify_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    cluster.must_put(b"k0", b"v0");
}

// A filter that disable read index by heartbeat.
#[derive(Clone)]
struct EraseHeartbeatContext;

impl Filter for EraseHeartbeatContext {
    fn before(&self, msgs: &mut Vec<RaftMessage>) -> Result<()> {
        for msg in msgs {
            if msg.get_message().get_msg_type() == MessageType::MsgHeartbeat {
                msg.mut_message().clear_context();
            }
        }
        Ok(())
    }
}

#[test_case(test_raftstore_v2::new_node_cluster)]
fn test_node_split_during_read_index() {
    let mut cluster = new_cluster(0, 3);
    configure_for_lease_read(&mut cluster.cfg, None, Some(5000));
    cluster.cfg.raft_store.snap_wait_split_duration = ReadableDuration::hours(1);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();

    pd_client.must_add_peer(region_id, new_peer(2, 2));
    pd_client.must_add_peer(region_id, new_peer(3, 3));

    let region = cluster.get_region(b"");

    // Delay read index.
    cluster.add_recv_filter_on_node(2, Box::new(EraseHeartbeatContext));
    cluster.add_recv_filter_on_node(3, Box::new(EraseHeartbeatContext));
    let mut request = new_request(
        region.get_id(),
        region.get_region_epoch().clone(),
        vec![new_read_index_cmd()],
        true,
    );
    request.mut_header().set_peer(new_peer(1, 1));
    let (msg, sub) = raftstore_v2::router::PeerMsg::raft_query(request);
    cluster
        .sim
        .rl()
        .async_peer_msg_on_node(1, region.get_id(), msg)
        .unwrap();

    cluster.must_split(&region, b"a");

    // Enable read index
    cluster.clear_recv_filter_on_node(2);
    cluster.clear_recv_filter_on_node(3);

    match block_on_timeout(sub.result(), Duration::from_secs(5)) {
        Ok(Some(QueryResult::Response(resp))) if resp.get_header().has_error() => {}
        other => {
            panic!("{:?}", other);
        }
    }
}

#[test_case(test_raftstore::new_node_cluster)]
fn test_clear_uncampaigned_regions_after_split() {
    let mut cluster = new_cluster(0, 3);
    cluster.cfg.raft_store.raft_base_tick_interval = ReadableDuration::millis(50);
    cluster.cfg.raft_store.raft_election_timeout_ticks = 10;

    let pd_client = cluster.pd_client.clone();
    pd_client.disable_default_operator();

    cluster.run();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k2", b"v2");
    cluster.must_put(b"k3", b"v3");
    // Transfer leader to peer 3.
    let region = pd_client.get_region(b"k2").unwrap();
    cluster.must_transfer_leader(region.get_id(), new_peer(3, 3));

    // New split regions will be recorded into uncampaigned region list of
    // followers (in peer 1 and peer 2).
    cluster.split_region(
        &region,
        b"k2",
        Callback::write(Box::new(move |_write_resp: WriteResponse| {})),
    );
    // Wait the old lease of the leader timeout and followers clear its
    // uncampaigned region list.
    thread::sleep(
        cluster.cfg.raft_store.raft_base_tick_interval.0
            * cluster.cfg.raft_store.raft_election_timeout_ticks as u32
            * 3,
    );
    // The leader of the parent region should still be peer 3 as no
    // other peers can become leader.
    cluster.reset_leader_of_region(region.get_id());
    assert_eq!(
        cluster.leader_of_region(region.get_id()).unwrap(),
        new_peer(3, 3)
    );
}

#[test]
fn test_split_init_raft_state_recovery() {
    let mut cluster = new_node_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    cluster.run();
    cluster.must_put(b"k2", b"v2");
    let region = cluster.get_region(b"k2");

    // Block prevote so that new peers do not update and persist raft state.
    cluster.add_send_filter(CloneFilterFactory(DropMessageFilter::new(Arc::new(
        move |m| {
            let msg_type = m.get_message().get_msg_type();
            msg_type != MessageType::MsgRequestPreVote
        },
    ))));

    // Split region
    cluster.split_region(&region, b"k2", Callback::None);
    // New regions are created with region id 1000.
    let new_region_id = 1000;
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        let region_state: Option<RegionLocalState> = cluster
            .get_engine(2)
            .get_msg_cf(CF_RAFT, &keys::region_state_key(new_region_id))
            .unwrap();
        region_state.is_some()
    });
    let new_region = cluster.get_region(b"");
    assert_eq!(new_region.get_id(), new_region_id);
    let new_peer_on_store_2 = find_peer(&new_region, 2).unwrap().to_owned();

    // Restart node 2.
    cluster.stop_node(2);

    // Make sure raft_state is not persisted.
    let raft_state = cluster
        .get_raft_engine(2)
        .get_raft_state(new_region_id)
        .unwrap();
    assert!(
        raft_state.is_none(),
        "raft state should not be persisted: {:?}",
        raft_state
    );

    cluster.run_node(2).unwrap();

    cluster.clear_send_filters();

    // Make sure raft_state is recovered.
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        let raft_state = cluster
            .get_raft_engine(2)
            .get_raft_state(new_region_id)
            .unwrap();
        raft_state.is_some()
    });
    cluster.must_transfer_leader(new_region_id, new_peer_on_store_2);
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");
}

#[test]
fn test_split_init_raft_state_overwritten_by_snapshot() {
    let mut cluster = new_node_cluster(0, 3);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();
    cluster.run();
    let region = cluster.get_region(b"k2");
    let peer1 = find_peer(&region, 1).unwrap().to_owned();
    cluster.must_transfer_leader(region.get_id(), peer1);
    cluster.must_put(b"k2", b"v2");

    // New regions are created with region id 1000.
    let new_region_id = 1000;

    // Block messages to new regions on store 2 so that it does not update and
    // persist raft state.
    cluster.add_recv_filter_on_node(
        2,
        Box::new(DropMessageFilter::new(Arc::new(move |m| {
            m.get_region_id() != new_region_id
        }))),
    );

    // Split region
    cluster.split_region(&region, b"k2", Callback::None);
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        let region_state: Option<RegionLocalState> = cluster
            .get_engine(2)
            .get_msg_cf(CF_RAFT, &keys::region_state_key(new_region_id))
            .unwrap();
        region_state.is_some()
    });

    let new_region = cluster.get_region(b"");
    assert_eq!(new_region.get_id(), new_region_id);
    let new_peer_on_store_2 = find_peer(&new_region, 2).unwrap().to_owned();
    pd_client.must_remove_peer(new_region_id, new_peer_on_store_2.clone());
    let mut new_peer_on_store_2_successor = new_peer_on_store_2.clone();
    new_peer_on_store_2_successor.set_id(pd_client.alloc_id().unwrap());
    pd_client.must_add_peer(new_region_id, new_peer_on_store_2_successor.clone());

    // Make sure raft_state is not persisted.
    let raft_state = cluster
        .get_raft_engine(2)
        .get_raft_state(new_region_id)
        .unwrap();
    assert!(
        raft_state.is_none(),
        "raft state should not be persisted: {:?}",
        raft_state
    );

    cluster.clear_recv_filter_on_node(2);

    // Make sure raft_state is persisted.
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        let raft_state = cluster
            .get_raft_engine(2)
            .get_raft_state(new_region_id)
            .unwrap();
        raft_state.is_some()
    });

    cluster.must_transfer_leader(new_region_id, new_peer_on_store_2_successor);
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");
}
