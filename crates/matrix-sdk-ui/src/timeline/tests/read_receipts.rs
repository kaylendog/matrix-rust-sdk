// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use eyeball_im::VectorDiff;
use matrix_sdk::assert_next_matches_with_timeout;
use matrix_sdk_test::{ALICE, BOB, CAROL, async_test, event_factory::EventFactory};
use ruma::{
    event_id,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::message::{MessageType, RoomMessageEventContent, SyncRoomMessageEvent},
    },
    owned_event_id, room_id,
    room_version_rules::RoomVersionRules,
    uint,
};
use stream_assert::{assert_next_matches, assert_pending};

use super::{ReadReceiptMap, TestRoomDataProvider};
use crate::timeline::{
    MsgLikeContent, MsgLikeKind, TimelineFocus, controller::TimelineSettings,
    tests::TestTimelineBuilder,
};

fn filter_notice(ev: &AnySyncTimelineEvent, _rules: &RoomVersionRules) -> bool {
    match ev {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(msg),
        )) => !matches!(msg.content.msgtype, MessageType::Notice(_)),
        _ => true,
    }
}

#[async_test]
async fn test_read_receipts_updates_on_live_events() {
    let timeline = TestTimelineBuilder::new()
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE)).await;
    timeline.handle_live_event(f.text_msg("B").sender(*BOB)).await;

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let _date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // Implicit read receipt of Bob.
    let item_b = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_b = item_b.as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 1);
    assert!(event_b.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_event(f.text_msg("C").sender(*BOB)).await;

    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 2, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let item_c = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    timeline.handle_live_event(f.text_msg("D").sender(*ALICE)).await;

    let item_d = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_d = item_d.as_event().unwrap();
    assert!(event_d.read_receipts().is_empty());

    // Explicit read receipt is updated.
    timeline
        .handle_read_receipts([(
            event_d.event_id().unwrap().to_owned(),
            ReceiptType::Read,
            BOB.to_owned(),
            ReceiptThread::Unthreaded,
        )])
        .await;

    let item_c = assert_next_matches!(stream, VectorDiff::Set { index: 3, value } => value);
    let event_c = item_c.as_event().unwrap();
    assert!(event_c.read_receipts().is_empty());

    let item_d = assert_next_matches!(stream, VectorDiff::Set { index: 4, value } => value);
    let event_d = item_d.as_event().unwrap();
    assert_eq!(event_d.read_receipts().len(), 1);
    assert!(event_d.read_receipts().get(*BOB).is_some());
}

#[async_test]
async fn test_read_receipts_updates_on_back_paginated_events() {
    let timeline = TestTimelineBuilder::new()
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();
    let room_id = room_id!("!room:localhost");

    let f = EventFactory::new().room(room_id);

    timeline
        .handle_back_paginated_event(
            f.text_msg("A").sender(*BOB).event_id(event_id!("$event_a")).into_raw_timeline(),
        )
        .await;
    timeline
        .handle_back_paginated_event(
            f.text_msg("B")
                .sender(*CAROL)
                .event_id(event_id!("$event_with_bob_receipt"))
                .into_raw_timeline(),
        )
        .await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 3);
    assert!(items[0].is_date_divider());

    // Implicit read receipt of Bob.
    let event_a = items[2].as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Carol, explicit read receipt of Bob ignored.
    let event_b = items[1].as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 1);
    assert!(event_b.read_receipts().get(*CAROL).is_some());
}

