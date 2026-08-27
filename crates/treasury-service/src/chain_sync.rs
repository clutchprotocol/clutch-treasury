//! Is the node we read actually at the tip?
//!
//! A node that has fallen behind does not report an error. It answers `get_chain_info` cheerfully
//! with a stale height and a stale `total_supply`, and everything downstream treats that as the
//! state of the world. On stage, node1 sat ~115,000 blocks behind for a full day: reconciliation
//! judged against a supply frozen near genesis, the outbox submitted mints into it, and every
//! reading looked internally consistent. Nothing anywhere said "behind".
//!
//! One node cannot answer this about itself, so the check needs a second opinion: ask the peers,
//! take the highest height any of them reports, and see how far the primary trails it.
//!
//! # Why this fails OPEN when no peer answers
//!
//! Blocking on peer silence would stop minting every time a peer restarts, and on a three-node
//! stack every deploy restarts all of them. That trades a rare, detectable problem for a frequent,
//! self-inflicted one. So: if at least one peer answers, the comparison is enforced; if none does,
//! the lag is unknown and the caller is told so rather than being handed a fabricated zero.

use std::sync::Arc;

use clutch_chain::node_client::NodeClient;

/// How far the primary trails the best peer.
///
/// Saturating: a primary AHEAD of every peer is not negative lag, it is zero. That happens
/// routinely — the primary can hold a block the peers have not applied yet — and a signed
/// underflow here would read as an enormous lag and stop minting on a healthy node.
pub fn lag(primary_height: u64, peer_heights: &[u64]) -> u64 {
    match peer_heights.iter().copied().max() {
        Some(best) => best.saturating_sub(primary_height),
        None => 0,
    }
}

/// What the caller should do about the primary's height.
#[derive(Debug, PartialEq)]
pub enum SyncState {
    /// Within tolerance of the best peer.
    InSync { lag: u64 },
    /// Behind by more than the configured tolerance — do not act on what this node reports.
    Behind { lag: u64, primary: u64, best_peer: u64 },
    /// No peer could be reached, so there is nothing to compare against. NOT the same as
    /// in-sync, and deliberately a distinct variant so a caller cannot treat it as one by
    /// accident.
    Unknown,
}

/// Ask every peer for its height and compare against the primary.
///
/// Peers are queried in sequence rather than concurrently: there are three of them on the whole
/// stack, this runs once per outbox pass, and a join adds a dependency for no measurable gain.
pub async fn check(primary: &Arc<NodeClient>, peers: &[Arc<NodeClient>], tolerance: u64) -> SyncState {
    let primary_height = match primary.get_chain_info().await {
        Ok(i) => i.latest_block_index,
        // The primary being unreachable is a different problem, and every caller already handles
        // its own RPC failures. Nothing to compare, so nothing to say.
        Err(_) => return SyncState::Unknown,
    };

    let mut heights = Vec::new();
    for p in peers {
        if let Ok(info) = p.get_chain_info().await {
            heights.push(info.latest_block_index);
        }
    }
    if heights.is_empty() {
        return SyncState::Unknown;
    }

    let l = lag(primary_height, &heights);
    if l > tolerance {
        SyncState::Behind {
            lag: l,
            primary: primary_height,
            best_peer: heights.iter().copied().max().unwrap_or(primary_height),
        }
    } else {
        SyncState::InSync { lag: l }
    }
}

/// Build peer clients from a comma-separated list. Blank entries are skipped so a trailing comma
/// or an empty setting is simply "no peers", not a client pointed at "".
///
/// Must be called from inside a Tokio runtime: NodeClient::new spawns its connection task.
pub fn peer_clients(csv: &str) -> Vec<Arc<NodeClient>> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        // NodeClient::new already hands back an Arc -- wrapping it again type-errors.
        .map(|url| NodeClient::new(url.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lag_is_the_gap_to_the_best_peer() {
        assert_eq!(lag(100, &[150, 120, 90]), 50);
    }

    /// The failure this exists for: node1 at 2,408 while node2 and node3 were at 117,573.
    #[test]
    fn the_stage_failure_is_a_large_lag() {
        assert_eq!(lag(2_408, &[117_573, 117_573]), 115_165);
    }

    /// A primary ahead of its peers is normal — it can hold a block they have not applied yet.
    /// Signed arithmetic here would underflow into an enormous lag and stop minting on a healthy
    /// node, which is the opposite of what this is for.
    #[test]
    fn a_primary_ahead_of_its_peers_has_no_lag() {
        assert_eq!(lag(200, &[199, 150]), 0);
        assert_eq!(lag(u64::MAX, &[0]), 0);
    }

    #[test]
    fn no_peers_means_no_measurable_lag() {
        assert_eq!(lag(100, &[]), 0);
    }

    /// Async because NodeClient::new spawns onto the runtime — constructing one outside a Tokio
    /// context panics, which is worth knowing before calling this from anywhere new.
    #[tokio::test]
    async fn blank_and_trailing_entries_do_not_become_clients() {
        assert_eq!(peer_clients("").len(), 0);
        assert_eq!(peer_clients("  ").len(), 0);
        assert_eq!(peer_clients("ws://a/ws,").len(), 1);
        assert_eq!(peer_clients("ws://a/ws, ws://b/ws").len(), 2);
    }
}
