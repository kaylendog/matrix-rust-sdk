use std::{collections::BTreeMap, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use eyeball_im::VectorDiff;
use futures_util::FutureExt;
use matrix_sdk::{
    authentication::oauth::{error::OAuthTokenRevocationError, OAuthError},
    config::{RequestConfig, StoreConfig, SyncSettings},
    store::RoomLoadSettings,
    sync::RoomUpdate,
    test_utils::{
        client::mock_matrix_session, mocks::MatrixMockServer, no_retry_test_client_with_server,
    },
    Client, Error, MemoryStore, StateChanges, StateStore,
};
use matrix_sdk_base::{sync::RoomUpdates, RoomState};
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    async_test, sync_state_event,
    test_json::{
        self,
        sync::{
            MIXED_INVITED_ROOM_ID, MIXED_JOINED_ROOM_ID, MIXED_KNOCKED_ROOM_ID, MIXED_LEFT_ROOM_ID,
            MIXED_SYNC,
        },
        sync_events::PINNED_EVENTS,
        TAG,
    },
    GlobalAccountDataTestEvent, JoinedRoomBuilder, SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
};
use ruma::{
    api::client::{
        directory::{
            get_public_rooms,
            get_public_rooms_filtered::{self, v3::Request as PublicRoomsFilterRequest},
        },
        uiaa,
    },
    assign, device_id,
    directory::Filter,
    event_id,
    events::{
        direct::{DirectEventContent, OwnedDirectUserIdentifier},
        AnyInitialStateEvent,
    },
    room_id,
    serde::Raw,
    user_id, OwnedUserId,
};
use serde_json::{json, Value as JsonValue};
use stream_assert::{assert_next_matches, assert_pending};
use tokio_stream::wrappers::BroadcastStream;
use wiremock::{
    matchers::{header, method, path, path_regex},
    Mock, Request, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn test_sync() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let response = client.sync_once(sync_settings).await.unwrap();

    assert_ne!(response.next_batch, "");
}

#[async_test]
async fn test_devices() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::DEVICES))
        .mount(&server)
        .await;

    client.devices().await.unwrap();
}

#[async_test]
async fn test_delete_devices() {
    let (client, server) = no_retry_test_client_with_server().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/delete_devices"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "flows": [
                {
                    "stages": [
                        "m.login.password"
                    ]
                }
            ],
            "params": {},
            "session": "vBslorikviAjxzYBASOBGfPp"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/delete_devices"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "flows": [
                {
                    "stages": [
                        "m.login.password"
                    ]
                }
            ],
            "params": {},
            "session": "vBslorikviAjxzYBASOBGfPp"
        })))
        .mount(&server)
        .await;

    let devices = &[device_id!("DEVICEID").to_owned()];

    if let Err(e) = client.delete_devices(devices, None).await {
        if let Some(info) = e.as_uiaa_response() {
            let mut auth_parameters = BTreeMap::new();

            let identifier = json!({
                "type": "m.id.user",
                "user": "example",
            });
            auth_parameters.insert("identifier".to_owned(), identifier);
            auth_parameters.insert("password".to_owned(), "wordpass".into());

            let auth_data = uiaa::AuthData::Password(assign!(
                uiaa::Password::new(
                    uiaa::UserIdentifier::UserIdOrLocalpart("example".to_owned()),
                    "wordpass".to_owned(),
                ), {
                    session: info.session.clone(),
                }
            ));

            client.delete_devices(devices, Some(auth_data)).await.unwrap();
        }
    }
}

#[async_test]
async fn test_resolve_room_alias() {
    let (client, server) = no_retry_test_client_with_server().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/directory/room/%23alias:example.org"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::GET_ALIAS))
        .mount(&server)
        .await;

    let alias = ruma::room_alias_id!("#alias:example.org");
    client.resolve_room_alias(alias).await.unwrap();
}

#[async_test]
async fn test_join_leave_room() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID);
    assert!(room.is_none());

    let sync_token = client.sync_once(SyncSettings::default()).await.unwrap().next_batch;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Joined);

    mock_sync(&server, &*test_json::LEAVE_SYNC_EVENT, Some(sync_token.clone())).await;

    client.sync_once(SyncSettings::default().token(sync_token)).await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_join_room_by_id() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/join"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_ID))
        .mount(&server)
        .await;

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
        // field
        client.join_room_by_id(&DEFAULT_TEST_ROOM_ID).await.unwrap().room_id(),
        *DEFAULT_TEST_ROOM_ID
    );
}