#[async_test]
async fn test_read_receipts_updates_on_filtered_events() {
    let timeline = TestTimelineBuilder::new()
        .settings(TimelineSettings {
            track_read_receipts: true,
            event_filter: Arc::new(filter_notice),
            ..Default::default()
        })
        .build();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE)).await;
    timeline.handle_live_event(f.notice("B").sender(*BOB)).await;

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let _date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // Implicit read receipt of Bob.
    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_event(f.text_msg("C").sender(*BOB)).await;

    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let item_c = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    // Populate more events.
    let event_d_id = owned_event_id!("$event_d");

    timeline.handle_live_event(f.notice("D").sender(*ALICE).event_id(&event_d_id)).await;

    timeline.handle_live_event(f.text_msg("E").sender(*ALICE)).await;

    let item_e = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_e = item_e.as_event().unwrap();
    assert!(event_e.read_receipts().is_empty());

    // Explicit read receipt is updated but its visible event doesn't change.
    timeline
        .handle_read_receipts([(
            event_d_id,
            ReceiptType::Read,
            BOB.to_owned(),
            ReceiptThread::Unthreaded,
        )])
        .await;

    // Explicit read receipt is updated and its visible event changes.
    timeline
        .handle_read_receipts([(
            event_e.event_id().unwrap().to_owned(),
            ReceiptType::Read,
            BOB.to_owned(),
            ReceiptThread::Unthreaded,
        )])
        .await;

    let item_c = assert_next_matches!(stream, VectorDiff::Set { index: 2, value } => value);
    let event_c = item_c.as_event().unwrap();
    assert!(event_c.read_receipts().is_empty());

    let item_e = assert_next_matches!(stream, VectorDiff::Set { index: 3, value } => value);
    let event_e = item_e.as_event().unwrap();
    assert_eq!(event_e.read_receipts().len(), 1);
    assert!(event_e.read_receipts().get(*BOB).is_some());

    assert_pending!(stream);
}

#[async_test]
async fn test_read_receipts_updates_on_filtered_events_with_stored() {
    let event_with_bob_receipt_id = event_id!("$event_with_bob_receipt");

    // Add initial unthreaded private receipt.
    let mut initial_user_receipts = ReadReceiptMap::new();
    initial_user_receipts
        .entry(ReceiptType::Read)
        .or_default()
        .entry(ReceiptThread::Unthreaded)
        .or_default()
        .insert(
            BOB.to_owned(),
            (
                event_with_bob_receipt_id.to_owned(),
                Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(5))),
            ),
        );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings {
            track_read_receipts: true,
            event_filter: Arc::new(filter_notice),
            ..Default::default()
        })
        .build();
    let f = &timeline.factory;
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_event(f.text_msg("A").sender(*ALICE)).await;
    timeline
        .handle_live_event(f.notice("B").sender(*CAROL).event_id(event_with_bob_receipt_id))
        .await;

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let _date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // Stored read receipt of Bob.
    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Carol.
    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 2);
    assert!(event_a.read_receipts().get(*BOB).is_some());
    assert!(event_a.read_receipts().get(*CAROL).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_event(f.text_msg("C").sender(*BOB)).await;

    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);

    let item_c = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    assert_pending!(stream);
}

#[async_test]
async fn test_read_receipts_updates_on_back_paginated_filtered_events() {
    let event_with_bob_receipt_id = event_id!("$event_with_bob_receipt");

    // Add initial unthreaded private receipt.
    let mut initial_user_receipts = ReadReceiptMap::new();
    initial_user_receipts
        .entry(ReceiptType::Read)
        .or_default()
        .entry(ReceiptThread::Unthreaded)
        .or_default()
        .insert(
            BOB.to_owned(),
            (
                event_with_bob_receipt_id.to_owned(),
                Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(5))),
            ),
        );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings {
            track_read_receipts: true,
            event_filter: Arc::new(filter_notice),
            ..Default::default()
        })
        .build();
    let mut stream = timeline.subscribe().await;
    let room_id = room_id!("!room:localhost");

    let f = EventFactory::new().room(room_id);

    timeline
        .handle_back_paginated_event(
            f.text_msg("A").sender(*ALICE).event_id(event_id!("$event_a")).into_raw_timeline(),
        )
        .await;
    timeline
        .handle_back_paginated_event(
            f.notice("B").sender(*CAROL).event_id(event_with_bob_receipt_id).into_raw_timeline(),
        )
        .await;

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    // Add non-filtered event to show read receipts.
    timeline
        .handle_back_paginated_event(
            f.text_msg("C").sender(*CAROL).event_id(event_id!("$event_c")).into_raw_timeline(),
        )
        .await;

    // Implicit read receipt of Carol.
    let item_c = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 2);
    assert!(event_c.read_receipts().get(*BOB).is_some());
    assert!(event_c.read_receipts().get(*CAROL).is_some());

    // Reinsert a new date divider before the first back-paginated event.
    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    // Remove the last date divider.
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });

    assert_pending!(stream);
}

