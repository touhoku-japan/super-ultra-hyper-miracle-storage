//! Chooses which disks receive the copies of an object.

use std::collections::HashSet;

use crate::disk::DiskId;

/// Copies that must be durable before a write is acknowledged: a majority of the replicas
/// (1 of 1, 2 of 2, 2 of 3, 3 of 4, ...).
pub fn ack_threshold(replicas: u8) -> usize {
    usize::from(replicas) / 2 + 1
}

#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub disk_id: DiskId,
    pub node_id: &'a str,
    pub free: u64,
}

/// Disks whose free space is within this fraction of the emptiest eligible disk count as equally
/// empty (see [`choose`]).
const TIE_FRACTION: u64 = 20; // 1/20 = 5%

/// Picks up to `count` disks for new copies.
///
/// Disks already holding a copy (`existing`) are never picked, and copies go to distinct nodes.
/// Only with `allow_same_node` (single-node deployments) may a node get several copies, on
/// different disks. In a cluster that is short of live nodes this returns fewer disks than
/// requested: a second copy on the same node would never be moved once the missing node is back.
///
/// Disks with more free space win, but disks within 5% of the emptiest one are picked among at
/// random. Free space is only refreshed by heartbeats, so with a strict order every write until
/// the next heartbeat would land on the same disk of each node (identical disks of a fresh node
/// differ by a few bytes), leaving the node's other disks idle.
pub fn choose(
    candidates: Vec<Candidate<'_>>,
    existing: &[(DiskId, &str)],
    count: usize,
    allow_same_node: bool,
) -> Vec<DiskId> {
    choose_with(candidates, existing, count, allow_same_node, || {
        // uuid's v4 generator is the random source the crate already has.
        let r = uuid::Uuid::new_v4().as_u128();
        (r >> 64) as u64 ^ r as u64
    })
}

/// [`choose`] with the random source given (`random` returns uniformly distributed values).
pub fn choose_with(
    mut candidates: Vec<Candidate<'_>>,
    existing: &[(DiskId, &str)],
    count: usize,
    allow_same_node: bool,
    mut random: impl FnMut() -> u64,
) -> Vec<DiskId> {
    let mut used_disks: HashSet<DiskId> = existing.iter().map(|(d, _)| *d).collect();
    let mut used_nodes: HashSet<&str> = existing.iter().map(|(_, n)| *n).collect();
    candidates.retain(|c| !used_disks.contains(&c.disk_id));

    let mut picked = Vec::with_capacity(count);
    // First one copy per node; then, in a single-node deployment, more disks of the same node.
    for same_node in [false, true] {
        if same_node && !allow_same_node {
            break;
        }
        while picked.len() < count {
            let eligible: Vec<&Candidate<'_>> = candidates
                .iter()
                .filter(|c| !used_disks.contains(&c.disk_id))
                .filter(|c| same_node || !used_nodes.contains(c.node_id))
                .collect();
            let Some(best) = eligible.iter().map(|c| c.free).max() else {
                break;
            };
            let floor = best - best / TIE_FRACTION;
            let tier: Vec<&&Candidate<'_>> = eligible.iter().filter(|c| c.free >= floor).collect();
            let c = tier[(random() % tier.len() as u64) as usize];
            used_disks.insert(c.disk_id);
            used_nodes.insert(c.node_id);
            picked.push(c.disk_id);
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn cand(node: &str, free: u64) -> Candidate<'_> {
        Candidate {
            disk_id: Uuid::new_v4(),
            node_id: node,
            free,
        }
    }

    #[test]
    fn ack_threshold_is_majority() {
        assert_eq!(ack_threshold(1), 1);
        assert_eq!(ack_threshold(2), 2);
        assert_eq!(ack_threshold(3), 2);
        assert_eq!(ack_threshold(4), 3);
    }

    #[test]
    fn spreads_across_nodes_before_reusing_one() {
        // Node a has the two emptiest disks, but copies must still go to b and c.
        let cs = vec![
            cand("a", 900),
            cand("a", 800),
            cand("b", 100),
            cand("c", 50),
        ];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        let picked = choose(cs, &[], 3, false);
        assert_eq!(picked, vec![ids[0], ids[2], ids[3]]);
    }

    #[test]
    fn falls_back_to_distinct_disks_on_one_node() {
        let cs = vec![cand("a", 1), cand("a", 3), cand("a", 2)];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        let picked = choose(cs, &[], 3, true);
        assert_eq!(picked, vec![ids[1], ids[2], ids[0]]);
    }

    #[test]
    fn skips_disks_and_nodes_that_already_hold_a_copy() {
        let cs = vec![cand("a", 9), cand("b", 5), cand("c", 1)];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        let picked = choose(cs, &[(ids[0], "a")], 1, false);
        assert_eq!(picked, vec![ids[1]]);
    }

    #[test]
    fn never_doubles_up_on_a_node_in_a_cluster() {
        // node c is down: only a and b are candidates.
        let cs = vec![cand("a", 9), cand("a", 8), cand("b", 1)];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        assert_eq!(choose(cs, &[], 3, false), vec![ids[0], ids[2]]);
    }

    #[test]
    fn returns_fewer_when_not_enough_disks() {
        let cs = vec![cand("a", 1)];
        assert_eq!(choose(cs, &[], 3, true).len(), 1);
    }

    #[test]
    fn nearly_equal_disks_share_the_writes() {
        // Four identical disks of one node (differing by a few bytes), as on a fresh node.
        let cs = vec![
            cand("a", 1_000_000),
            cand("a", 999_990),
            cand("a", 1_000_005),
            cand("a", 999_999),
        ];
        let mut counts = std::collections::HashMap::new();
        for _ in 0..400 {
            let picked = choose(cs.clone(), &[], 1, true);
            *counts.entry(picked[0]).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 4, "{counts:?}");
        assert!(counts.values().all(|n| *n > 50), "{counts:?}");
    }

    #[test]
    fn a_clearly_emptier_disk_still_wins() {
        // A disk added later: much more free space than the others, so it takes every copy.
        let cs = vec![cand("a", 500), cand("a", 520), cand("a", 1000)];
        let new = cs[2].disk_id;
        for _ in 0..50 {
            assert_eq!(choose(cs.clone(), &[], 1, true), vec![new]);
        }
    }

    #[test]
    fn ties_are_broken_by_the_random_source() {
        let cs = vec![cand("a", 100), cand("b", 99), cand("c", 98), cand("d", 10)];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        // The tier is a, b, c (d is far behind); index 2 picks c, then index 1 of a, b picks b.
        let mut draws = [2u64, 1, 0].into_iter();
        let picked = choose_with(cs, &[], 3, false, || draws.next().unwrap());
        assert_eq!(picked, vec![ids[2], ids[1], ids[0]]);
    }
}