#[async_test]
async fn test_join_room_by_id_or_alias() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/join/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_ID))
        .mount(&server)
        .await;

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
        // field
        client
            .join_room_by_id_or_alias(
                (&**DEFAULT_TEST_ROOM_ID).into(),
                &["server.com".try_into().unwrap()]
            )
            .await
            .unwrap()
            .room_id(),
        *DEFAULT_TEST_ROOM_ID
    );
}

#[async_test]
async fn test_room_search_all() {
    let (client, server) = no_retry_test_client_with_server().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/publicRooms"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
        .mount(&server)
        .await;

    let get_public_rooms::v3::Response { chunk, .. } =
        client.public_rooms(Some(10), None, None).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn test_room_search_filtered() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/publicRooms"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
        .mount(&server)
        .await;

    let generic_search_term = Some("cheese".to_owned());
    let filter = assign!(Filter::new(), { generic_search_term });
    let request = assign!(PublicRoomsFilterRequest::new(), { filter });

    let get_public_rooms_filtered::v3::Response { chunk, .. } =
        client.public_rooms_filtered(request).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn test_invited_rooms() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::INVITE_SYNC, None).await;

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(client.left_rooms().is_empty());
    assert!(!client.invited_rooms().is_empty());

    let room = client.get_room(room_id!("!696r7674:example.com")).unwrap();
    assert_eq!(room.state(), RoomState::Invited);
}

#[async_test]
async fn test_left_rooms() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(!client.left_rooms().is_empty());
    assert!(client.invited_rooms().is_empty());

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_whoami() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/account/whoami"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
        .mount(&server)
        .await;

    let user_id = user_id!("@joe:example.org");

    assert_eq!(client.whoami().await.unwrap().user_id, user_id);
}

#[async_test]
async fn test_room_update_channel() {
    let (client, server) = logged_in_client_with_server().await;

    let mut rx = client.subscribe_to_room_updates(&DEFAULT_TEST_ROOM_ID);

    mock_sync(&server, &*test_json::SYNC, None).await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    client.sync_once(sync_settings).await.unwrap();

    let update = rx.recv().now_or_never().unwrap().unwrap();
    assert_let!(RoomUpdate::Joined { updates, .. } = update);

    assert_eq!(updates.account_data.len(), 1);
    assert_eq!(updates.ephemeral.len(), 1);
    assert_eq!(updates.state.len(), 9);

    assert!(updates.timeline.limited);
    assert_eq!(updates.timeline.events.len(), 1);
    assert_eq!(updates.timeline.prev_batch, Some("t392-516_47314_0_7_1_1_1_11444_1".to_owned()));

    assert_eq!(updates.unread_notifications.highlight_count, 0);
    assert_eq!(updates.unread_notifications.notification_count, 11);
}

#[async_test]
async fn test_subscribe_all_room_updates() {
    let (client, server) = logged_in_client_with_server().await;

    let mut rx = client.subscribe_to_all_room_updates();

    mock_sync(&server, &*MIXED_SYNC, None).await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    client.sync_once(sync_settings).await.unwrap();

    let room_updates = rx.recv().now_or_never().unwrap().unwrap();
    assert_let!(RoomUpdates { left, joined, invited, knocked } = room_updates);

    // Check the left room updates.
    {
        assert_eq!(left.len(), 1);

        let (room_id, update) = left.iter().next().unwrap();

        assert_eq!(room_id, *MIXED_LEFT_ROOM_ID);
        assert!(update.state.is_empty());
        assert_eq!(update.timeline.events.len(), 1);
        assert!(update.account_data.is_empty());
    }

    // Check the joined room updates.
    {
        assert_eq!(joined.len(), 1);

        let (room_id, update) = joined.iter().next().unwrap();

        assert_eq!(room_id, *MIXED_JOINED_ROOM_ID);

        assert_eq!(update.account_data.len(), 1);
        assert_eq!(update.ephemeral.len(), 1);
        assert_eq!(update.state.len(), 1);

        assert!(update.timeline.limited);
        assert_eq!(update.timeline.events.len(), 1);
        assert_eq!(update.timeline.prev_batch, Some("t392-516_47314_0_7_1_1_1_11444_1".to_owned()));

        assert_eq!(update.unread_notifications.highlight_count, 0);
        assert_eq!(update.unread_notifications.notification_count, 11);
    }

    // Check the invited room updates.
    {
        assert_eq!(invited.len(), 1);

        let (room_id, update) = invited.iter().next().unwrap();

        assert_eq!(room_id, *MIXED_INVITED_ROOM_ID);
        assert_eq!(update.invite_state.events.len(), 2);
    }

    // Check the knocked room updates.
    {
        assert_eq!(knocked.len(), 1);

        let (room_id, update) = knocked.iter().next().unwrap();

        assert_eq!(room_id, *MIXED_KNOCKED_ROOM_ID);
        assert_eq!(update.knock_state.events.len(), 2);
    }
}

