//! A server set to drive a background desktop never drives the live desktop instead: a
//! setting it can't use fails every call with `background_unavailable`, and a desktop
//! that doesn't start fails the call that tried, and the next call tries again. Until the
//! first call it lists the client's tools itself, without Noctalia's, which belong to the
//! host's shell. None of these runs reaches the fake niri's desktop.

use serde_json::json;

use crate::client::{Server, tool_error};
use crate::fixture::Fixture;
use crate::niri::Niri;
use crate::shared::guardians;

/// Writes the fixture's policy file.
fn policy(fixture: &Fixture, text: &str) {
    std::fs::create_dir_all(fixture.path("config/niri-computer-use")).unwrap();
    std::fs::write(fixture.path("config/niri-computer-use/policy.toml"), text).unwrap();
}

/// The names of the tools `server` lists.
async fn names(server: &mut Server) -> Vec<String> {
    server
        .tools()
        .await
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

/// Calls each of a few tools, which must fail with `background_unavailable` and `reason`
/// in the detail, and with no crash guardian started.
async fn every_call_unavailable(server: &mut Server, reason: &str) {
    let listed = names(server).await;
    assert!(
        !listed
            .iter()
            .any(|name| name == "shell_panel" || name == "noctalia"),
        "{listed:?}"
    );
    for tool in ["status", "desktop_state", "acquire_desktop", "screenshot"] {
        assert!(listed.iter().any(|name| name == tool), "{listed:?}");
        let (error, detail) = tool_error(&server.call(tool, json!({})).await);
        assert_eq!(error, "background_unavailable", "{tool}: {detail}");
        assert!(detail.contains(reason), "{tool}: {detail}");
        assert!(detail.contains("never drives the live desktop"), "{detail}");
    }
    assert_eq!(guardians(i32::try_from(server.pid).unwrap()), 0);
}

#[tokio::test]
async fn a_wrong_variable_fails_every_call_and_never_drives_the_live_desktop() {
    let mut fixture = Fixture::new("bg-variable");
    fixture.set("NIRI_COMPUTER_USE_BACKGROUND", "yes");
    let _niri = Niri::start(&fixture);
    let mut server = Server::start(&fixture).await;
    every_call_unavailable(&mut server, "NIRI_COMPUTER_USE_BACKGROUND is \"yes\"").await;
    let (_, _, stderr) = server.stop().await;
    assert!(
        stderr.contains("so every tool fails with background_unavailable"),
        "{stderr}"
    );
}

/// A mistake elsewhere in a file that asks for a background desktop makes it invalid, but
/// mustn't put the agent on the live desktop.
#[tokio::test]
async fn an_invalid_file_asking_for_one_fails_every_call() {
    let fixture = Fixture::new("bg-invalid");
    policy(&fixture, "[background]\nwidth = 0\n");
    let _niri = Niri::start(&fixture);
    let mut server = Server::start(&fixture).await;
    every_call_unavailable(
        &mut server,
        "asks for a background desktop, but it is invalid",
    )
    .await;
    server.stop().await;
}

#[tokio::test]
async fn a_required_shared_engine_conflicts_with_it() {
    let mut fixture = Fixture::new("bg-shared");
    // The standalone run's variable would win over the file's `shared`.
    fixture.unset("NIRI_COMPUTER_USE_SHARED");
    policy(&fixture, "shared = \"required\"\n[background]\n");
    let mut server = Server::start(&fixture).await;
    every_call_unavailable(&mut server, "shared = \"required\"").await;
    server.stop().await;
}

/// The desktop starts on the first call, not before, and one that fails fails that call;
/// the next call tries again, and what a start made is undone.
#[tokio::test]
async fn a_start_that_fails_fails_the_call_and_the_next_call_tries_again() {
    let mut fixture = Fixture::new("bg-retry");
    fixture.set("NIRI_COMPUTER_USE_BACKGROUND", "private");
    let mut server = Server::start(&fixture).await;
    let listed = names(&mut server).await;
    assert!(
        !listed.iter().any(|name| name == "shell_panel"),
        "{listed:?}"
    );
    let work = fixture.runtime_dir().join("background");
    assert!(!work.exists(), "nothing starts before the first call");
    for _ in 0..2 {
        let (error, detail) = tool_error(&server.call("status", json!({})).await);
        assert_eq!(error, "background_unavailable", "{detail}");
        assert!(detail.contains("the next call tries again"), "{detail}");
        assert!(detail.contains("niri.test.sock"), "{detail}");
        assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0, "{detail}");
    }
    assert_eq!(guardians(i32::try_from(server.pid).unwrap()), 0);
    server.stop().await;
}
