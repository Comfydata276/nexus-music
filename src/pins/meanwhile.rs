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