// Check that the `Room::latest_encryption_state().await?.is_encrypted()` is
// properly deduplicated, meaning we only make a single request to the server,
// and that multiple calls do return the same result.
#[cfg(all(feature = "e2e-encryption", not(target_family = "wasm")))]
#[async_test]
async fn test_request_encryption_event_before_sending() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    client
        .sync_once(SyncSettings::default())
        .await
        .expect("We should be able to performs an initial sync");

    let room =
        client.get_room(&DEFAULT_TEST_ROOM_ID).expect("We should know about our default room");

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.encryption/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "rotation_period_ms": 604800000,
                    "rotation_period_msgs": 100
                }))
                // Introduce a delay so the first `latest_encryption_state()` doesn't finish before
                // we make the second call.
                .set_delay(Duration::from_millis(50)),
        )
        .mount(&server)
        .await;

    let first_handle = spawn({
        let room = room.to_owned();
        async move { room.to_owned().latest_encryption_state().await.map(|state| state.is_encrypted()) }
    });

    let second_handle =
        spawn(
            async move { room.latest_encryption_state().await.map(|state| state.is_encrypted()) },
        );

    let first_encrypted =
        first_handle.await.unwrap().expect("We should be able to test if the room is encrypted.");
    let second_encrypted =
        second_handle.await.unwrap().expect("We should be able to test if the room is encrypted.");

    assert!(first_encrypted, "We should have found out that the room is encrypted.");
    assert_eq!(
        first_encrypted, second_encrypted,
        "Both attempts to find out if the room is encrypted should return the same result."
    );
}

