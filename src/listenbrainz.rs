//! How much a song is played, from ListenBrainz.
//!
//! The questions and the reading of the answers are in the library
//! (`nexus_core::listenbrainz`), for the Android app to use too. This is the Windows app's
//! end: the connection they're asked over. Nothing here is used unless requests are
//! switched on.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nexus_core::listenbrainz::ListenBrainz;
pub use nexus_core::listenbrainz::TopSong;

pub(crate) fn service() -> &'static ListenBrainz {
    static SERVICE: OnceLock<ListenBrainz> = OnceLock::new();
    SERVICE.get_or_init(|| ListenBrainz::new(Arc::new(crate::corehttp::Agent(crate::subsonic::make_agent_idle(Duration::from_secs(50))))))
}

pub fn top_songs(artist: &str) -> Result<Vec<TopSong>, String> {
    service().top_songs(artist)
}

pub fn listens(recordings: &[String]) -> Result<HashMap<String, u64>, String> {
    service().listens(recordings)
}

pub fn artist_listens(artists: &[String]) -> Result<HashMap<String, u64>, String> {
    service().artist_listens(artists)
}
