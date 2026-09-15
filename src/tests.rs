use std::{fs, time::Duration};

use ratatui::{Terminal, backend::TestBackend, style::Color};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, header_exists, method, path, query_param},
};

use crate::{
    auth::{Api, Credentials, Error, Scan},
    content,
    session::Store,
    ui::{self, Code},
};

const QR_URL: &str = "https://h5.xiaoyuzhoufm.com/oauth?qrcode_id=6aa8073a7c72d6f274950175";

fn credentials() -> Credentials {
    Credentials {
        access_token: "test-access".into(),
        refresh_token: "test-refresh".into(),
    }
}

async fn mount_episode(server: &MockServer, eid: &str, title: &str, delay: Duration) {
    Mock::given(method("GET"))
        .and(path("/v1/episode/get"))
        .and(query_param("eid", eid))
        .and(header("x-jike-access-token", "test-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({
                    "data": {"eid":eid,"title":title,"duration":3661,"podcast":{"title":"测试播客"}}
                })),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn playlist_preserves_order_deduplicates_ids_and_passes_opaque_cursor() {
    let server = MockServer::start().await;
    let cursor = json!({"offset":2,"revision":"opaque"});
    for (body, response) in [
        (
            json!({}),
            json!({"data":{"list":["first","second","first"]},"loadMoreKey":cursor}),
        ),
        (
            json!({"loadMoreKey":cursor}),
            json!({"data":{"list":["second","third"]}}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path("/v1/playlist/pull"))
            .and(header("x-jike-access-token", "test-access"))
            .and(body_json(body))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
    }
    mount_episode(
        &server,
        "first",
        "第一集\n新标题\u{0007}",
        Duration::from_millis(50),
    )
    .await;
    mount_episode(&server, "second", "第二集", Duration::ZERO).await;
    mount_episode(&server, "third", "第三集", Duration::ZERO).await;
    Mock::given(method("POST"))
        .and(path("/v1/playback-progress/list"))
        .and(header("x-jike-access-token", "test-access"))
        .and(body_json(json!({"eids":["first","second","third"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[
            {"eid":"third","progress":4000},
            {"eid":"first","progress":60.5},
            {"eid":"unrequested","progress":12}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let entries = content::Api::for_test(server.uri())
        .playlist(&credentials())
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.eid.as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    let episode = entries[0].episode.as_ref().unwrap();
    assert_eq!(episode.title, "第一集 新标题");
    assert_eq!(episode.podcast.as_ref().unwrap().title, "测试播客");
    assert_eq!(episode.duration, Some(3661));
    assert_eq!(entries[0].remaining(), Some(3601));
    assert_eq!(entries[1].remaining(), None);
    assert_eq!(entries[2].remaining(), Some(0));
    assert!(!entries[0].progress_failed);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.headers.contains_key("x-jike-refresh-token"))
    );
}

#[tokio::test]
async fn playlist_empty_is_distinct_from_missing_data_and_broken_pagination() {
    let server = MockServer::start().await;
    let api = content::Api::for_test(server.uri());
    for (response, empty) in [
        (json!({"data":{"list":[]}}), true),
        (json!({"data":{}}), false),
        (json!({"data":{"list":[""]}}), false),
        (json!({"data":{"list":[]},"loadMoreKey":"next"}), false),
        (
            json!({"data":{"list":["first"]},"loadMoreKey":"repeated"}),
            false,
        ),
    ] {
        let _guard = Mock::given(path("/v1/playlist/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount_as_scoped(&server)
            .await;
        let result = api.playlist(&credentials()).await;
        if empty {
            assert!(result.unwrap().is_empty());
        } else {
            assert!(matches!(result, Err(Error::InvalidResponse)));
        }
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method.as_str() == "POST")
    );
}

#[tokio::test]
async fn playlist_retains_unavailable_episodes_without_hiding_other_entries() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/playlist/pull"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"list":["removed","available"]}})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/episode/get"))
        .and(query_param("eid", "removed"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_episode(&server, "available", "仍可收听", Duration::ZERO).await;
    let entries = content::Api::for_test(server.uri())
        .playlist(&credentials())
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].eid, "removed");
    assert!(matches!(&entries[0].episode, Err(Error::Http(status)) if status.as_u16() == 404));
    assert!(entries[1].episode.is_ok());
}

#[tokio::test]
async fn playlist_reports_auth_rate_limit_and_server_failures() {
    let server = MockServer::start().await;
    let api = content::Api::for_test(server.uri());
    for endpoint in [
        "/v1/playlist/pull",
        "/v1/episode/get",
        "/v1/playback-progress/list",
    ] {
        let _queue = if endpoint != "/v1/playlist/pull" {
            Some(
                Mock::given(path("/v1/playlist/pull"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_json(json!({"data":{"list":["first"]}})),
                    )
                    .mount_as_scoped(&server)
                    .await,
            )
        } else {
            None
        };
        let _episode = if endpoint == "/v1/playback-progress/list" {
            Some(
                Mock::given(path("/v1/episode/get"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "data":{"eid":"first","title":"测试单集","duration":60}
                    })))
                    .expect(1)
                    .mount_as_scoped(&server)
                    .await,
            )
        } else {
            None
        };
        for status in [401, 429, 503] {
            let _error = Mock::given(path(endpoint))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Retry-After", "12")
                        .set_body_string("not json"),
                )
                .mount_as_scoped(&server)
                .await;
            let result = api.playlist(&credentials()).await;
            match status {
                429 => assert!(
                    matches!(result, Err(Error::RateLimited(delay)) if delay.as_secs() == 12)
                ),
                503 if endpoint == "/v1/episode/get" => {
                    assert!(result.unwrap()[0].episode.is_err())
                }
                503 if endpoint == "/v1/playback-progress/list" => {
                    let entries = result.unwrap();
                    assert!(entries[0].episode.is_ok());
                    assert!(entries[0].progress_failed);
                    assert_eq!(entries[0].remaining(), None);
                }
                _ => assert!(matches!(result, Err(Error::Http(code)) if code.as_u16() == status)),
            }
        }
    }
}

#[tokio::test]
async fn missing_or_invalid_progress_never_looks_like_an_unplayed_episode() {
    for (data, failed) in [
        (json!({"data":[]}), false),
        (json!({"data":[{"eid":"first","progress":-10}]}), true),
        (json!({"data":[{"eid":"first","progress":null}]}), true),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/v1/playlist/pull"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data":{"list":["first"]}})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v1/playback-progress/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(data))
            .expect(1)
            .mount(&server)
            .await;
        mount_episode(&server, "first", "标题", Duration::ZERO).await;
        let entries = content::Api::for_test(server.uri())
            .playlist(&credentials())
            .await
            .unwrap();
        assert!(entries[0].episode.is_ok());
        assert_eq!(entries[0].remaining(), None);
        assert_eq!(entries[0].progress_failed, failed);
    }
}