// Check that we're fetching account data from the server when marking a room as
// a DM.
#[async_test]
async fn test_marking_room_as_dm() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    client
        .sync_once(SyncSettings::default())
        .await
        .expect("We should be able to perform an initial sync");

    let account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("We should be able to fetch the account data event from the store");

    assert!(account_data.is_none(), "We should not have any account data initially");

    let bob = user_id!("@bob:example.com");
    let users = vec![bob.to_owned()];

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/m.direct"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@bob:example.com": [
                "!abcdefgh:example.com",
                "!hgfedcba:example.com"
            ],
            "@alice:example.com": [
                "!abcdefgh:example.com",
            ]
        })))
        .expect(1..)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    let put_direct_content_matcher = |request: &Request| {
        let content: DirectEventContent = request.body_json().expect(
            "The body of the PUT /account_data request should be a valid DirectEventContent",
        );

        let bob_entry = content
            .get(&OwnedDirectUserIdentifier::from(bob.to_owned()))
            .expect("We should have bob in the direct event content");

        assert_eq!(content.len(), 2, "We should have entries for bob and foo");
        assert_eq!(bob_entry.len(), 3, "Bob should have 3 direct rooms");

        content.len() == 2 && bob_entry.len() == 3
    };

    Mock::given(method("PUT"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/m.direct"))
        .and(header("authorization", "Bearer 1234"))
        .and(put_direct_content_matcher)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .named("m.direct account data PUT")
        .mount(&server)
        .await;

    client
        .account()
        .mark_as_dm(&DEFAULT_TEST_ROOM_ID, &users)
        .await
        .expect("We should be able to mark the room as a DM");

    server.verify().await;
}

// Check that we're fetching account data from the server when marking a room as
// a DM, and that we don't clobber the previous entry if it was impossible to
// deserialize.
#[async_test]
async fn test_marking_room_as_dm_fails_if_undeserializable() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    client
        .sync_once(SyncSettings::default())
        .await
        .expect("We should be able to perform an initial sync");

    let account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("We should be able to fetch the account data event from the store");

    assert!(account_data.is_none(), "We should not have any account data initially");

    let bob = user_id!("@bob:example.com");
    let users = vec![bob.to_owned()];

    // The response must be valid JSON, but not a valid `DirectEventContent`
    // representation.
    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/m.direct"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(["hey", null, true, 42])))
        .expect(1)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    let result = client.account().mark_as_dm(&DEFAULT_TEST_ROOM_ID, &users).await;

    assert_matches!(result, Err(Error::SerdeJson(_)));

    server.verify().await;
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_get_own_device() {
    let (client, _) = logged_in_client_with_server().await;

    let device = client
        .encryption()
        .get_own_device()
        .await
        .unwrap()
        .expect("We should always have access to our own device, even before any sync");

    assert!(
        device.user_id() == client.user_id().expect("The client should know about its user ID"),
        "The user ID of the client and our own device should match"
    );
    assert!(
        device.device_id()
            == client.device_id().expect("The client should know about its device ID"),
        "The device ID of the client and our own device should match"
    );
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_cross_signing_status() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/signatures/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "failures": {}
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {}
        })))
        .mount(&server)
        .await;

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to fetch our cross-signing status");

    assert!(
        !status.has_master && !status.has_self_signing && !status.has_user_signing,
        "Initially we shouldn't have any cross-signing keys"
    );

    client.encryption().bootstrap_cross_signing(None).await.unwrap();

    client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to fetch our cross-signing status");

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should have the private cross-signing keys after the bootstrap process");
    assert!(status.is_complete(), "We should have all the private cross-signing keys locally");

    server.verify().await;
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_encrypt_room_event() {
    use std::sync::Arc;

    use ruma::events::room::encrypted::RoomEncryptedEventContent;

    let (client, server) = logged_in_client_with_server().await;
    let user_id = client.user_id().unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": {
                user_id: {}
            }
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    client
        .sync_once(SyncSettings::default())
        .await
        .expect("We should be able to performs an initial sync");

    let room =
        client.get_room(&DEFAULT_TEST_ROOM_ID).expect("We should know about our default room");

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.encryption/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "rotation_period_ms": 604800000,
            "rotation_period_msgs": 100
        })))
        .mount(&server)
        .await;

    assert!(
        room.latest_encryption_state()
            .await
            .expect("We should be able to check if the room is encrypted")
            .is_encrypted(),
        "The room should be encrypted"
    );

    Mock::given(method("GET"))
        .and(path_regex("/_matrix/client/r0/rooms/!SVkFJHzfwvuaIEawgC:localhost/members"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "chunk": []})))
        .mount(&server)
        .await;

    let event_content = Arc::new(std::sync::Mutex::new(None));

    let event_content_matcher = {
        let event_content = event_content.to_owned();
        move |request: &Request| {
            let mut path_segments =
                request.url.path_segments().expect("The URL should be able to be a base");

            let event_type = path_segments
                .nth_back(1)
                .expect("The path should have a event type as the last segment")
                .to_owned();

            assert_eq!(
                event_type, "m.room.encrypted",
                "The event type should be the `m.room.encrypted` event type"
            );

            let content: RoomEncryptedEventContent = request
                .body_json()
                .expect("The uploaded content should be a valid `m.room.encrypted` event content");

            *event_content.lock().unwrap() = Some(content);

            true
        }
    };

    Mock::given(method("PUT"))
        .and(path(
            "/_matrix/client/r0/rooms/!SVkFJHzfwvuaIEawgC:localhost/send/m.room.encrypted/foobar",
        ))
        .and(event_content_matcher)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": "$foobar"
        })))
        .mount(&server)
        .await;

    room.send_raw("m.room.message", json!({"body": "Hello", "msgtype": "m.text"}))
        .with_transaction_id("foobar".into())
        .await
        .expect("We should be able to send a message to the encrypted room");

    let content = event_content
        .lock()
        .unwrap()
        .take()
        .expect("We should have intercepted an `m.room.encrypted` event content");

    let event = Raw::new(&json!({
        "room_id": room.room_id(),
        "event_id": "$foobar",
        "origin_server_ts": 1600000u64,
        "sender": user_id,
        "content": content,
    }))
    .expect("We should be able to construct a full event from the encrypted event content")
    .cast_unchecked();

    let push_ctx =
        room.push_context().await.expect("We should be able to get the push action context");
    let timeline_event = room
        .decrypt_event(&event, push_ctx.as_ref())
        .await
        .expect("We should be able to decrypt an event that we ourselves have encrypted");

    let event = timeline_event
        .raw()
        .deserialize()
        .expect("We should be able to deserialize the decrypted event");

    assert_let!(
        ruma::events::AnySyncTimelineEvent::MessageLike(
            ruma::events::AnySyncMessageLikeEvent::RoomMessage(message_event)
        ) = event
    );

    let message_event =
        message_event.as_original().expect("The decrypted event should not be a redacted event");

    assert_eq!(
        message_event.content.body(),
        "Hello",
        "The now decrypted message should match to our plaintext payload"
    );
}

