// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    num::NonZeroUsize,
    sync::{Arc, mpsc::channel},
    thread,
    time::Duration,
};

use collections::HashMap;
use kvproto::metapb::Region;
use more_asserts::{assert_gt, assert_le};
use pd_client::{RegionStat, RegionWriteCfCopDetail};
use raftstore::coprocessor::{
    RegionInfoAccessor, RegionInfoProvider,
    region_info_accessor::{RaftStoreEvent, RegionActivity, RegionInfoQuery},
};
use test_raftstore::*;
use tikv_util::HandyRwLock;

fn prepare_cluster<T: Simulator>(cluster: &mut Cluster<T>) -> Vec<Region> {
    for i in 0..15 {
        let i = i + b'0';
        let key = vec![b'k', i];
        let value = vec![b'v', i];
        cluster.must_put(&key, &value);
    }

    let end_keys = [
        b"k1".to_vec(),
        b"k3".to_vec(),
        b"k5".to_vec(),
        b"k7".to_vec(),
        b"k9".to_vec(),
        b"".to_vec(),
    ];

    let start_keys = [
        b"".to_vec(),
        b"k1".to_vec(),
        b"k3".to_vec(),
        b"k5".to_vec(),
        b"k7".to_vec(),
        b"k9".to_vec(),
    ];

    let mut regions = Vec::new();

    for mut key in end_keys.iter().take(end_keys.len() - 1).cloned() {
        let region = cluster.get_region(&key);
        cluster.must_split(&region, &key);

        key[1] -= 1;
        let region = cluster.get_region(&key);
        regions.push(region);
    }
    regions.push(cluster.get_region(b"k9"));

    assert_eq!(regions.len(), end_keys.len());
    assert_eq!(regions.len(), start_keys.len());
    for i in 0..regions.len() {
        assert_eq!(regions[i].start_key, start_keys[i]);
        assert_eq!(regions[i].end_key, end_keys[i]);
    }

    // Wait for raftstore to update regions
    thread::sleep(Duration::from_secs(2));
    regions
}

#[test]
fn test_region_collection_seek_region() {
    let mut cluster = new_node_cluster(0, 3);

    let (tx, rx) = channel();
    cluster
        .sim
        .wl()
        .post_create_coprocessor_host(Box::new(move |id, host| {
            let p = RegionInfoAccessor::new(host, Arc::new(|| false), Box::new(|| 0));
            tx.send((id, p)).unwrap()
        }));

    cluster.run();
    let region_info_providers: HashMap<_, _> = rx.try_iter().collect();
    assert_eq!(region_info_providers.len(), 3);
    let regions = prepare_cluster(&mut cluster);

    for node_id in cluster.get_node_ids() {
        let engine = &region_info_providers[&node_id];

        // Test traverse all regions
        let key = b"".to_vec();
        let (tx, rx) = channel();
        let tx_ = tx.clone();
        engine
            .seek_region(
                &key,
                Box::new(move |infos| {
                    tx_.send(infos.map(|i| i.region.clone()).collect()).unwrap();
                }),
            )
            .unwrap();
        let sought_regions: Vec<_> = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(sought_regions, regions);

        // Test end_key is exclusive
        let (tx, rx) = channel();
        let tx_ = tx.clone();
        engine
            .seek_region(
                b"k1",
                Box::new(move |infos| tx_.send(infos.next().unwrap().region.clone()).unwrap()),
            )
            .unwrap();
        let region = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(region, regions[1]);

        // Test seek from non-starting key
        let tx_ = tx.clone();
        engine
            .seek_region(
                b"k6\xff\xff\xff\xff\xff",
                Box::new(move |infos| tx_.send(infos.next().unwrap().region.clone()).unwrap()),
            )
            .unwrap();
        let region = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(region, regions[3]);
        let tx_ = tx.clone();
        engine
            .seek_region(
                b"\xff\xff\xff\xff\xff\xff\xff\xff",
                Box::new(move |infos| tx_.send(infos.next().unwrap().region.clone()).unwrap()),
            )
            .unwrap();
        let region = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(region, regions[5]);
    }

    for (_, p) in region_info_providers {
        p.stop();
    }
}