#[tokio::test]
async fn playlist_does_not_forward_tokens_on_redirects() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(path("/v1/playlist/pull"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", other.uri()))
        .mount(&server)
        .await;
    assert!(
        matches!(content::Api::for_test(server.uri()).playlist(&credentials()).await, Err(Error::Http(status)) if status.as_u16() == 302)
    );
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn creates_a_qr_with_the_web_login_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/auth/qrcode/create"))
        .and(header("x-midway-app-id", "v6worU4NnWyL"))
        .and(header("Origin", "https://accounts.xiaoyuzhoufm.com"))
        .and(body_json(json!({"clientId":"xyz-web"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"6aa8073a7c72d6f274950175", "url": QR_URL
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        Api::for_test(server.uri()).create().await.unwrap().url,
        QR_URL
    );
}

#[tokio::test]
async fn consumes_success_tokens_from_response_headers() {
    let server = MockServer::start().await;
    let api = Api::for_test(server.uri());
    for status in ["WAITTING", "SCANNED", "USED", "CONFIRMED"] {
        let response = ResponseTemplate::new(200)
            .insert_header("x-jike-access-token", "test-access")
            .insert_header("x-jike-refresh-token", "test-refresh")
            .set_body_json(json!({"status":status}));
        let _guard = Mock::given(method("POST"))
            .and(path("/v1/auth/qrcode/login"))
            .and(body_json(json!({"id":"qr-id"})))
            .respond_with(response)
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        match (status, api.poll("qr-id").await.unwrap()) {
            ("WAITTING", Scan::Waiting) | ("SCANNED", Scan::Scanned) => (),
            ("USED" | "CONFIRMED", Scan::Authenticated(tokens)) => {
                assert_eq!(tokens.access_token, "test-access");
                assert_eq!(tokens.refresh_token, "test-refresh");
            }
            _ => panic!("wrong login transition"),
        }
    }
}

#[tokio::test]
async fn incomplete_success_never_becomes_a_session() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/auth/qrcode/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-jike-access-token", "test-access")
                .set_body_json(json!({"status":"USED"})),
        )
        .mount(&server)
        .await;
    assert!(matches!(
        Api::for_test(server.uri()).poll("id").await,
        Err(Error::MissingCredentials)
    ));
}

#[tokio::test]
async fn handles_expiry_and_non_json_errors_before_decoding() {
    let server = MockServer::start().await;
    let api = Api::for_test(server.uri());
    for status in [400, 401, 500] {
        let _guard = Mock::given(path("/v1/auth/qrcode/login"))
            .respond_with(ResponseTemplate::new(status).set_body_string("<html>error</html>"))
            .mount_as_scoped(&server)
            .await;
        let result = api.poll("id").await;
        if status == 500 {
            assert!(matches!(result, Err(Error::Http(_))));
        } else {
            assert!(matches!(result, Ok(Scan::Expired)));
        }
    }
}

#[tokio::test]
async fn respects_rate_limit_wait() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/auth/qrcode/login"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "45"))
        .mount(&server)
        .await;
    let result = Api::for_test(server.uri()).poll("id").await;
    assert!(matches!(result, Err(Error::RateLimited(delay)) if delay == Duration::from_secs(45)));
}

#[tokio::test]
async fn checks_saved_access_token_before_reporting_logged_in() {
    let server = MockServer::start().await;
    let api = Api::for_test(server.uri());
    for (status, valid) in [(200, true), (401, false)] {
        let _guard = Mock::given(method("GET"))
            .and(path("/web/user/get-me"))
            .and(header("x-jike-access-token", "test-access"))
            .respond_with(
                ResponseTemplate::new(status).set_body_json(json!({"data":{"uid":"test-user"}})),
            )
            .mount_as_scoped(&server)
            .await;
        assert_eq!(api.validate(&credentials()).await.unwrap(), valid);
    }
}

#[tokio::test]
async fn rejects_qr_links_outside_the_login_service() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/auth/qrcode/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"id", "url":"https://example.com/oauth?qrcode_id=id"
        })))
        .mount(&server)
        .await;
    assert!(matches!(
        Api::for_test(server.uri()).create().await,
        Err(Error::InvalidResponse)
    ));
}

