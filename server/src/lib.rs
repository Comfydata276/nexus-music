//! The Nexus server: one small service of our own beside the music server (design
//! `docs/backend-design.md`). This first slice (S1) is its shell: the listener, its bounds,
//! and `hello`. **Nothing else, and nothing that needs a secret.**
//!
//! Every rule it shares with the apps is in the shared library (`nexus_core::nexus`), which
//! it links as they do; what is here is its plumbing.

#![forbid(unsafe_code)]

mod admin;
pub mod bounds;
pub mod head;
pub mod health;
pub mod identity;
pub mod probe;
pub mod serve;
pub mod settings;
pub mod store;

use nexus_core::nexus::proto::{Hello, SPEAKS, Version};

/// This program's version, as `hello` says it.
pub fn version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version is MAJOR.MINOR.PATCH")
}

/// The `hello` of a backend with this id. At this slice it does nothing yet, so `has` is
/// empty.
pub fn hello(id: nexus_core::nexus::proto::BackendId) -> Hello {
    Hello::new(version(), id, SPEAKS, &[]).expect("an empty `has` is always allowed")
}