#[async_test]
async fn test_read_receipts_updates_on_message_decryption() {
    use std::{io::Cursor, iter};

    use assert_matches2::assert_let;
    use matrix_sdk_base::crypto::{OlmMachine, decrypt_room_key_export};
    use ruma::{
        events::room::encrypted::{
            EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
        },
        user_id,
    };

    use crate::timeline::{EncryptedMessage, TimelineItemContent};

    fn filter_text_msg(ev: &AnySyncTimelineEvent, _rules: &RoomVersionRules) -> bool {
        match ev {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncRoomMessageEvent::Original(msg),
            )) => !matches!(msg.content.msgtype, MessageType::Text(_)),
            _ => true,
        }
    }

    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

    let timeline = TestTimelineBuilder::new()
        .settings(TimelineSettings {
            track_read_receipts: true,
            event_filter: Arc::new(filter_text_msg),
            ..Default::default()
        })
        .build();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.notice("I am not encrypted").sender(&CAROL)).await;

    timeline
        .handle_live_event(
            f.event(RoomEncryptedEventContent::new(
                EncryptedEventScheme::MegolmV1AesSha2(
                    MegolmV1AesSha2ContentInit {
                        ciphertext: "\
                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                            .to_owned(),
                        sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA".to_owned(),
                        device_id: "NLAZCWIOCO".into(),
                        session_id: SESSION_ID.into(),
                    }
                    .into(),
                ),
                None,
            ))
            .sender(&BOB)
            .into_utd_sync_timeline_event(),
        )
        .await;

    assert_eq!(timeline.controller.items().await.len(), 3);

    // The first event only has Carol's receipt.
    let clear_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let clear_event = clear_item.as_event().unwrap();
    assert!(clear_event.content().is_message());
    assert_eq!(clear_event.read_receipts().len(), 1);
    assert!(clear_event.read_receipts().get(*CAROL).is_some());

    let _date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // The second event is encrypted and only has Bob's receipt.
    let encrypted_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let encrypted_event = encrypted_item.as_event().unwrap();

    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::UnableToDecrypt(EncryptedMessage::MegolmV1AesSha2 {
                session_id,
                ..
            }),
            ..
        }) = encrypted_event.content()
    );

    assert_eq!(session_id, SESSION_ID);
    assert_eq!(encrypted_event.read_receipts().len(), 1);
    assert!(encrypted_event.read_receipts().get(*BOB).is_some());

    // Decrypt encrypted message.
    let own_user_id = user_id!("@example:morheus.localhost");
    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;
    olm_machine.store().import_exported_room_keys(exported_keys, |_, _| {}).await.unwrap();

    timeline
        .controller
        .retry_event_decryption_test(
            room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
            olm_machine,
            Some(iter::once(SESSION_ID.to_owned()).collect()),
        )
        .await;

    // The first event now has both receipts.
    let clear_item =
        assert_next_matches_with_timeout!(stream, VectorDiff::Set { index: 1, value } => value);
    let clear_event = clear_item.as_event().unwrap();
    assert!(clear_event.content().is_message());
    assert_eq!(clear_event.read_receipts().len(), 2);
    assert!(clear_event.read_receipts().get(*CAROL).is_some());
    assert!(clear_event.read_receipts().get(*BOB).is_some());

    // The second event is removed.
    assert_next_matches_with_timeout!(stream, VectorDiff::Remove { index: 2 });

    assert_pending!(stream);
}

#[async_test]
async fn test_initial_public_unthreaded_receipt() {
    let event_id = owned_event_id!("$event_with_receipt");

    // Add initial unthreaded public receipt.
    let mut initial_user_receipts = ReadReceiptMap::new();
    initial_user_receipts
        .entry(ReceiptType::Read)
        .or_default()
        .entry(ReceiptThread::Unthreaded)
        .or_default()
        .insert(
            ALICE.to_owned(),
            (event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(10)))),
        );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();

    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id);
}