#[tokio::test]
async fn refresh_accepts_header_and_json_credentials_without_sending_access_token() {
    let server = MockServer::start().await;
    let api = Api::for_test(server.uri());
    let tokens = json!({"x-jike-access-token":"new-access","x-jike-refresh-token":"new-refresh"});
    for response in [
        ResponseTemplate::new(200)
            .insert_header("x-jike-access-token", "new-access")
            .insert_header("x-jike-refresh-token", "new-refresh"),
        ResponseTemplate::new(200).set_body_json(tokens.clone()),
        ResponseTemplate::new(200).set_body_json(json!({"data":tokens})),
        ResponseTemplate::new(200)
            .insert_header("x-jike-access-token", "new-access")
            .set_body_json(json!({"x-jike-refresh-token":"new-refresh"})),
    ] {
        let _guard = Mock::given(method("POST"))
            .and(path("/app_auth_tokens.refresh"))
            .and(header("x-jike-refresh-token", "test-refresh"))
            .respond_with(response)
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let renewed = api.refresh(&credentials()).await.unwrap();
        assert_eq!(renewed.access_token, "new-access");
        assert_eq!(renewed.refresh_token, "new-refresh");
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.headers.contains_key("x-jike-access-token"))
    );
}

#[tokio::test]
async fn refresh_rejects_incomplete_or_malformed_credentials_and_preserves_error_status() {
    let server = MockServer::start().await;
    let api = Api::for_test(server.uri());
    for response in [
        ResponseTemplate::new(200).set_body_string("not json"),
        ResponseTemplate::new(200).set_body_json(json!({"x-jike-access-token":"only-access"})),
        ResponseTemplate::new(200).set_body_json(
            json!({"x-jike-access-token":"bad\nheader","x-jike-refresh-token":"refresh"}),
        ),
    ] {
        let _guard = Mock::given(path("/app_auth_tokens.refresh"))
            .respond_with(response)
            .mount_as_scoped(&server)
            .await;
        assert!(matches!(
            api.refresh(&credentials()).await,
            Err(Error::InvalidResponse)
        ));
    }
    for status in [401, 429, 503] {
        let _guard = Mock::given(path("/app_auth_tokens.refresh"))
            .respond_with(ResponseTemplate::new(status).insert_header("Retry-After", "45"))
            .mount_as_scoped(&server)
            .await;
        let result = api.refresh(&credentials()).await;
        if status == 429 {
            assert!(matches!(result, Err(Error::RateLimited(delay)) if delay.as_secs() == 45));
        } else {
            assert!(matches!(result, Err(Error::Http(code)) if code.as_u16() == status));
        }
    }
}

