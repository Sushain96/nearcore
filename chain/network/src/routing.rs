use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use borsh::{BorshDeserialize, BorshSerialize};
use byteorder::WriteBytesExt;
use bytes::LittleEndian;
use log::debug;

use near_crypto::{SecretKey, Signature};
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::types::AccountId;

use crate::types::PeerId;

/// Information that will be ultimately used to create a new edge.
/// It contains nonce proposed for the edge with signature from peer.
#[derive(Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq, Debug, Default)]
pub struct EdgeInfo {
    pub nonce: u64,
    pub signature: Signature,
}

impl EdgeInfo {
    pub fn new(peer0: PeerId, peer1: PeerId, nonce: u64, secret_key: &SecretKey) -> Self {
        let (peer0, peer1) = Edge::key(peer0, peer1);
        let data = Edge::build_hash(&peer0, &peer1, nonce);
        let signature = secret_key.sign(data.as_ref());
        Self { nonce, signature }
    }
}

/// Status of the edge
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub enum EdgeType {
    Added,
    Removed,
}

// TODO(MarX): If two connected peers find an known edge between them with higher nonce in the network
//  they should update its nonce so that further edges are accepted among other members in the network.
//  This event can happen if both nodes get offline and loose persisted information.
/// Edge object. Contains information relative to a new edge that is being added or removed
/// from the network. This is the information that is required
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Since edges are not directed `peer0 < peer1` should hold.
    peer0: PeerId,
    peer1: PeerId,
    /// Nonce to keep tracking of the last update on this edge.
    /// It must be even
    pub nonce: u64,
    /// Signature from parties validating the edge. These are signature of the added edge.
    signature0: Signature,
    signature1: Signature,
    /// Info necessary to declare an edge as removed.
    /// The bool says which party is removing the edge: false for Peer0, true for Peer1
    /// The signature from the party removing the edge.
    removal_info: Option<(bool, Signature)>,
}

impl Edge {
    /// Create an addition edge.
    pub fn new(
        peer0: PeerId,
        peer1: PeerId,
        nonce: u64,
        signature0: Signature,
        signature1: Signature,
    ) -> Self {
        let (peer0, signature0, peer1, signature1) = if peer0 < peer1 {
            (peer0, signature0, peer1, signature1)
        } else {
            (peer1, signature1, peer0, signature0)
        };

        Self { peer0, peer1, nonce, signature0, signature1, removal_info: None }
    }

    /// Create the remove edge change from an added edge change.
    pub fn remove_edge(&self, me: PeerId, sk: SecretKey) -> Self {
        assert_eq!(self.edge_type(), EdgeType::Added);
        let mut edge = self.clone();
        edge.nonce += 1;
        let me = edge.peer0 == me;
        let hash = edge.hash();
        let signature = sk.sign(hash.as_ref());
        edge.removal_info = Some((me, signature));
        edge
    }

    fn build_hash(peer0: &PeerId, peer1: &PeerId, nonce: u64) -> CryptoHash {
        let mut buffer = Vec::<u8>::new();
        let peer0: Vec<u8> = peer0.clone().into();
        buffer.extend_from_slice(peer0.as_slice());
        let peer1: Vec<u8> = peer1.clone().into();
        buffer.extend_from_slice(peer1.as_slice());
        buffer.write_u64::<LittleEndian>(nonce).unwrap();
        hash(buffer.as_slice())
    }

    fn hash(&self) -> CryptoHash {
        Edge::build_hash(&self.peer0, &self.peer1, self.nonce)
    }

    fn prev_hash(&self) -> CryptoHash {
        Edge::build_hash(&self.peer0, &self.peer1, self.nonce - 1)
    }

    pub fn verify(&self) -> bool {
        if self.peer0 > self.peer1 {
            return false;
        }

        match self.edge_type() {
            EdgeType::Added => {
                let data = self.hash();

                self.removal_info.is_none()
                    && self.signature0.verify(data.as_ref(), &self.peer0.public_key())
                    && self.signature1.verify(data.as_ref(), &self.peer1.public_key())
            }
            EdgeType::Removed => {
                // nonce should be an even positive number
                if self.nonce == 0 {
                    return false;
                }

                // Check referring added edge is valid.
                let add_hash = self.prev_hash();
                if !self.signature0.verify(add_hash.as_ref(), &self.peer0.public_key())
                    || !self.signature1.verify(add_hash.as_ref(), &self.peer1.public_key())
                {
                    return false;
                }

                if let Some((party, signature)) = &self.removal_info {
                    let peer = if *party { &self.peer0 } else { &self.peer1 };
                    let del_hash = self.hash();
                    signature.verify(del_hash.as_ref(), &peer.public_key())
                } else {
                    false
                }
            }
        }
    }