#[cfg(not(feature = "e2e-encryption"))]
#[async_test]
async fn test_create_dm_non_encrypted() {
    let (client, server) = logged_in_client_with_server().await;
    let user_id = user_id!("@invitee:localhost");

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/createRoom"))
        .and(|request: &Request| {
            // The body is JSON.
            let Ok(body) = request.body_json::<Raw<JsonValue>>() else {
                return false;
            };

            // The body's `direct` field is set to `true`.
            if !body.get_field::<bool>("is_direct").is_ok_and(|b| b == Some(true)) {
                return false;
            }

            // The body's `preset` field is set to `trusted_private_chat`.
            if !body
                .get_field::<String>("preset")
                .is_ok_and(|s| s.as_deref() == Some("trusted_private_chat"))
            {
                return false;
            }

            // The body's `invite` field is set to an array with the user ID.
            if !body
                .get_field::<Vec<OwnedUserId>>("invite")
                .is_ok_and(|v| v.as_deref() == Some(&[user_id.to_owned()]))
            {
                return false;
            }

            // There is no initial state.
            body.get_field::<Vec<Raw<AnyInitialStateEvent>>>("initial_state")
                .is_ok_and(|v| v.is_none())
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "room_id": "!sefiuhWgwghwWgh:example.com"
        })))
        .expect(1)
        .mount(&server)
        .await;

    client.create_dm(user_id).await.unwrap();
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_create_dm_encrypted() {
    let (client, server) = logged_in_client_with_server().await;
    let user_id = user_id!("@invitee:localhost");

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/createRoom"))
        .and(|request: &Request| {
            // The body is JSON.
            let Ok(body) = request.body_json::<Raw<JsonValue>>() else {
                return false;
            };

            // The body's `direct` field is set to `true`.
            if !body.get_field::<bool>("is_direct").is_ok_and(|b| b == Some(true)) {
                return false;
            }

            // The body's `preset` field is set to `trusted_private_chat`.
            if !body
                .get_field::<String>("preset")
                .is_ok_and(|s| s.as_deref() == Some("trusted_private_chat"))
            {
                return false;
            }

            // The body's `invite` field is set to an array with the user ID.
            if !body
                .get_field::<Vec<OwnedUserId>>("invite")
                .is_ok_and(|v| v.as_deref() == Some(&[user_id.to_owned()]))
            {
                return false;
            }

            // The body's `initial_state` field is set to an array with an
            // `m.room.encryption` event.
            body.get_field::<Vec<Raw<AnyInitialStateEvent>>>("initial_state").is_ok_and(|v| {
                let Some(v) = v else {
                    return false;
                };

                if v.len() != 1 {
                    return false;
                }

                let initial_event = &v[0];

                initial_event
                    .get_field::<String>("type")
                    .is_ok_and(|s| s.as_deref() == Some("m.room.encryption"))
            })
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "room_id": "!sefiuhWgwghwWgh:example.com"
        })))
        .expect(1)
        .mount(&server)
        .await;

    client.create_dm(user_id).await.unwrap();
}

#[async_test]
async fn test_create_dm_error() {
    let (client, _server) = logged_in_client_with_server().await;
    let user_id = user_id!("@invitee:localhost");

    // The endpoint is not mocked so we encounter a 404.
    let error = client.create_dm(user_id).await.unwrap_err();
    let client_api_error = error.as_client_api_error().unwrap();

    assert_eq!(client_api_error.status_code, 404);
}

