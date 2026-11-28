//! The Nexus server: one small service of our own beside the music server (design
//! `docs/backend-design.md`): the listener, its bounds and `hello` (S1); the data and the store
//! kept apart (S3); accounts and signing in (S8).
//!
//! Every rule it shares with the apps is in the shared library (`nexus_core::nexus`), which
//! it links as they do; what is here is its plumbing.

#![forbid(unsafe_code)]

pub mod accounts;
mod admin;
pub mod bounds;
pub mod diag;
pub mod events;
pub mod head;
pub mod health;
pub mod identity;
pub mod music;
pub mod probe;
pub mod proxy;
pub mod requests;
pub mod serve;
pub mod settings;
pub mod store;

use nexus_core::nexus::proto::{Hello, SPEAKS, Version};

/// This program's version, as `hello` says it.
pub fn version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version is MAJOR.MINOR.PATCH")
}

/// The `hello` of a backend with this id: it has accounts (S8), so `has` names them (design
/// 18; an app refuses to sign in to a backend that does not).
pub fn hello(id: nexus_core::nexus::proto::BackendId) -> Hello {
    Hello::for_accounts(version(), id, SPEAKS)
}