#[test]
fn session_roundtrip_is_private_and_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let location = directory.path().join("state");
    let store = Store::for_test(location.clone());
    assert!(store.load().unwrap().is_none());
    store.save(&credentials()).unwrap();
    let changed = Credentials {
        access_token: "new-access".into(),
        refresh_token: "new-refresh".into(),
    };
    store.save(&changed).unwrap();
    let restored = store.load().unwrap().unwrap();
    assert_eq!(restored.access_token, "new-access");
    assert_eq!(restored.refresh_token, "new-refresh");
    assert_eq!(fs::read_dir(&location).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(location.join("session.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(location).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn incomplete_session_cannot_replace_a_valid_session() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::for_test(directory.path().to_owned());
    store.save(&credentials()).unwrap();
    assert!(
        store
            .save(&Credentials {
                access_token: "".into(),
                refresh_token: "".into()
            })
            .is_err()
    );
    assert_eq!(store.load().unwrap().unwrap().access_token, "test-access");
    fs::write(directory.path().join("session.json"), "broken").unwrap();
    assert!(store.load().is_err());
}

#[test]
fn qr_in_an_80_by_24_terminal_decodes_to_the_original_url() {
    let code = Code::new(QR_URL).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| ui::draw(frame, Some(&code), "用小宇宙扫码", "r 刷新 · q 退出"))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let scale = 8;
    let mut image = rqrr::PreparedImage::prepare_from_greyscale(80 * scale, 48 * scale, |x, y| {
        let cell = &buffer[((x / scale) as u16, (y / scale / 2) as u16)];
        let top = (y / scale) % 2 == 0;
        let ink = matches!((cell.symbol(), top), ("█", _) | ("▀", true) | ("▄", false));
        if cell.bg == Color::White && ink {
            0
        } else {
            255
        }
    });
    let grids = image.detect_grids();
    assert_eq!(
        grids.len(),
        1,
        "QR must fit without clipping and retain a quiet zone"
    );
    assert_eq!(grids[0].decode().unwrap().1, QR_URL);
}

#[test]
fn tiny_terminal_does_not_panic_or_render_a_partial_qr() {
    let code = Code::new(QR_URL).unwrap();
    for (width, height) in [(0, 0), (1, 1), (20, 10)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| ui::draw(frame, Some(&code), "请放大终端窗口", "q 退出"))
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.bg != Color::White)
        );
    }
}

#[tokio::test]
async fn playable_uses_fresh_progress_and_rejects_unsafe_media_urls() {
    for (url, success) in [
        ("https://media.example/audio.mp3", true),
        ("file:///etc/passwd", false),
        ("https://user:secret@media.example/a.mp3", false),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/v1/episode/get"))
            .and(query_param("eid", "episode"))
            .and(header("x-jike-access-token", "test-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{
                "eid":"episode", "pid":"podcast", "title":"title", "duration":60,
                "media":{"source":{"url":url}}
            }})))
            .mount(&server)
            .await;
        Mock::given(path("/v1/playback-progress/list"))
            .and(body_json(json!({"eids":["episode"]})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"eid":"episode","progress":12.5}]})),
            )
            .mount(&server)
            .await;
        let result = content::Api::for_test(server.uri())
            .playable(&credentials(), "episode")
            .await;
        assert_eq!(result.is_ok(), success);
        if let Ok(source) = result {
            assert_eq!(source.start, 12.5);
            assert_eq!(source.url, url);
        }
    }
}