    pub fn key(peer0: PeerId, peer1: PeerId) -> (PeerId, PeerId) {
        if peer0 < peer1 {
            (peer0, peer1)
        } else {
            (peer1, peer0)
        }
    }

    /// Helper function when adding a new edge and we receive information from new potential peer
    /// to verify the signature.
    pub fn partial_verify(peer0: PeerId, peer1: PeerId, edge_info: &EdgeInfo) -> bool {
        let pk = peer1.public_key();
        let (peer0, peer1) = Edge::key(peer0, peer1);
        let data = Edge::build_hash(&peer0, &peer1, edge_info.nonce);
        edge_info.signature.verify(data.as_ref(), &pk)
    }

    fn get_pair(&self) -> (PeerId, PeerId) {
        (self.peer0.clone(), self.peer1.clone())
    }

    /// It will be considered as a new edge if the nonce is odd, otherwise it is canceling the
    /// previous edge.
    fn edge_type(&self) -> EdgeType {
        if self.nonce % 2 == 1 {
            EdgeType::Added
        } else {
            EdgeType::Removed
        }
    }

    /// Next nonce of valid addition edge.
    pub fn next_nonce(&self) -> u64 {
        if self.nonce % 2 == 1 {
            self.nonce + 2
        } else {
            self.nonce + 1
        }
    }
}

#[derive(Clone)]
pub struct RoutingTable {
    // TODO(MarX, #1363): Use cache and file storing to keep this information.
    // TODO(MarX): Add proof that this account belongs to this peer.
    /// PeerId associated for every known account id.
    pub account_peers: HashMap<AccountId, PeerId>,
    /// Active PeerId that are part of the shortest path to each PeerId.
    pub peer_forwarding: HashMap<PeerId, HashSet<PeerId>>,
    /// Store last update for known edges.
    pub edges_info: HashMap<(PeerId, PeerId), Edge>,
    /// Current view of the network. Nodes are Peers and edges are active connections.
    raw_graph: Graph,
}

#[derive(Debug)]
pub enum FindRouteError {
    Disconnected,
    PeerNotFound,
    AccountNotFound,
}

impl RoutingTable {
    pub fn new(peer_id: PeerId) -> Self {
        Self {
            account_peers: HashMap::new(),
            peer_forwarding: HashMap::new(),
            edges_info: HashMap::new(),
            raw_graph: Graph::new(peer_id),
        }
    }

    /// Find peer that is connected to `source` and belong to the shortest path
    /// from `source` to `peer_id`.
    pub fn find_route(&self, peer_id: &PeerId) -> Result<PeerId, FindRouteError> {
        if let Some(routes) = self.peer_forwarding.get(&peer_id) {
            if routes.is_empty() {
                Err(FindRouteError::Disconnected)
            } else {
                // TODO(MarX, #1363): Do Round Robin
                Ok(routes.iter().next().unwrap().clone())
            }
        } else {
            Err(FindRouteError::PeerNotFound)
        }
    }

    /// Find peer that owns this AccountId.
    pub fn account_owner(&self, account_id: &AccountId) -> Result<PeerId, FindRouteError> {
        self.account_peers.get(account_id).cloned().ok_or_else(|| FindRouteError::AccountNotFound)
    }

