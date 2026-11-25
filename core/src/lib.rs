//! The parts of Nexus Music that every app on an account must do identically.
//!
//! The Windows app is built on this, and the Android app will be: a rule that lives here is
//! written once and gives both the same answer. So nothing here draws anything, keeps a
//! file, starts a thread or opens a connection; whatever it needs from the world, the app
//! hands it.

#![forbid(unsafe_code)]

pub mod address;
pub mod asked;
pub mod bulk;
pub mod discover;
pub mod gate;
pub mod handover;
pub mod http;
pub mod jam;
pub mod levelling;
pub mod lidarr;
pub mod link;
pub mod listenbrainz;
pub mod model;
pub mod musicbrainz;
pub mod notes;
pub mod offer;
pub mod playlists;
pub mod queue;
pub mod record;
pub mod requests;
pub mod search;
pub mod step;
pub mod sync;
pub mod text;
pub mod time;
pub mod ways;