#[tokio::test]
async fn private_media_uses_authorized_url_and_completed_episodes_restart() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/episode/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{
            "eid":"episode", "pid":"podcast", "title":"title", "duration":60, "isPrivateMedia":true
        }})))
        .mount(&server)
        .await;
    Mock::given(path("/v1/private-media/get"))
        .and(query_param("eid", "episode"))
        .and(query_param("dubbing", "false"))
        .and(header("x-jike-access-token", "test-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"url":"https://cdn.example/private.mp3"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/playback-progress/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"eid":"episode","progress":60}]})),
        )
        .mount(&server)
        .await;
    let source = content::Api::for_test(server.uri())
        .playable(&credentials(), "episode")
        .await
        .unwrap();
    assert_eq!(source.start, 0.0);
    assert_eq!(source.url, "https://cdn.example/private.mp3");
}

#[tokio::test]
async fn progress_upload_contract_preserves_seconds_and_rate_limits() {
    let server = MockServer::start().await;
    let progress = content::ProgressUpdate::new("episode", "podcast", 123);
    Mock::given(path("/v1/playback-progress/update")).and(method("POST"))
        .and(header_exists("local-time"))
        .and(header("x-jike-access-token", "test-access"))
        .and(body_json(json!({"data":[{"eid":"episode","pid":"podcast","progress":123,"playedAt":progress.played_at}]})))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "45"))
        .expect(1).mount(&server).await;
    let api = content::Api::for_test(server.uri());
    api.update_progress(&credentials(), &[]).await.unwrap();
    assert!(
        matches!(api.update_progress(&credentials(), &[progress]).await, Err(Error::RateLimited(delay)) if delay == Duration::from_secs(45))
    );
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("x-jike-refresh-token"));
}

#[tokio::test]
async fn removal_rebases_after_conflict_and_only_removes_the_selected_episode() {
    use std::sync::{Arc, Mutex};
    let server = MockServer::start().await;
    let state = Arc::new(Mutex::new((vec!["a", "b", "c"], 0)));
    let shared = state.clone();
    Mock::given(path("/v1/playlist/pull"))
        .respond_with(move |_: &wiremock::Request| {
            let state = shared.lock().unwrap();
            ResponseTemplate::new(200).set_body_json(
                json!({"data":{"list":state.0,"sha":format!("revision-{}",state.1)}}),
            )
        })
        .mount(&server)
        .await;
    let shared = state.clone();
    Mock::given(path("/v1/playlist/patch"))
        .and(header("x-jike-device-name", "xyz-tui"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let mut state = shared.lock().unwrap();
            assert!(body.get("list").is_none());
            assert!(uuid::Uuid::parse_str(body["id"].as_str().unwrap()).is_ok());
            assert!(request.headers.contains_key("x-jike-device-id"));
            assert_eq!(body["base"], format!("revision-{}", state.1));
            assert_eq!(
                body["ops"],
                json!([{"action":"rem","item":"b","pos":if state.1==0 {1} else {2}}])
            );
            if state.1 == 0 {
                state.0.insert(0, "new");
                state.1 += 1;
                ResponseTemplate::new(200).set_body_json(json!({"data":{"kind":"REJECT"}}))
            } else {
                state.0.remove(2);
                state.1 += 1;
                ResponseTemplate::new(200).set_body_json(
                    json!({"data":{"kind":"ACK","id":body["id"],"sha":"revision-2"}}),
                )
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let api = content::Api::for_test(server.uri());
    api.remove_from_playlist(&credentials(), "b").await.unwrap();
    api.remove_from_playlist(&credentials(), "b").await.unwrap();
    assert_eq!(state.lock().unwrap().0, ["new", "a", "c"]);
}

#[tokio::test]
async fn removal_rejects_unconfirmed_ack_and_never_pushes_a_full_list() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/playlist/pull"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"list":["a"],"sha":"revision"}})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/playlist/patch"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"kind":"ACK","id":body["id"],"sha":"revision"}}))
        })
        .mount(&server)
        .await;
    assert!(matches!(
        content::Api::for_test(server.uri())
            .remove_from_playlist(&credentials(), "a")
            .await,
        Err(Error::PlaylistChanged)
    ));
}

#[test]
fn device_identity_survives_restarts_and_logout() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::for_test(directory.path().to_owned());
    let id = store.device_id().unwrap();
    store.save(&credentials()).unwrap();
    store.clear().unwrap();
    assert_eq!(store.device_id().unwrap(), id);
    assert_eq!(
        Store::for_test(directory.path().to_owned())
            .device_id()
            .unwrap(),
        id
    );
}