    /// Add (account id, peer id) to routing table.
    /// Returns a bool indicating whether this is a new entry or not.
    /// Note: There is at most on peer id per account id.
    pub fn add_account(&mut self, account_id: AccountId, peer_id: PeerId) -> bool {
        match self.account_peers.entry(account_id) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(peer_id);
                true
            }
        }
    }

    /// Add this edge to the current view of the network.
    /// This edge is assumed to be valid at this point.
    /// Edge contains about being added or removed (this can trigger both types of events).
    /// Return true if the edge contains new information about the network. Old if this information
    /// is outdated.
    pub fn process_edge(&mut self, edge: Edge) -> bool {
        let key = edge.get_pair();

        if self.find_nonce(&key) >= edge.nonce {
            // We already have a newer information about this edge. Discard this information.
            debug!(target:"network", "Received outdated edge: {:?}", edge);
            return false;
        }

        match edge.edge_type() {
            EdgeType::Added => {
                self.raw_graph.add_edge(key.0.clone(), key.1.clone());
            }
            EdgeType::Removed => {
                self.raw_graph.remove_edge(&key.0, &key.1);
            }
        }

        self.edges_info.insert(key, edge);
        // TODO(MarX, #1363): Don't recalculate all the time
        self.peer_forwarding = self.raw_graph.calculate_distance();
        true
    }

    pub fn find_nonce(&self, edge: &(PeerId, PeerId)) -> u64 {
        self.edges_info.get(&edge).map_or(0, |x| x.nonce)
    }

    pub fn get_edge(&self, peer0: PeerId, peer1: PeerId) -> Option<Edge> {
        let key = Edge::key(peer0, peer1);
        self.edges_info.get(&key).cloned()
    }

    pub fn get_edges(&self) -> Vec<Edge> {
        self.edges_info.iter().map(|(_, edge)| edge.clone()).collect()
    }

    pub fn info(&self) -> RoutingTableInfo {
        RoutingTableInfo {
            account_peers: self.account_peers.clone(),
            peer_forwarding: self.peer_forwarding.clone(),
        }
    }
}

#[derive(Debug)]
pub struct RoutingTableInfo {
    pub account_peers: HashMap<AccountId, PeerId>,
    pub peer_forwarding: HashMap<PeerId, HashSet<PeerId>>,
}

#[derive(Clone)]
pub struct Graph {
    pub source: PeerId,
    adjacency: HashMap<PeerId, HashSet<PeerId>>,
}

impl Graph {
    pub fn new(source: PeerId) -> Self {
        Self { source, adjacency: HashMap::new() }
    }

    fn contains_edge(&mut self, peer0: &PeerId, peer1: &PeerId) -> bool {
        if let Some(adj) = self.adjacency.get(&peer0) {
            if adj.contains(&peer1) {
                return true;
            }
        }

        false
    }

    fn add_directed_edge(&mut self, peer0: PeerId, peer1: PeerId) {
        self.adjacency.entry(peer0).or_insert_with(HashSet::new).insert(peer1);
    }

    fn remove_directed_edge(&mut self, peer0: &PeerId, peer1: &PeerId) {
        self.adjacency.get_mut(&peer0).unwrap().remove(&peer1);
    }

    pub fn add_edge(&mut self, peer0: PeerId, peer1: PeerId) {
        if !self.contains_edge(&peer0, &peer1) {
            self.add_directed_edge(peer0.clone(), peer1.clone());
            self.add_directed_edge(peer1, peer0);
        }
    }

    pub fn remove_edge(&mut self, peer0: &PeerId, peer1: &PeerId) {
        if self.contains_edge(&peer0, &peer1) {
            self.remove_directed_edge(&peer0, &peer1);
            self.remove_directed_edge(&peer1, &peer0);
        }
    }

    // TODO(MarX, #1363): This is too slow right now. (See benchmarks)
    /// Compute for every node `u` on the graph (other than `source`) which are the neighbors of
    /// `sources` which belong to the shortest path from `source` to `u`. Nodes that are
    /// not connected to `source` will not appear in the result.
    pub fn calculate_distance(&self) -> HashMap<PeerId, HashSet<PeerId>> {
        let mut queue = vec![];
        let mut distance = HashMap::new();
        // TODO(MarX, #1363): Represent routes more efficiently at least while calculating distances
        let mut routes: HashMap<PeerId, HashSet<PeerId>> = HashMap::new();

        distance.insert(&self.source, 0);

        // Add active connections
        if let Some(neighbors) = self.adjacency.get(&self.source) {
            for neighbor in neighbors {
                queue.push(neighbor);
                distance.insert(neighbor, 1);
                routes.insert(neighbor.clone(), vec![neighbor.clone()].drain(..).collect());
            }
        }

        let mut head = 0;

        while head < queue.len() {
            let cur_peer = queue[head];
            let cur_distance = *distance.get(cur_peer).unwrap();
            head += 1;

            if let Some(neighbors) = self.adjacency.get(&cur_peer) {
                for neighbor in neighbors {
                    if !distance.contains_key(&neighbor) {
                        queue.push(neighbor);
                        distance.insert(neighbor, cur_distance + 1);
                        routes.insert(neighbor.clone(), HashSet::new());
                    }

                    // If this edge belong to a shortest path, all paths to
                    // the closer nodes are also valid for the current node.
                    if *distance.get(neighbor).unwrap() == cur_distance + 1 {
                        let adding_routes = routes.get(cur_peer).unwrap().clone();
                        let target_routes = routes.get_mut(neighbor).unwrap();

                        for route in adding_routes {
                            target_routes.insert(route.clone());
                        }
                    }
                }
            }
        }

        routes.into_iter().filter(|(_, hops)| !hops.is_empty()).collect()
    }
}