#[async_test]
async fn test_test_ambiguity_changes() {
    let (client, server) = logged_in_client_with_server().await;

    let example_id = user_id!("@example:localhost");
    let example_2_id = user_id!("@example2:localhost");
    let example_3_id = user_id!("@example3:localhost");

    let mut updates = BroadcastStream::new(client.subscribe_to_room_updates(&DEFAULT_TEST_ROOM_ID));
    assert_pending!(updates);

    // Initial sync, adds 2 members.
    mock_sync(&server, &*test_json::SYNC, None).await;
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let changes = &response.rooms.joined.get(*DEFAULT_TEST_ROOM_ID).unwrap().ambiguity_changes;

    // A new member always triggers an ambiguity change.
    let example_change = changes.get(event_id!("$151800140517rfvjc:localhost")).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    let example_2_change = changes.get(event_id!("$152034824468gOeNB:localhost")).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id).await.unwrap().unwrap();
    assert!(!example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id).await.unwrap().unwrap();
    assert!(!example_2.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_change = changes.get(event_id!("$151800140517rfvjc:localhost")).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    let example_2_change = changes.get(event_id!("$152034824468gOeNB:localhost")).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    // Add 1 member and set all 3 to the same display name.
    let example_2_rename_1_event_id = event_id!("$example_2_rename_1");
    let example_3_join_event_id = event_id!("$example_3_join");

    let mut sync_builder = SyncResponseBuilder::new();
    let joined_room = JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_bulk([
        sync_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": example_2_rename_1_event_id,
            "origin_server_ts": 151800140,
            "sender": example_2_id,
            "state_key": example_2_id,
            "type": "m.room.member",
        }),
        sync_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": example_3_join_event_id,
            "origin_server_ts": 151800140,
            "sender": example_3_id,
            "state_key": example_3_id,
            "type": "m.room.member",
        }),
    ]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let changes = &response.rooms.joined.get(*DEFAULT_TEST_ROOM_ID).unwrap().ambiguity_changes;

    // First joined member made both members ambiguous.
    let example_2_change = changes.get(example_2_rename_1_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member.as_deref(), Some(example_id));
    assert_eq!(example_2_change.disambiguated_member, None);

    // Second joined member only adds itself as ambiguous.
    let example_3_change = changes.get(example_3_join_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_eq!(example_3_change.ambiguated_member, None);
    assert_eq!(example_3_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id).await.unwrap().unwrap();
    assert!(example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id).await.unwrap().unwrap();
    assert!(example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_2_change = changes.get(example_2_rename_1_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member.as_deref(), Some(example_id));
    assert_eq!(example_2_change.disambiguated_member, None);

    let example_3_change = changes.get(example_3_join_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_eq!(example_3_change.ambiguated_member, None);
    assert_eq!(example_3_change.disambiguated_member, None);

    // Rename example 2 to a unique name.
    let example_2_rename_2_event_id = event_id!("$example_2_rename_2");

    let joined_room =
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_bulk([sync_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "another example",
                "membership": "join"
            },
            "event_id": example_2_rename_2_event_id,
            "origin_server_ts": 151800140,
            "sender": example_2_id,
            "state_key": example_2_id,
            "type": "m.room.member",
        })]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let changes = &response.rooms.joined.get(*DEFAULT_TEST_ROOM_ID).unwrap().ambiguity_changes;

    // example 2 is not ambiguous anymore.
    let example_2_change = changes.get(example_2_rename_2_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id).await.unwrap().unwrap();
    assert!(example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id).await.unwrap().unwrap();
    assert!(!example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_2_change = changes.get(example_2_rename_2_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    // Rename example 3, using the same name as example 2.
    let example_3_rename_event_id = event_id!("$example_3_rename");

    let joined_room =
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_bulk([sync_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "another example",
                "membership": "join"
            },
            "event_id": example_3_rename_event_id,
            "origin_server_ts": 151800140,
            "sender": example_3_id,
            "state_key": example_3_id,
            "type": "m.room.member",
        })]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let changes = &response.rooms.joined.get(*DEFAULT_TEST_ROOM_ID).unwrap().ambiguity_changes;

    // example 3 is now ambiguous with example 2, not example.
    let example_3_change = changes.get(example_3_rename_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_eq!(example_3_change.ambiguated_member.as_deref(), Some(example_2_id));
    assert_eq!(example_3_change.disambiguated_member.as_deref(), Some(example_id));

    let example = room.get_member_no_sync(example_id).await.unwrap().unwrap();
    assert!(!example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id).await.unwrap().unwrap();
    assert!(example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_3_change = changes.get(example_3_rename_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_eq!(example_3_change.ambiguated_member.as_deref(), Some(example_2_id));
    assert_eq!(example_3_change.disambiguated_member.as_deref(), Some(example_id));

    // Rename example, still using a unique name.
    let example_rename_event_id = event_id!("$example_rename");

    let joined_room =
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_bulk([sync_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "the first example",
                "membership": "join"
            },
            "event_id": example_rename_event_id,
            "origin_server_ts": 151800140,
            "sender": example_id,
            "state_key": example_id,
            "type": "m.room.member",
        })]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let changes = &response.rooms.joined.get(*DEFAULT_TEST_ROOM_ID).unwrap().ambiguity_changes;

    // name change, even if still not ambiguous, triggers ambiguity change.
    let example_change = changes.get(example_rename_event_id).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id).await.unwrap().unwrap();
    assert!(!example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id).await.unwrap().unwrap();
    assert!(example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_change = changes.get(example_rename_event_id).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    // Change avatar.
    let example_avatar_event_id = event_id!("$example_avatar");

    let joined_room =
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_bulk([sync_state_event!({
            "content": {
                "avatar_url": "mxc://localhost/avatar",
                "displayname": "the first example",
                "membership": "join"
            },
            "event_id": example_avatar_event_id,
            "origin_server_ts": 151800140,
            "sender": example_id,
            "state_key": example_id,
            "type": "m.room.member",
        })]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    // Avatar change does not trigger ambiguity change.
    assert!(response.rooms.joined.get(*DEFAULT_TEST_ROOM_ID).unwrap().ambiguity_changes.is_empty());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);
    assert!(changes.is_empty());

    assert_pending!(updates);
}

#[cfg(not(target_family = "wasm"))]
#[async_test]
async fn test_rooms_stream() {
    use futures_util::StreamExt as _;

    let (client, server) = logged_in_client_with_server().await;
    let (rooms, mut rooms_stream) = client.rooms_stream();

    assert!(rooms.is_empty());
    assert_pending!(rooms_stream);

    let room_id_1 = room_id!("!room0:matrix.org");
    let room_id_2 = room_id!("!room1:matrix.org");
    let room_id_3 = room_id!("!room2:matrix.org");

    let payload = json!({
        "next_batch": "foo",
        "rooms": {
            "invite": {},
            "join": {
                room_id_1: {},
                room_id_2: {},
                room_id_3: {},
            },
            "leave": {}
        },
    });

    mock_sync(&server, &payload, None).await;

    assert!(client.get_room(room_id_1).is_none());
    assert!(client.get_room(room_id_2).is_none());
    assert!(client.get_room(room_id_3).is_none());

    client.sync_once(SyncSettings::default()).await.unwrap();

    // Rooms are created.
    assert!(client.get_room(room_id_1).is_some());
    assert!(client.get_room(room_id_2).is_some());
    assert!(client.get_room(room_id_3).is_some());

    // We receive 3 diffs…
    assert_let!(Some(diffs) = rooms_stream.next().await);
    assert_eq!(diffs.len(), 3);

    // … which map to the new rooms!
    assert_let!(VectorDiff::PushBack { value: room_1 } = &diffs[0]);
    assert_eq!(room_1.room_id(), room_id_1);
    assert_let!(VectorDiff::PushBack { value: room_2 } = &diffs[1]);
    assert_eq!(room_2.room_id(), room_id_2);
    assert_let!(VectorDiff::PushBack { value: room_3 } = &diffs[2]);
    assert_eq!(room_3.room_id(), room_id_3);

    assert_pending!(rooms_stream);
}

#[async_test]
async fn test_dms_are_processed_in_any_sync_response() {
    let (client, server) = logged_in_client_with_server().await;
    let user_a_id = user_id!("@a:e.uk");
    let user_b_id = user_id!("@b:e.uk");
    let room_id_1 = room_id!("!r:e.uk");
    let room_id_2 = room_id!("!s:e.uk");

    let joined_room_builder = JoinedRoomBuilder::new(room_id_1);
    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(
        json!({
          "content": {
            user_a_id: [
                room_id_1
            ],
            user_b_id: [
                room_id_2
            ]
          },
          "type": "m.direct",
          "event_id": "$757957878228ekrDs:localhost",
            "origin_server_ts": 17195787,
            "sender": "@example:localhost",
            "state_key": "",
            "type": "m.direct",
            "unsigned": {
              "age": 139298
            }
        }),
    ));
    sync_response_builder.add_joined_room(joined_room_builder);
    let json_response = sync_response_builder.build_json_sync_response();

    mock_sync(&server, json_response, None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let room_1 = client.get_room(room_id_1).unwrap();
    assert!(room_1.is_direct().await.unwrap());

    // Now perform a sync without new account data
    let joined_room_builder = JoinedRoomBuilder::new(room_id_2);
    sync_response_builder.add_joined_room(joined_room_builder);
    let json_response = sync_response_builder.build_json_sync_response();

    mock_sync(&server, json_response, None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let room_2 = client.get_room(room_id_2).unwrap();
    assert!(room_2.is_direct().await.unwrap());
}

#[async_test]
async fn test_restore_room() {
    let room_id = room_id!("!stored_room:localhost");

    // Create memory store with some room data.
    let store = MemoryStore::new();

    let mut changes = StateChanges::default();

    let raw_tag_event = Raw::new(&*TAG).unwrap().cast_unchecked();
    let tag_event = raw_tag_event.deserialize().unwrap();
    changes.add_room_account_data(room_id, tag_event, raw_tag_event);

    let raw_pinned_events_event = Raw::new(&*PINNED_EVENTS).unwrap().cast_unchecked();
    let pinned_events_event = raw_pinned_events_event.deserialize().unwrap();
    changes.add_state_event(room_id, pinned_events_event, raw_pinned_events_event);

    let room_info = serde_json::from_value(json!({
        "room_id": room_id,
        "room_state": "Joined",
        "notification_counts": {
            "highlight_count": 0,
            "notification_count": 0,
        },
        "summary": {
            "room_heroes": [],
            "joined_member_count": 1,
            "invited_member_count": 0,
        },
        "members_synced": true,
        "last_prev_batch": "pb",
        "sync_info": "FullySynced",
        "encryption_state_synced": true,
        "base_info": {
            "avatar": null,
            "canonical_alias": null,
            "create": null,
            "dm_targets": [],
            "encryption": null,
            "guest_access": null,
            "history_visibility": null,
            "join_rules": null,
            "max_power_level": 100,
            "name": null,
            "tombstone": null,
            "topic": null,
        },
    }))
    .unwrap();
    changes.add_room(room_info);

    store.save_changes(&changes).await.unwrap();

    // Build a client with that store.
    let store_config =
        StoreConfig::new("cross-process-store-locks-holder-name".to_owned()).state_store(store);
    let client = Client::builder()
        .homeserver_url("http://localhost:1234")
        .request_config(RequestConfig::new().disable_retry())
        .store_config(store_config)
        .build()
        .await
        .unwrap();
    client
        .matrix_auth()
        .restore_session(mock_matrix_session(), RoomLoadSettings::default())
        .await
        .unwrap();

    let room = client.get_room(room_id).unwrap();
    assert!(room.is_favourite());
    assert!(!room.pinned_event_ids().unwrap().is_empty());
}

#[async_test]
async fn test_logout() {
    let server = MatrixMockServer::new().await;

    // Test unauthenticated client.
    let unlogged_client = server.client_builder().unlogged().build().await;
    let res = unlogged_client.logout().await;
    assert_matches!(res, Err(Error::AuthenticationRequired));

    // Test MatrixAuth.
    server.mock_logout().ok().mock_once().named("matrix_logout").mount().await;

    let matrix_auth_client = server.client_builder().build().await;
    matrix_auth_client.logout().await.unwrap();

    // Test OAuth.
    server
        .oauth()
        .mock_server_metadata()
        .ok()
        .mock_once()
        .named("oauth_server_metadata")
        .mount()
        .await;

    let oauth_client = server.client_builder().logged_in_with_oauth().build().await;
    let res = oauth_client.logout().await;

    // This returns an error because it requires a HTTPS server URI, or to be able
    // to call `OAuth::insecure_rewrite_https_to_http()`, but at least we are
    // testing the OAuth branch inside `Client::logout()`.
    assert_matches!(res, Err(Error::OAuth(oauth_error)));
    assert_matches!(*oauth_error, OAuthError::Logout(OAuthTokenRevocationError::Url(_)));
}
