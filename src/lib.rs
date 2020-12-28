#![deny(missing_docs)]

//! **pea2pea** is a P2P library designed with the following use cases in mind:
//! - simple and quick creation of custom P2P networks
//! - testing/verifying network protocols
//! - benchmarking and stress-testing P2P nodes (or other network entities)
//! - substituting other, "heavier" nodes in local network tests

mod config;
mod connection;
mod connections;
mod known_peers;
mod node;
mod protocols;
mod topology;

pub use config::NodeConfig;
pub use connection::{Connection, ConnectionReader};
pub use node::{ContainsNode, Node};
pub use protocols::{
    HandshakeSetup, HandshakeState, Handshaking, InboundMessage, Messaging, ReadingClosure,
};
pub use topology::{connect_nodes, Topology};