#[cfg(test)]
mod test {
    use crate::routing::Graph;
    use crate::test_utils::{expected_routing_tables, random_peer_id};

    #[test]
    fn graph_contains_edge() {
        let source = random_peer_id();

        let node0 = random_peer_id();
        let node1 = random_peer_id();

        let mut graph = Graph::new(source.clone());

        assert_eq!(graph.contains_edge(&source, &node0), false);
        assert_eq!(graph.contains_edge(&source, &node1), false);
        assert_eq!(graph.contains_edge(&node0, &node1), false);
        assert_eq!(graph.contains_edge(&node1, &node0), false);

        graph.add_edge(node0.clone(), node1.clone());

        assert_eq!(graph.contains_edge(&source, &node0), false);
        assert_eq!(graph.contains_edge(&source, &node1), false);
        assert_eq!(graph.contains_edge(&node0, &node1), true);
        assert_eq!(graph.contains_edge(&node1, &node0), true);

        graph.remove_edge(&node1, &node0);

        assert_eq!(graph.contains_edge(&node0, &node1), false);
        assert_eq!(graph.contains_edge(&node1, &node0), false);
    }

    #[test]
    fn graph_distance0() {
        let source = random_peer_id();
        let node0 = random_peer_id();

        let mut graph = Graph::new(source.clone());
        graph.add_edge(source.clone(), node0.clone());

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![(node0.clone(), vec![node0.clone()])],
        ));
    }

    #[test]
    fn graph_distance1() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(nodes[0].clone(), nodes[1].clone());
        graph.add_edge(nodes[2].clone(), nodes[1].clone());
        graph.add_edge(nodes[1].clone(), nodes[2].clone());

        assert!(expected_routing_tables(graph.calculate_distance(), vec![]));
    }

    #[test]
    fn graph_distance2() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(nodes[0].clone(), nodes[1].clone());
        graph.add_edge(nodes[2].clone(), nodes[1].clone());
        graph.add_edge(nodes[1].clone(), nodes[2].clone());
        graph.add_edge(source.clone(), nodes[0].clone());

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![
                (nodes[0].clone(), vec![nodes[0].clone()]),
                (nodes[1].clone(), vec![nodes[0].clone()]),
                (nodes[2].clone(), vec![nodes[0].clone()]),
            ],
        ));
    }

    #[test]
    fn graph_distance3() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(nodes[0].clone(), nodes[1].clone());
        graph.add_edge(nodes[2].clone(), nodes[1].clone());
        graph.add_edge(nodes[0].clone(), nodes[2].clone());
        graph.add_edge(source.clone(), nodes[0].clone());
        graph.add_edge(source.clone(), nodes[1].clone());

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![
                (nodes[0].clone(), vec![nodes[0].clone()]),
                (nodes[1].clone(), vec![nodes[1].clone()]),
                (nodes[2].clone(), vec![nodes[0].clone(), nodes[1].clone()]),
            ],
        ));
    }

    /// Test the following graph
    ///     0 - 3 - 6
    ///   /   x   x
    /// s - 1 - 4 - 7  
    ///   \   x   x
    ///     2 - 5 - 8
    ///
    ///    9 - 10 (Dummy edge disconnected)
    ///
    /// There is a shortest path to nodes [3..9) going through 0, 1, and 2.
    #[test]
    fn graph_distance4() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..11).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        for i in 0..3 {
            graph.add_edge(source.clone(), nodes[i].clone());
        }

        for level in 0..2 {
            for i in 0..3 {
                for j in 0..3 {
                    graph.add_edge(nodes[level * 3 + i].clone(), nodes[level * 3 + 3 + j].clone());
                }
            }
        }

        // Dummy edge.
        graph.add_edge(nodes[9].clone(), nodes[10].clone());

        let mut next_hops: Vec<_> =
            (0..3).map(|i| (nodes[i].clone(), vec![nodes[i].clone()])).collect();
        let target: Vec<_> = (0..3).map(|i| nodes[i].clone()).collect();

        for i in 3..9 {
            next_hops.push((nodes[i].clone(), target.clone()));
        }

        assert!(expected_routing_tables(graph.calculate_distance(), next_hops));
    }
}
