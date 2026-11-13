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

/// Picks up to `count` disks for new copies.
///
/// Disks already holding a copy (`existing`) are never picked. Disks on nodes that hold no copy
/// yet are preferred, so copies land on distinct nodes whenever possible; only when there are not
/// enough nodes does a node get a second copy (on a different disk). Within each tier, disks
/// with more free space win.
pub fn choose(
    mut candidates: Vec<Candidate<'_>>,
    existing: &[(DiskId, &str)],
    count: usize,
) -> Vec<DiskId> {
    let mut used_disks: HashSet<DiskId> = existing.iter().map(|(d, _)| *d).collect();
    let mut used_nodes: HashSet<&str> = existing.iter().map(|(_, n)| *n).collect();
    candidates.retain(|c| !used_disks.contains(&c.disk_id));
    // Stable sort keeps config order as the tie-breaker.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.free));

    let mut picked = Vec::with_capacity(count);
    for c in &candidates {
        if picked.len() == count {
            return picked;
        }
        if used_nodes.insert(c.node_id) {
            used_disks.insert(c.disk_id);
            picked.push(c.disk_id);
        }
    }
    for c in &candidates {
        if picked.len() == count {
            break;
        }
        if used_disks.insert(c.disk_id) {
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
        let picked = choose(cs, &[], 3);
        assert_eq!(picked, vec![ids[0], ids[2], ids[3]]);
    }

    #[test]
    fn falls_back_to_distinct_disks_on_one_node() {
        let cs = vec![cand("a", 1), cand("a", 3), cand("a", 2)];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        let picked = choose(cs, &[], 3);
        assert_eq!(picked, vec![ids[1], ids[2], ids[0]]);
    }

    #[test]
    fn skips_disks_and_nodes_that_already_hold_a_copy() {
        let cs = vec![cand("a", 9), cand("b", 5), cand("c", 1)];
        let ids: Vec<_> = cs.iter().map(|c| c.disk_id).collect();
        let picked = choose(cs, &[(ids[0], "a")], 1);
        assert_eq!(picked, vec![ids[1]]);
    }

    #[test]
    fn returns_fewer_when_not_enough_disks() {
        let cs = vec![cand("a", 1)];
        assert_eq!(choose(cs, &[], 3).len(), 1);
    }
}
