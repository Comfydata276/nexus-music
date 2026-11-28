// E3 (2026-11-28). Each distinct Navidrome answer preserved by sub.py, across accept,
// refuse, spelling, limiter, parallel, rp, rp-detect and jwt. XML was saved verbatim.
// JSON was decoded before recording; retain the recorded fields and add its outer object.
pub(super) const ADMIN: &[u8] =
    br#"{"subsonic-response": {"status": "ok", "user": {"username": "Exp-Admin"}}}"#;
pub(super) const ALICE: &[u8] =
    br#"{"subsonic-response": {"status": "ok", "user": {"username": "Alice"}}}"#;
pub(super) const BOB: &[u8] =
    br#"{"subsonic-response": {"status": "ok", "user": {"username": "bob smith"}}}"#;
pub(super) const FORBIDDEN: &[u8] = br#"{"subsonic-response": {"status": "failed", "error": {"code": 50, "message": "User is not authorized for the given operation"}}}"#;
pub(super) const MISSING_C: &[u8] = br#"{"subsonic-response": {"status": "failed", "error": {"code": 10, "message": "missing parameter: 'c'"}}}"#;
pub(super) const MISSING_U: &[u8] = br#"{"subsonic-response": {"status": "failed", "error": {"code": 10, "message": "missing parameter: 'u'"}}}"#;
pub(super) const MISSING_V: &[u8] = br#"{"subsonic-response": {"status": "failed", "error": {"code": 10, "message": "missing parameter: 'v'"}}}"#;
pub(super) const OK: &[u8] = br#"{"subsonic-response": {"status": "ok"}}"#;
pub(super) const OK_WITH_VERSION: &[u8] = br#"{"subsonic-response": {"status": "ok", "version": "1.16.1", "type": "navidrome", "serverVersion": "0.64.2 (10114574)", "openSubsonic": true}}"#;
pub(super) const REFUSED: &[u8] = br#"{"subsonic-response": {"status": "failed", "error": {"code": 40, "message": "Wrong username or password"}}}"#;
pub(super) const REFUSED_WITH_VERSION: &[u8] = br#"{"subsonic-response": {"status": "failed", "version": "1.16.1", "type": "navidrome", "serverVersion": "0.64.2 (10114574)", "openSubsonic": true, "error": {"code": 40, "message": "Wrong username or password"}}}"#;
pub(super) const SONG: &[u8] = br#"{"subsonic-response": {"status": "ok", "song": {"id": "0CXchJ3RJZn8tyodicbznj", "title": "Rust Compass"}}}"#;
pub(super) const SONG_MISSING: &[u8] =
    br#"{"subsonic-response": {"status": "failed", "error": {"code": 70, "message": "Song not found"}}}"#;
pub(super) const XML_LARGE: &[u8] = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1" type="navidrome" serverVersion="0.64.2 (10114574)" openSubsonic="true"><error code="0" message="http: request body too large"></error></subsonic-response>"#;
pub(super) const XML_MISSING_C: &[u8] = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1" type="navidrome" serverVersion="0.64.2 (10114574)" openSubsonic="true"><error code="10" message="missing parameter: &#39;c&#39;"></error></subsonic-response>"#;
pub(super) const XML_MISSING_U: &[u8] = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1" type="navidrome" serverVersion="0.64.2 (10114574)" openSubsonic="true"><error code="10" message="missing parameter: &#39;u&#39;"></error></subsonic-response>"#;
pub(super) const XML_MISSING_V: &[u8] = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1" type="navidrome" serverVersion="0.64.2 (10114574)" openSubsonic="true"><error code="10" message="missing parameter: &#39;v&#39;"></error></subsonic-response>"#;
pub(super) const XML_OK: &[u8] = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome" serverVersion="0.64.2 (10114574)" openSubsonic="true"></subsonic-response>"#;
pub(super) const XML_REFUSED: &[u8] = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1" type="navidrome" serverVersion="0.64.2 (10114574)" openSubsonic="true"><error code="40" message="Wrong username or password"></error></subsonic-response>"#;