#[async_test]
async fn test_initial_public_main_thread_receipt() {
    let event_id = owned_event_id!("$event_with_receipt");

    // Add initial public receipt on the main thread.
    let mut initial_user_receipts = ReadReceiptMap::new();
    initial_user_receipts
        .entry(ReceiptType::Read)
        .or_default()
        .entry(ReceiptThread::Main)
        .or_default()
        .insert(
            ALICE.to_owned(),
            (event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(10)))),
        );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();

    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id);
}

#[async_test]
async fn test_initial_private_unthreaded_receipt() {
    let event_id = owned_event_id!("$event_with_receipt");

    // Add initial unthreaded private receipt.
    let mut initial_user_receipts = ReadReceiptMap::new();
    initial_user_receipts
        .entry(ReceiptType::ReadPrivate)
        .or_default()
        .entry(ReceiptThread::Unthreaded)
        .or_default()
        .insert(
            ALICE.to_owned(),
            (event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(10)))),
        );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();

    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id);
}

#[async_test]
async fn test_initial_private_main_thread_receipt() {
    let event_id = owned_event_id!("$event_with_receipt");

    // Add initial private receipt on the main thread.
    let mut initial_user_receipts = ReadReceiptMap::new();
    initial_user_receipts
        .entry(ReceiptType::ReadPrivate)
        .or_default()
        .entry(ReceiptThread::Main)
        .or_default()
        .insert(
            ALICE.to_owned(),
            (event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(10)))),
        );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();

    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id);
}

#[async_test]
async fn test_clear_read_receipts() {
    let room_id = room_id!("!room:localhost");
    let event_a_id = event_id!("$event_a");
    let event_b_id = event_id!("$event_b");

    let timeline = TestTimelineBuilder::new()
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();
    let f = &timeline.factory;

    let event_a_content = RoomMessageEventContent::text_plain("A");
    timeline
        .handle_live_event(f.event(event_a_content.clone()).sender(*BOB).event_id(event_a_id))
        .await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 2);

    // Implicit read receipt of Bob.
    let event_a = items[1].as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // We received a limited timeline.
    timeline.controller.clear().await;

    // New message via sync.
    timeline.handle_live_event(f.text_msg("B").sender(*BOB).event_id(event_b_id)).await;

    // Old message via back-pagination.
    timeline
        .handle_back_paginated_event(
            f.event(event_a_content)
                .sender(*BOB)
                .room(room_id)
                .event_id(event_a_id)
                .into_raw_timeline(),
        )
        .await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 3);

    // Old implicit read receipt of Bob is gone.
    let event_a = items[1].as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 0);

    // New implicit read receipt of Bob.
    let event_b = items[2].as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 1);
    assert!(event_b.read_receipts().get(*BOB).is_some());
}

