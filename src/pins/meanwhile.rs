//! What changes while a page is being found: the "singles and EPs" setting, switched in
//! Settings, and the library copy, replaced by the window's worker when the server's has
//! changed. Each is changed while one request is held, and the page goes by what 0.40.8 read
//! when it read it.

use super::*;

#[test]
fn an_artists_page_goes_by_the_setting_as_it_is_once_lidarr_has_answered() {
    // Switched off while Lidarr is asked whether it keeps them: MusicBrainz is asked for
    // their albums only, and so is the extras' look.
    let mc = ("mc1", "Setting Band");
    let _scene = Scene::new()
        .singles(true)
        .meanwhile("GET lidarr.test/api/v1/artist", || crate::lidarr::set_singles(false))
        .answer("GET lidarr.test/api/v1/artist", "[]")
        .answer("GET musicbrainz.org/ws/2/release-group?artist=mc1&type=album&", groups(&[group("rg-mc1-a", "Albums Only", "Album", &[], "2015-01-01", mc)]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=mc1&type=album%7Cep%7Csingle", groups(&[
            group("rg-mc1-a", "Albums Only", "Album", &[], "2015-01-01", mc),
            group("rg-mc1-s", "A Single", "Single", &[], "2016-01-01", mc),
        ]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc1", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/release-group", "[]")
        .enter();
    pinned("meanwhile-artist-singles-off", &walk(Route::OutsideArtist("mc1".into()), None), false);
}

#[test]
fn an_artist_lidarr_keeps_goes_by_the_setting_as_it_is_once_lidarr_has_answered() {
    // The same for an artist Lidarr keeps: no singles and EPs are brought in from MusicBrainz.
    let _scene = Scene::new()
        .singles(true)
        .meanwhile("GET lidarr.test/api/v1/artist", || crate::lidarr::set_singles(false))
        .answer("GET lidarr.test/api/v1/artist", r#"[{"id":8,"artistName":"Kept Setting","foreignArtistId":"mc2"}]"#)
        .answer("GET lidarr.test/api/v1/album?artistId=8", list(&[json!({ "id": 81, "title": "Kept Album", "foreignAlbumId": "rg-mc2-a", "albumType": "Album", "secondaryTypes": [], "releaseDate": "2015-01-01T00:00:00Z", "monitored": false, "artist": { "artistName": "Kept Setting", "foreignArtistId": "mc2" } })]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=mc2", groups(&[
            group("rg-mc2-a", "Kept Album", "Album", &[], "2015-01-01", ("mc2", "Kept Setting")),
            group("rg-mc2-s", "Kept Single", "Single", &[], "2016-01-01", ("mc2", "Kept Setting")),
        ]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc2", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/release-group", "[]")
        .enter();
    pinned("meanwhile-artist-kept-singles-off", &walk(Route::OutsideArtist("mc2".into()), None), false);
}
#[test]
fn an_artists_page_goes_by_the_library_as_it_is_once_their_releases_are_known() {
    // One of their albums arrives in the library while MusicBrainz is asked for their list:
    // it's shown as the library's, counted in, and not open to "Add artist".
    let mid = ("mc3", "Mid Band");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist", "[]")
        .meanwhile("GET musicbrainz.org/ws/2/release-group?artist=mc3", replace_library(&[("lib-grown", "Grown", "Mid Band")], &[]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=mc3", groups(&[group("rg-grown", "Grown", "Album", &[], "2015-01-01", mid)]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc3", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/release-group", "[]")
        .enter();
    pinned("meanwhile-artist-library", &walk(Route::OutsideArtist("mc3".into()), None), false);
}

#[test]
fn a_search_goes_by_the_library_as_it_is_when_the_most_played_are_known() {
    // An artist meant beyond doubt; one of their most played arrives in the library while
    // ListenBrainz is asked what they're played for: it's left out, and that's said.
    let star = ("mc4", "Mid Star");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Mid%20Star", list(&[lidarr_artist("mc4", "Mid Star", true, "")]))
        .answer("GET lidarr.test/api/v1/album/lookup?term=Mid%20Star", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/artist", listens("artist_mbid", &[("mc4", 6_000_000)]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=mc4", groups(&[group("rg-mc4", "Star Album", "Album", &[], "2015-01-01", star)]))
        .meanwhile("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc4", replace_library(&[], &[("s-top", "Top One", "Mid Star")]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc4", top(&[("Top One", "Star Album", 900), ("Second One", "Star Album", 800)]))
        .enter();
    pinned("meanwhile-search-library", &searched("Mid Star"), true);
}

#[test]
fn an_artists_most_played_go_by_the_library_as_it_is_when_they_are_known() {
    let pop = ("mc5", "Mid Pop");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist", "[]")
        .answer("GET musicbrainz.org/ws/2/release-group?artist=mc5", groups(&[group("rg-mc5", "Pop Album", "Album", &[], "2015-01-01", pop)]))
        .meanwhile("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc5", replace_library(&[], &[("s-top5", "Top One", "Mid Pop")]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/mc5", top(&[("Top One", "Pop Album", 900), ("Second One", "Pop Album", 800)]))
        .enter();
    pinned("meanwhile-artist-popular-library", &walk(Route::OutsideArtist("mc5".into()), None), false);
}