#[test]
fn test_region_collection_get_regions_in_range() {
    let mut cluster = new_node_cluster(0, 3);

    let (tx, rx) = channel();
    cluster
        .sim
        .wl()
        .post_create_coprocessor_host(Box::new(move |id, host| {
            let p = RegionInfoAccessor::new(host, Arc::new(|| false), Box::new(|| 0));
            tx.send((id, p)).unwrap()
        }));

    cluster.run();
    let region_info_providers: HashMap<_, _> = rx.try_iter().collect();
    assert_eq!(region_info_providers.len(), 3);
    let regions = prepare_cluster(&mut cluster);

    for node_id in cluster.get_node_ids() {
        let engine = &region_info_providers[&node_id];

        let result = engine.get_regions_in_range(b"", b"").unwrap();
        assert_eq!(result, regions);

        let result = engine.get_regions_in_range(b"k1", b"k3").unwrap();
        assert_eq!(&result, &regions[1..3]);

        let result = engine.get_regions_in_range(b"k3", b"k8").unwrap();
        assert_eq!(&result, &regions[2..5]);

        let result = engine.get_regions_in_range(b"k6", b"k8").unwrap();
        assert_eq!(&result, &regions[3..5]);

        let result = engine.get_regions_in_range(b"k7", b"k99").unwrap();
        assert_eq!(&result, &regions[4..6]);

        let result = engine.get_regions_in_range(b"k99", b"").unwrap();
        assert_eq!(&result, &regions[5..6]);
    }

    for (_, p) in region_info_providers {
        p.stop();
    }
}

#[test]
fn test_region_collection_get_top_regions() {
    let mut cluster = new_node_cluster(0, 3);

    let (tx, rx) = channel();
    cluster
        .sim
        .wl()
        .post_create_coprocessor_host(Box::new(move |id, host| {
            let p = RegionInfoAccessor::new(host, Arc::new(|| true), Box::new(|| 0));
            tx.send((id, p)).unwrap()
        }));
    cluster.run();
    let region_info_providers: HashMap<_, _> = rx.try_iter().collect();
    assert_eq!(region_info_providers.len(), 3);
    let regions = prepare_cluster(&mut cluster);
    let mut region_ids = regions.iter().map(|r| r.get_id()).collect::<Vec<_>>();
    region_ids.sort();
    for node_id in cluster.get_node_ids() {
        let engine = &region_info_providers[&node_id];
        for r in &regions {
            let mut region_stat = RegionStat::default();
            region_stat.cop_detail = RegionWriteCfCopDetail::new(10, 10, 10);
            let _ = engine.scheduler().schedule(RegionInfoQuery::RaftStoreEvent(
                RaftStoreEvent::UpdateRegionActivity {
                    region: r.clone(),
                    activity: RegionActivity { region_stat },
                },
            ));
        }

        let result = engine
            .get_top_regions(NonZeroUsize::new(10).unwrap())
            .unwrap()
            .into_iter()
            .map(|(r, _)| r.get_id())
            .collect::<Vec<_>>();

        for region_id in &result {
            assert!(region_ids.contains(region_id));
        }
        let len = result.len();
        if engine.region_leaders().read().unwrap().contains(&node_id) {
            // Assert that top regions are populated on a leader.
            assert_gt!(len, 0);
            assert_le!(len, 10);
        }
    }

    for (_, p) in region_info_providers {
        p.stop();
    }
}

#[test]
fn test_region_collection_find_region_by_key() {
    let mut cluster = new_node_cluster(0, 3);

    let (tx, rx) = channel();
    cluster
        .sim
        .wl()
        .post_create_coprocessor_host(Box::new(move |id, host| {
            let p = RegionInfoAccessor::new(host, Arc::new(|| false), Box::new(|| 0));
            tx.send((id, p)).unwrap()
        }));

    cluster.run();
    let region_info_providers: HashMap<_, _> = rx.try_iter().collect();
    assert_eq!(region_info_providers.len(), 3);
    let regions = prepare_cluster(&mut cluster);

    for node_id in cluster.get_node_ids() {
        let engine = &region_info_providers[&node_id];

        let region = engine.find_region_by_key(b"").unwrap();
        assert_eq!(region, regions[0]);

        let region = engine.find_region_by_key(b"k2").unwrap();
        assert_eq!(region, regions[1]);

        let region = engine.find_region_by_key(b"k99").unwrap();
        assert_eq!(region, *regions.last().unwrap());
    }

    for (_, p) in region_info_providers {
        p.stop();
    }
}
