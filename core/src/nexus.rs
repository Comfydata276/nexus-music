//! What a device and the Nexus server (the backend of our own beside the music server) say
//! to each other, and the rules each side applies to it.
//!
//! The design is `docs/backend-design.md`. Stage 1 starts with [`proto`]: the backend's
//! `hello`, how an app tells the backend from anything else that may answer at its
//! address, and the one plain form of an address the backend accepts.
//!
//! Like the rest of this library, nothing here has a clock, a file, a socket or a source of
//! randomness, and nothing here panics, whatever it is given. The backend links this, as
//! the apps do, so both sides read the same words the same way.

pub mod account;
pub mod admin;
pub mod client;
pub mod compare;
pub mod diag;
pub mod events;
pub mod kept;
pub mod music;
pub mod proto;
pub mod requests;
pub mod wire;
