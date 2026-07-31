use url::Url;

use super::WebIntent;
use crate::ChannelState;

#[test]
fn local_session_lan_url_parses_without_server_root_match() {
    let url = Url::parse("http://192.168.1.5:1234/local-session/abcSecret").unwrap();
    let intent = WebIntent::try_from_url(&url).expect("LAN local-session URL should parse");
    match intent {
        WebIntent::LocalSessionView { secret } => assert_eq!(secret, "abcSecret"),
        other => panic!("expected LocalSessionView, got {other:?}"),
    }

    let native = WebIntent::try_from_url(&url).unwrap().into_intent_url();
    assert_eq!(
        native.as_str(),
        format!("{}://local_session/abcSecret", ChannelState::url_scheme())
    );
}

#[test]
fn local_session_empty_secret_is_rejected() {
    let url = Url::parse("http://192.168.1.5:1234/local-session/").unwrap();
    // Trailing slash may yield an empty final segment depending on Url parsing.
    // Either way it must not become a LocalSessionView with an empty secret.
    match WebIntent::try_from_url(&url) {
        Ok(WebIntent::LocalSessionView { secret }) => {
            assert!(!secret.is_empty(), "empty secret must not be accepted");
        }
        Ok(_) | Err(_) => {}
    }
}

#[test]
fn cloud_session_url_still_parses_as_session_view() {
    let session_id = "11111111-1111-1111-1111-111111111111";
    let url = Url::parse(&format!(
        "{}/session/{session_id}",
        ChannelState::server_root_url()
    ))
    .unwrap();

    let intent = WebIntent::try_from_url(&url).expect("cloud session URL should parse");
    match intent {
        WebIntent::SessionView(native) => {
            assert!(native.as_str().contains("shared_session"));
            assert!(native.as_str().contains(session_id));
        }
        other => panic!("expected SessionView, got {other:?}"),
    }
}

#[test]
fn unrelated_lan_path_is_not_a_web_intent() {
    let url = Url::parse("http://192.168.1.5:1234/not-a-session").unwrap();
    assert!(WebIntent::try_from_url(&url).is_err());
}