#[async_test]
async fn test_implicit_read_receipt_before_explicit_read_receipt() {
    // Test a timeline in this order:
    // 1. $alice_event: sent by alice, has no explicit read receipts.
    // 2. $bob_event: sent by bob, has no explicit read receipts.
    // 3. $carol_event: sent by carol, has the explicit read receipts of all users.
    let room_id = room_id!("!room:localhost");
    let alice_event_id = owned_event_id!("$alice_event");
    let bob_event_id = owned_event_id!("$bob_event");
    let carol_event_id = owned_event_id!("$carol_event");

    // Add initial unthreaded private receipt.
    let mut initial_user_receipts = ReadReceiptMap::new();
    let unthreaded_read_receipts = initial_user_receipts
        .entry(ReceiptType::Read)
        .or_default()
        .entry(ReceiptThread::Unthreaded)
        .or_default();
    unthreaded_read_receipts.insert(
        ALICE.to_owned(),
        (carol_event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(10)))),
    );
    unthreaded_read_receipts.insert(
        BOB.to_owned(),
        (carol_event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(5)))),
    );
    unthreaded_read_receipts.insert(
        CAROL.to_owned(),
        (carol_event_id.clone(), Receipt::new(ruma::MilliSecondsSinceUnixEpoch(uint!(1)))),
    );

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_initial_user_receipts(initial_user_receipts))
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();

    // Check that the receipts are at the correct place.
    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, carol_event_id);
    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(receipt_event_id, carol_event_id);
    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*CAROL).await.unwrap();
    assert_eq!(receipt_event_id, carol_event_id);

    // Add the events.
    timeline
        .handle_back_paginated_event(
            timeline
                .factory
                .text_msg("I am Carol!")
                .sender(*CAROL)
                .room(room_id)
                .event_id(&carol_event_id)
                .into_raw_timeline(),
        )
        .await;
    timeline
        .handle_back_paginated_event(
            timeline
                .factory
                .text_msg("I am Bob!")
                .sender(*BOB)
                .room(room_id)
                .event_id(&bob_event_id)
                .into_raw_timeline(),
        )
        .await;
    timeline
        .handle_back_paginated_event(
            timeline
                .factory
                .text_msg("I am Alice!")
                .sender(*ALICE)
                .room(room_id)
                .event_id(&alice_event_id)
                .into_raw_timeline(),
        )
        .await;

    // The receipts shouldn't have moved.
    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, carol_event_id);
    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(receipt_event_id, carol_event_id);
    let (receipt_event_id, _) = timeline.controller.latest_user_read_receipt(*CAROL).await.unwrap();
    assert_eq!(receipt_event_id, carol_event_id);
}

#[async_test]
async fn test_threaded_latest_user_read_receipt() {
    let thread_root = owned_event_id!("$thread_root");
    let receipt_thread = ReceiptThread::Thread(thread_root.clone());

    let timeline = TestTimelineBuilder::new()
        .focus(TimelineFocus::Thread { root_event_id: thread_root })
        .settings(TimelineSettings { track_read_receipts: true, ..Default::default() })
        .build();

    // Sanity check: no read receipts before any events.
    assert!(timeline.controller.latest_user_read_receipt(*ALICE).await.is_none());
    assert!(timeline.controller.latest_user_read_receipt(*BOB).await.is_none());

    // Add some sync events.
    let f = &timeline.factory;

    timeline
        .handle_live_event(f.text_msg("hi I'm Bob.").sender(*ALICE).event_id(event_id!("$1")))
        .await;

    timeline
        .handle_live_event(f.text_msg("hi Alice, I'm Bob.").sender(*BOB).event_id(event_id!("$2")))
        .await;

    // Implicit receipts are taken into account.
    let (receipt_event_id, receipt) =
        timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id!("$1"));
    assert_eq!(receipt.thread, receipt_thread);

    let (receipt_event_id, receipt) =
        timeline.controller.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(receipt_event_id, event_id!("$2"));
    assert_eq!(receipt.thread, receipt_thread);

    timeline
        .handle_live_event(f.text_msg("nice to meet you!").sender(*ALICE).event_id(event_id!("$3")))
        .await;

    // Alice's latest read receipt is updated.
    let (receipt_event_id, receipt) =
        timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id!("$3"));
    assert_eq!(receipt.thread, receipt_thread);

    // But Bob's isn't.
    let (receipt_event_id, receipt) =
        timeline.controller.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(receipt_event_id, event_id!("$2"));
    assert_eq!(receipt.thread, receipt_thread);

    // Bob sees Alice's message.
    timeline
        .handle_read_receipts([(
            owned_event_id!("$3"),
            ReceiptType::Read,
            BOB.to_owned(),
            receipt_thread.clone(),
        )])
        .await;

    // Alice's latest read receipt is at the same position.
    let (receipt_event_id, receipt) =
        timeline.controller.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(receipt_event_id, event_id!("$3"));
    assert_eq!(receipt.thread, receipt_thread);

    // But Bob's has moved!
    let (receipt_event_id, receipt) =
        timeline.controller.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(receipt_event_id, event_id!("$3"));
    assert_eq!(receipt.thread, receipt_thread);
}
