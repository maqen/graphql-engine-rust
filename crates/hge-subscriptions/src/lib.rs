//! GraphQL subscriptions: the WebSocket protocols and the multiplexed pollers behind live
//! queries and streaming subscriptions (spec 09). The WebSocket transport itself lives in
//! `hge-server`; this crate is transport- and backend-agnostic.

pub mod poller;
pub mod protocol;

pub use poller::{PollerInfo, PollerOptions, Registry, Subscriber, SubscriptionHandle};
