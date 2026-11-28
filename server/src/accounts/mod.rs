//! Accounts, and signing in (design 8; slice S8). What is allowed, and what an answer is, is
//! decided in the shared library (`nexus_core::nexus::{account, kept}`); this carries it out.
//!
//! - [`check`] (S8b): the two password workers and their three doors (8.2, 8.3).
//! - [`desk`]: the one thread that writes, and the line of stated length waiting for it (19.3).
//! - [`rows`]: what they read from the data.

pub mod check;
pub mod desk;
pub mod rows;
