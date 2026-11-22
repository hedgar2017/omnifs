#![cfg(not(target_os = "wasi"))]

//! The requests of the mount `slack` to Slack: the full listings of the users
//! and of the channels, the history of each channel, and the messages, the
//! replies, and the content of the files of their own feeds.

mod support;

use std::time::Duration;

use omnifs_engine::NsError;
use omnifs_itest::RuntimeHarness;
use omnifs_provider::Artifact;
use serde_json::Value;
use support::{
    answering, blob, channel, elsewhere, expect_channels, expect_file, expect_history,
    expect_message, expect_new, expect_replies, expect_users, failure, file, hidden, lookup,
    message, names, names_at, network, ok, page, rate_limited, read, reply, served, slack_harness,
    thread, ts, unedited, user, with_files,
};

/// The `ts` of the fixtures `ns`, sorted.
fn stamps(ns: &[u32]) -> Vec<String> {
    let mut stamps = ns.iter().map(|n| ts(*n)).collect::<Vec<_>>();
    stamps.sort();
    stamps
}

/// The first full listing of the channels of one page with `channels`.
async fn channels(harness: &RuntimeHarness, channels: &[Value]) {
    let (listing, requests) = answering(
        harness,
        names(harness, "/slack/channels"),
        vec![ok(&page("channels", channels, None))],
    )
    .await;
    listing.unwrap();
    expect_channels(&requests[0], None);
}

/// The first full listing of the history of `channel` of one page with
/// `messages`.
async fn first_full_listing(harness: &RuntimeHarness, channel: &str, messages: &[Value]) {
    let (listing, requests) = answering(
        harness,
        names(harness, &format!("/slack/channels/{channel}/messages")),
        vec![ok(&page("messages", messages, None))],
    )
    .await;
    listing.unwrap();
    expect_history(&requests[0], channel, None);
}

/// The check of the token of a mount at its creation, which the provider
/// declares: a `POST` of `auth.test` whose answer `200` has a `bot_id`.
#[test]
fn the_token_of_a_mount_is_that_of_a_bot_by_auth_test() {
    let provider = "omnifs_provider_slack.wasm";
    let bytes = std::fs::read(omnifs_itest::provider_wasm_path(provider)).unwrap();
    let (_, manifest) = Artifact::from_bytes_with_manifest(provider, bytes).unwrap();
    let manifest = serde_json::to_value(manifest).unwrap();
    let check = &manifest["auth"]["schemes"][0]["staticToken"]["validation"];
    assert_eq!(check["method"], "POST");
    assert_eq!(check["url"], "https://slack.com/api/auth.test");
    assert_eq!(check["expectStatus"], 200);
    assert_eq!(check["jsonPointer"], "/bot_id");
}

/// The first full listing of the users at the first read of `users/`, by
/// `next_cursor`, each page at its answer: a `429` in it gives the read the
/// error of the stop while the users of the pages before it are served, and
/// the next read continues from the next `cursor`; `name` of the `real_name`
/// of a user or nothing; past the deadline no request for a read of a user
/// that the source filesystem has, a full listing for `users/` and one for a
/// user that it lacks, which removes a user that the listing lacks, and no
/// request for a user that it lacks within the deadline.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_users_are_the_full_listings_of_users_list() {
    let harness = slack_harness();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/users"),
        vec![
            ok(&page(
                "members",
                &[user("U01", Some("Ada Lovelace")), user("U02", None)],
                Some("cursor-2"),
            )),
            rate_limited("1"),
        ],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    expect_users(&requests[0], None);
    expect_users(&requests[1], Some("cursor-2"));
    assert_eq!(
        served(&harness, read(&harness, "/slack/users/U01/name"))
            .await
            .unwrap(),
        "Ada Lovelace"
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/users"),
        vec![ok(&page(
            "members",
            &[user("U03", Some("Grace Hopper"))],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["U01", "U02", "U03"]);
    expect_users(&requests[0], Some("cursor-2"));
    // An answer of `users.list` with its objects under another name than
    // `members` has none.
    harness.begin_sync();
    let (refused, requests) = answering(
        &harness,
        names(&harness, "/slack/users"),
        vec![ok(&page("channels", &[user("U01", None)], None))],
    )
    .await;
    assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
    expect_users(&requests[0], None);
    assert_eq!(
        served(&harness, names(&harness, "/slack/users/U01"))
            .await
            .unwrap(),
        ["name", "user.json"]
    );
    for (id, name) in [
        ("U01", "Ada Lovelace"),
        ("U02", ""),
        ("U03", "Grace Hopper"),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &format!("/slack/users/{id}/name")))
                .await
                .unwrap(),
            name,
            "{id}"
        );
    }
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, "/slack/users/U02/user.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, user("U02", None));

    harness.begin_sync();
    assert_eq!(
        served(&harness, read(&harness, "/slack/users/U01/name"))
            .await
            .unwrap(),
        "Ada Lovelace"
    );
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/users"),
        vec![ok(&page(
            "members",
            &[
                user("U01", Some("Ada King")),
                user("U02", None),
                user("U03", Some("Grace Hopper")),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["U01", "U02", "U03"]);
    expect_users(&requests[0], None);
    assert_eq!(
        served(&harness, read(&harness, "/slack/users/U01/name"))
            .await
            .unwrap(),
        "Ada King"
    );

    harness.begin_sync();
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "/slack/users/U04"),
        vec![ok(&page(
            "members",
            &[
                user("U01", Some("Ada King")),
                user("U03", Some("Grace Hopper")),
                user("U04", Some("Linus Torvalds")),
            ],
            None,
        ))],
    )
    .await;
    assert!(!found.unwrap().is_missing());
    expect_users(&requests[0], None);
    assert_eq!(
        served(&harness, names(&harness, "/slack/users"))
            .await
            .unwrap(),
        ["U01", "U03", "U04"]
    );
    assert!(
        served(&harness, lookup(&harness, "/slack/users/U05"))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The first full listing of the channels at the first read of `channels/`,
/// by `next_cursor`; past the deadline no request for a read of a channel
/// that the source filesystem has, a change of the attributes of `channels/`
/// at its lookup alone, a full listing for `channels/` and one
/// for a channel that it lacks, which removes a channel that the listing
/// lacks with its messages, and no request for a channel that it lacks
/// outside a sync.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_channels_are_the_full_listings_of_users_conversations() {
    let harness = slack_harness();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels"),
        vec![
            ok(&page(
                "channels",
                &[channel("C01", "general")],
                Some("cursor-2"),
            )),
            ok(&page("channels", &[channel("C02", "random")], None)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["C01", "C02"]);
    expect_channels(&requests[0], None);
    expect_channels(&requests[1], Some("cursor-2"));
    assert_eq!(
        served(&harness, names(&harness, "/slack/channels/C01"))
            .await
            .unwrap(),
        ["channel.json", "messages", "name"]
    );
    assert_eq!(
        served(&harness, read(&harness, "/slack/channels/C02/name"))
            .await
            .unwrap(),
        "random"
    );
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, "/slack/channels/C01/channel.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, channel("C01", "general"));
    first_full_listing(&harness, "C02", &[message(1, Some("U01"), "Hello")]).await;

    let folder = lookup(&harness, "/slack/channels").await.unwrap();
    assert_eq!(
        served(&harness, lookup(&harness, "/slack/channels"))
            .await
            .unwrap()
            .attrs()
            .unwrap()
            .change,
        folder.attrs().unwrap().change
    );
    harness.begin_sync();
    assert_eq!(
        served(&harness, read(&harness, "/slack/channels/C01/name"))
            .await
            .unwrap(),
        "general"
    );
    let due = served(&harness, lookup(&harness, "/slack/channels"))
        .await
        .unwrap();
    // The change of the folder moves with a change of its listing alone,
    // not with the sync.
    assert_eq!(due.attrs().unwrap().change, folder.attrs().unwrap().change);
    assert_eq!(
        served(&harness, lookup(&harness, "/slack/channels"))
            .await
            .unwrap()
            .attrs()
            .unwrap()
            .change,
        due.attrs().unwrap().change
    );
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels"),
        vec![ok(&page(
            "channels",
            &[channel("C01", "announcements"), channel("C02", "random")],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["C01", "C02"]);
    expect_channels(&requests[0], None);
    assert_eq!(
        served(&harness, read(&harness, "/slack/channels/C01/name"))
            .await
            .unwrap(),
        "announcements"
    );

    harness.begin_sync();
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "/slack/channels/C03"),
        vec![ok(&page(
            "channels",
            &[channel("C01", "announcements"), channel("C03", "design")],
            None,
        ))],
    )
    .await;
    assert!(!found.unwrap().is_missing());
    expect_channels(&requests[0], None);
    assert_eq!(
        served(&harness, names(&harness, "/slack/channels"))
            .await
            .unwrap(),
        ["C01", "C03"]
    );
    assert!(
        served(
            &harness,
            lookup(&harness, &format!("/slack/channels/C02/messages/{}", ts(1)))
        )
        .await
        .unwrap()
        .is_missing()
    );
    assert!(
        served(&harness, lookup(&harness, "/slack/channels/C04"))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The first full listing of a channel at the first read of `messages/`,
/// each page at its answer: a `429` in it gives each read of `messages/` the
/// error of the stop while the files of the messages that it took are
/// served, and the next read continues from the next `cursor`; it keeps a
/// newer message than its pages have, which a read took by its `ts`
/// meanwhile, and which the messages after the latest one of the sweep bring
/// again, since Slack gives no messages after a `ts` but one.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_first_full_listing_of_a_channel_takes_each_page_at_its_answer() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![
            ok(&page(
                "messages",
                &[
                    message(3, Some("U01"), "Third"),
                    message(2, Some("U02"), "Second"),
                ],
                Some("cursor-2"),
            )),
            rate_limited("1"),
        ],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    let listing = served(&harness, names(&harness, "/slack/channels/C01/messages")).await;
    assert!(
        matches!(listing, Err(NsError::RateLimited { .. })),
        "{listing:?}"
    );
    for (n, text) in [(3, "Third"), (2, "Second")] {
        assert_eq!(
            served(
                &harness,
                read(
                    &harness,
                    &format!("/slack/channels/C01/messages/{}/text", ts(n))
                )
            )
            .await
            .unwrap(),
            text
        );
    }

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (text, requests) = answering(
        &harness,
        read(
            &harness,
            &format!("/slack/channels/C01/messages/{}/text", ts(4)),
        ),
        vec![ok(&page(
            "messages",
            &[message(4, Some("U01"), "Fourth")],
            None,
        ))],
    )
    .await;
    assert_eq!(text.unwrap(), "Fourth");
    expect_message(&requests[0], "C01", &ts(4));
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![ok(&page("messages", &[message(1, None, "First")], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3, 4]));
    expect_history(&requests[0], "C01", Some("cursor-2"));
    let at = |n: u32| format!("/slack/channels/C01/messages/{}", ts(n));
    assert_eq!(
        served(&harness, read(&harness, &format!("{}/text", at(4))))
            .await
            .unwrap(),
        "Fourth"
    );
    harness.begin_sync();
    let (text, requests) = answering(
        &harness,
        read(&harness, &format!("{}/text", at(4))),
        vec![ok(&page(
            "messages",
            &[message(4, Some("U01"), "Fourth")],
            None,
        ))],
    )
    .await;
    assert_eq!(text.unwrap(), "Fourth");
    expect_new(&requests[0], "C01", &ts(3), None);
    assert_eq!(
        served(&harness, names(&harness, "/slack/channels/C01/messages"))
            .await
            .unwrap(),
        stamps(&[1, 2, 3, 4])
    );
    assert_eq!(
        served(&harness, names(&harness, &at(1))).await.unwrap(),
        ["author", "files", "message.json", "replies", "text"]
    );
    for (n, author) in [(1, ""), (2, "U02")] {
        assert_eq!(
            served(&harness, read(&harness, &format!("{}/author", at(n))))
                .await
                .unwrap(),
            author
        );
    }
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, &format!("{}/message.json", at(3))))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, message(3, Some("U01"), "Third"));
}

/// The changes that the answers of `conversations.history` tell, with no
/// other request: at a read of a file of a message past the deadline, the
/// messages after the latest one alone; at a read of `messages/` past the
/// deadline, a sweep of each page, which takes an edit of a message and the
/// removal of a message that its pages lack, writes `messages/` complete
/// after a last page without a new message, leaves the replies of a thread
/// whose `reply_count` and `latest_reply` are those of the replies of the
/// projection, which their next read takes from the projection, writes
/// `messages/` complete for each read within the deadline, and removes the
/// replies of a thread with another `reply_count` and `latest_reply`, which
/// their next read takes once more; and a read of `messages/` sweeps in place
/// of an unfinished answer of the messages after the latest one.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_sweep_takes_each_change_of_the_messages_of_its_pages() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let messages = "/slack/channels/C01/messages";
    let at = |n: u32| format!("{messages}/{}", ts(n));
    let question = |replies, latest| thread(message(2, Some("U02"), "Second"), replies, latest);
    let edited = || {
        let mut third = message(3, Some("U01"), "Third, edited");
        third["edited"] = serde_json::json!({ "user": "U01", "ts": ts(5) });
        third
    };
    let (listing, requests) = answering(
        &harness,
        names(&harness, messages),
        vec![
            ok(&page(
                "messages",
                &[message(3, Some("U01"), "Third"), question(1, 21)],
                Some("cursor-2"),
            )),
            ok(&page("messages", &[message(1, Some("U01"), "First")], None)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    let replies = format!("{}/replies", at(2));
    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![ok(&page(
            "messages",
            &[question(1, 21), reply(21, 2, "U02", "A reply")],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[21]));
    expect_replies(&requests[0], "C01", &ts(2), None);

    // The messages after the latest one.
    harness.begin_sync();
    let (text, requests) = answering(
        &harness,
        read(&harness, &format!("{}/text", at(3))),
        vec![ok(&page(
            "messages",
            &[message(4, Some("U02"), "Fourth")],
            None,
        ))],
    )
    .await;
    assert_eq!(text.unwrap(), "Third");
    expect_new(&requests[0], "C01", &ts(3), None);
    assert_eq!(
        served(&harness, names(&harness, messages)).await.unwrap(),
        stamps(&[1, 2, 3, 4])
    );

    // A sweep: an edit, a removal, and a thread as it was.
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, messages),
        vec![
            ok(&page(
                "messages",
                &[
                    message(5, Some("U01"), "Fifth"),
                    message(4, Some("U02"), "Fourth"),
                    edited(),
                    question(1, 21),
                ],
                Some("cursor-2"),
            )),
            ok(&page("messages", &[], None)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[2, 3, 4, 5]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    assert_eq!(
        served(&harness, read(&harness, &format!("{}/text", at(3))))
            .await
            .unwrap(),
        "Third, edited"
    );
    let json: Value = serde_json::from_str(
        &served(&harness, read(&harness, &format!("{}/message.json", at(3))))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json, edited());
    assert!(
        served(&harness, lookup(&harness, &at(1)))
            .await
            .unwrap()
            .is_missing()
    );
    assert_eq!(
        served(&harness, names(&harness, &replies)).await.unwrap(),
        stamps(&[21])
    );
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("{replies}/{}/text", ts(21)))
        )
        .await
        .unwrap(),
        "A reply"
    );
    // `messages/` is complete after a last page without a new message.
    assert_eq!(
        served(&harness, names(&harness, messages)).await.unwrap(),
        stamps(&[2, 3, 4, 5])
    );

    // A sweep with a thread of another `reply_count` and `latest_reply`.
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, messages),
        vec![ok(&page(
            "messages",
            &[
                message(5, Some("U01"), "Fifth"),
                message(4, Some("U02"), "Fourth"),
                edited(),
                question(2, 22),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[2, 3, 4, 5]));
    expect_history(&requests[0], "C01", None);
    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![ok(&page(
            "messages",
            &[
                question(2, 22),
                reply(21, 2, "U02", "A reply"),
                reply(22, 2, "U03", "Another reply"),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[21, 22]));
    expect_replies(&requests[0], "C01", &ts(2), None);

    // A read of `messages/` sweeps from the first page in place of an
    // unfinished answer of the new messages.
    harness.begin_sync();
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &format!("{}/text", at(4))),
        vec![
            ok(&page(
                "messages",
                &[message(6, Some("U01"), "Sixth")],
                Some("cursor-2"),
            )),
            network(),
        ],
    )
    .await;
    assert_eq!(stopped.unwrap(), "Fourth");
    expect_new(&requests[0], "C01", &ts(5), None);
    expect_new(&requests[1], "C01", &ts(5), Some("cursor-2"));
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, messages),
        vec![ok(&page(
            "messages",
            &[
                message(6, Some("U01"), "Sixth"),
                message(5, Some("U01"), "Fifth"),
                message(4, Some("U02"), "Fourth"),
                edited(),
                question(2, 22),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[2, 3, 4, 5, 6]));
    expect_history(&requests[0], "C01", None);
}

/// A message by its `ts` before the end of the first full listing of its
/// channel: for a message that the source filesystem lacks, and again 3
/// seconds or more after the answer with it, and none of it for an answer
/// without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_by_its_ts_before_the_end_of_the_first_full_listing() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let text = format!("/slack/channels/C01/messages/{}/text", ts(1));
    let (read_text, requests) = answering(
        &harness,
        read(&harness, &text),
        vec![ok(&page(
            "messages",
            &[message(1, Some("U01"), "First")],
            None,
        ))],
    )
    .await;
    assert_eq!(read_text.unwrap(), "First");
    expect_message(&requests[0], "C01", &ts(1));
    assert_eq!(
        served(&harness, read(&harness, &text)).await.unwrap(),
        "First"
    );

    harness.begin_sync();
    let (read_text, requests) = answering(
        &harness,
        read(&harness, &text),
        vec![ok(&page(
            "messages",
            &[message(1, Some("U01"), "First, edited")],
            None,
        ))],
    )
    .await;
    assert_eq!(read_text.unwrap(), "First, edited");
    expect_message(&requests[0], "C01", &ts(1));

    let gone = format!("/slack/channels/C01/messages/{}", ts(2));
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &gone),
        vec![ok(&page("messages", &[], None))],
    )
    .await;
    assert!(found.unwrap().is_missing());
    expect_message(&requests[0], "C01", &ts(2));
    assert!(
        served(&harness, lookup(&harness, &gone))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The replies by `next_cursor` at the first read of `replies/`, without the
/// message itself, whose files are those of `conversations.history`, and
/// without a reply of another thread, no
/// request for a reply that they lack or for a `reply_count` of `0`, the
/// removal of the message for `thread_not_found`, and, before the end of the
/// first full listing of its channel, the replies again for a read of
/// `replies/` by its path alone past the deadline of a message with another
/// `latest_reply`, without a reply that they lack, and no reply without a
/// request of them for a message without a `reply_count` then.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_replies_take_the_pages_of_conversations_replies() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![
            ok(&page(
                "messages",
                &[
                    thread(message(5, Some("U01"), "Gone"), 1, 6),
                    thread(message(4, Some("U02"), "Unanswered"), 0, 4),
                    thread(message(1, Some("U01"), "Question"), 2, 3),
                ],
                Some("cursor-2"),
            )),
            rate_limited("1"),
        ],
    )
    .await;
    assert!(
        matches!(stopped, Err(NsError::RateLimited { .. })),
        "{stopped:?}"
    );
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));

    // The replies are asked after the window of the stop.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let replies = |n: u32| format!("/slack/channels/C01/messages/{}/replies", ts(n));
    let asked = thread(message(1, Some("U01"), "Question, edited"), 2, 3);
    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies(1)),
        vec![
            ok(&page(
                "messages",
                &[asked.clone(), reply(2, 1, "U02", "An answer")],
                Some("cursor-2"),
            )),
            ok(&page(
                "messages",
                &[
                    reply(3, 1, "U01", "Thanks"),
                    reply(9, 8, "U02", "Of another thread"),
                ],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[2, 3]));
    expect_replies(&requests[0], "C01", &ts(1), None);
    expect_replies(&requests[1], "C01", &ts(1), Some("cursor-2"));
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("{}/{}/text", replies(1), ts(2)))
        )
        .await
        .unwrap(),
        "An answer"
    );
    // A reply that the complete `replies/` lacks asks Slack for nothing.
    assert!(
        served(
            &harness,
            lookup(&harness, &format!("{}/{}", replies(1), ts(8)))
        )
        .await
        .unwrap()
        .is_missing()
    );
    // The message itself of the replies is that of the history.
    let at = format!("/slack/channels/C01/messages/{}", ts(1));
    assert_eq!(
        served(&harness, read(&harness, &format!("{at}/text")))
            .await
            .unwrap(),
        "Question"
    );
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, &format!("{at}/message.json")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, thread(message(1, Some("U01"), "Question"), 2, 3));

    assert!(
        served(&harness, names(&harness, &replies(4)))
            .await
            .unwrap()
            .is_empty()
    );

    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies(5)),
        vec![failure("thread_not_found")],
    )
    .await;
    assert!(matches!(listing, Err(NsError::NotFound)), "{listing:?}");
    expect_replies(&requests[0], "C01", &ts(5), None);
    assert!(
        served(
            &harness,
            lookup(&harness, &format!("/slack/channels/C01/messages/{}", ts(5)))
        )
        .await
        .unwrap()
        .is_missing()
    );

    harness.begin_sync();
    let edited = thread(message(1, Some("U01"), "Question"), 2, 7);
    let (listing, requests) = answering(
        &harness,
        names_at(&harness, &replies(1)),
        vec![
            ok(&page("messages", std::slice::from_ref(&edited), None)),
            ok(&page(
                "messages",
                &[
                    edited,
                    reply(2, 1, "U02", "An answer"),
                    reply(7, 1, "U03", "Another answer"),
                ],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[2, 7]));
    expect_message(&requests[0], "C01", &ts(1));
    expect_replies(&requests[1], "C01", &ts(1), None);
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("{}/{}/text", replies(1), ts(7)))
        )
        .await
        .unwrap(),
        "Another answer"
    );

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names_at(&harness, &replies(1)),
        vec![ok(&page(
            "messages",
            &[message(1, Some("U01"), "Question")],
            None,
        ))],
    )
    .await;
    assert!(listing.unwrap().is_empty());
    expect_message(&requests[0], "C01", &ts(1));
    assert!(
        served(
            &harness,
            lookup(&harness, &format!("{}/{}", replies(1), ts(7)))
        )
        .await
        .unwrap()
        .is_missing()
    );
}

/// The content of a file at the first read of `content`, as a blob fetch with
/// the version of its `edit_timestamp`, and without one for a file without
/// it: the error of the status for an answer that is not `200`, which the
/// next read asks again, no request again for the same `edit_timestamp`, the
/// content again for another one, the content of a file of a reply, no
/// folder of a file without `url_private_download`, and none of a file that
/// its message has no more; no request for a `url_private_download` of
/// another host than `files.slack.com`; and a `429` without a `Retry-After`
/// fails the read of its file alone, without a wait of the host.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_content_of_a_file_is_the_blob_of_its_edit_timestamp() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    // A `message.json` of more than a range of a read of the projection.
    let long = "notes ".repeat(20_000);
    let notes = |files: &[Value]| thread(with_files(message(1, Some("U01"), &long), files), 1, 11);
    let first = notes(&[
        file("F01", "notes.txt", 1_700_000_000),
        hidden("F02"),
        unedited("F03", "plain.txt"),
    ]);
    let others = with_files(
        message(2, Some("U02"), "Others"),
        &[
            elsewhere("F05", "elsewhere.txt"),
            file("F06", "limited.txt", 1_700_000_000),
            file("F07", "waiting.txt", 1_700_000_000),
        ],
    );
    first_full_listing(&harness, "C01", &[others, first.clone()]).await;
    let others = format!("/slack/channels/C01/messages/{}/files", ts(2));
    let refused = served(&harness, read(&harness, &format!("{others}/F05/content"))).await;
    assert!(matches!(refused, Err(NsError::Invalid)), "{refused:?}");
    let (limited, requests) = answering(
        &harness,
        read(&harness, &format!("{others}/F06/content")),
        vec![blob(429, b"")],
    )
    .await;
    assert!(
        matches!(limited, Err(NsError::RateLimited { .. })),
        "{limited:?}"
    );
    expect_file(&requests[0], "F06", "limited.txt", Some("1700000000"));
    // A `429` without a `Retry-After` states no wait: the read of another
    // file asks for it.
    let (bytes, requests) = answering(
        &harness,
        read(&harness, &format!("{others}/F07/content")),
        vec![blob(200, b"waited")],
    )
    .await;
    assert_eq!(bytes.unwrap(), "waited");
    expect_file(&requests[0], "F07", "waiting.txt", Some("1700000000"));
    let files = format!("/slack/channels/C01/messages/{}/files", ts(1));
    assert_eq!(
        served(&harness, names(&harness, &files)).await.unwrap(),
        ["F01", "F03"]
    );
    assert_eq!(
        served(&harness, names(&harness, &format!("{files}/F01")))
            .await
            .unwrap(),
        ["content", "name"]
    );
    assert_eq!(
        served(&harness, read(&harness, &format!("{files}/F01/name")))
            .await
            .unwrap(),
        "notes.txt"
    );
    let content = format!("{files}/F01/content");
    let (refused, requests) = answering(
        &harness,
        read(&harness, &content),
        vec![blob(302, b"a redirect")],
    )
    .await;
    assert!(
        matches!(refused, Err(NsError::Internal { .. })),
        "{refused:?}"
    );
    expect_file(&requests[0], "F01", "notes.txt", Some("1700000000"));
    let (bytes, requests) = answering(
        &harness,
        read(&harness, &content),
        vec![blob(200, b"notes")],
    )
    .await;
    assert_eq!(bytes.unwrap(), "notes");
    expect_file(&requests[0], "F01", "notes.txt", Some("1700000000"));
    assert_eq!(
        served(&harness, read(&harness, &content)).await.unwrap(),
        "notes"
    );
    let (bytes, requests) = answering(
        &harness,
        read(&harness, &format!("{files}/F03/content")),
        vec![blob(200, b"plain")],
    )
    .await;
    assert_eq!(bytes.unwrap(), "plain");
    expect_file(&requests[0], "F03", "plain.txt", None);

    let of_reply = format!(
        "/slack/channels/C01/messages/{}/replies/{}/files/F04/content",
        ts(1),
        ts(11)
    );
    let (bytes, requests) = answering(
        &harness,
        read(&harness, &of_reply),
        vec![
            ok(&page(
                "messages",
                &[
                    first,
                    with_files(
                        reply(11, 1, "U02", "A reply"),
                        &[file("F04", "reply.txt", 1_700_000_050)],
                    ),
                ],
                None,
            )),
            blob(200, b"of a reply"),
        ],
    )
    .await;
    assert_eq!(bytes.unwrap(), "of a reply");
    expect_replies(&requests[0], "C01", &ts(1), None);
    expect_file(&requests[1], "F04", "reply.txt", Some("1700000050"));

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![ok(&page(
            "messages",
            &[notes(&[file("F01", "notes.txt", 1_700_000_100)])],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1]));
    expect_history(&requests[0], "C01", None);
    assert_eq!(
        served(&harness, names(&harness, &files)).await.unwrap(),
        ["F01"]
    );
    let (bytes, requests) = answering(
        &harness,
        read(&harness, &content),
        vec![blob(200, b"notes, edited")],
    )
    .await;
    assert_eq!(bytes.unwrap(), "notes, edited");
    expect_file(&requests[0], "F01", "notes.txt", Some("1700000100"));
}

/// The first request again after `invalid_cursor` to a request with a
/// `cursor`: of a full listing of the users, of the channels, of the first
/// sweep of a channel, of the replies of a message, of the messages after the
/// latest one, and of a later sweep of each page, each after the window
/// of its timeout, which is the error of its read, or with its read served
/// what the projection has after the end of the first sweep of the channel.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn invalid_cursor_begins_the_requests_again_from_the_first_one() {
    let harness = slack_harness();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/users"),
        vec![
            ok(&page("members", &[user("U01", None)], Some("cursor-2"))),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Timeout)), "{stopped:?}");
    expect_users(&requests[0], None);
    expect_users(&requests[1], Some("cursor-2"));
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/users"),
        vec![ok(&page("members", &[user("U01", None)], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["U01"]);
    expect_users(&requests[0], None);

    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/channels"),
        vec![
            ok(&page(
                "channels",
                &[channel("C01", "general")],
                Some("cursor-2"),
            )),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Timeout)), "{stopped:?}");
    expect_channels(&requests[0], None);
    expect_channels(&requests[1], Some("cursor-2"));
    harness.begin_sync();
    channels(&harness, &[channel("C01", "general")]).await;

    let question = || thread(message(1, Some("U01"), "Question"), 1, 11);
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![
            ok(&page("messages", &[question()], Some("cursor-2"))),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Timeout)), "{stopped:?}");
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![ok(&page(
            "messages",
            &[message(2, Some("U02"), "Second"), question()],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2]));
    expect_history(&requests[0], "C01", None);

    let replies = format!("/slack/channels/C01/messages/{}/replies", ts(1));
    let (stopped, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![
            ok(&page("messages", &[question()], Some("cursor-2"))),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Timeout)), "{stopped:?}");
    expect_replies(&requests[0], "C01", &ts(1), None);
    expect_replies(&requests[1], "C01", &ts(1), Some("cursor-2"));
    harness.begin_sync();
    // The channel is past its deadline after the window of the stop: its
    // new messages come before the replies.
    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![
            ok(&page("messages", &[], None)),
            ok(&page(
                "messages",
                &[question(), reply(11, 1, "U02", "An answer")],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[11]));
    expect_new(&requests[0], "C01", &ts(2), None);
    expect_replies(&requests[1], "C01", &ts(1), None);

    let history = || {
        ok(&page(
            "messages",
            &[
                message(3, Some("U01"), "Third"),
                message(2, Some("U02"), "Second"),
                question(),
            ],
            None,
        ))
    };
    let stop = || {
        vec![
            ok(&page(
                "messages",
                &[message(3, Some("U01"), "Third")],
                Some("cursor-2"),
            )),
            failure("invalid_cursor"),
        ]
    };
    harness.begin_sync();
    let text = format!("/slack/channels/C01/messages/{}/text", ts(2));
    let (stopped, requests) = answering(&harness, read(&harness, &text), stop()).await;
    // The read of the new messages that stopped is served what the
    // projection has.
    assert_eq!(stopped.unwrap(), "Second");
    expect_new(&requests[0], "C01", &ts(2), None);
    expect_new(&requests[1], "C01", &ts(2), Some("cursor-2"));
    harness.begin_sync();
    let (read_text, requests) = answering(
        &harness,
        read(&harness, &text),
        vec![ok(&page(
            "messages",
            &[message(3, Some("U01"), "Third")],
            None,
        ))],
    )
    .await;
    assert_eq!(read_text.unwrap(), "Second");
    expect_new(&requests[0], "C01", &ts(2), None);

    harness.begin_sync();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        stop(),
    )
    .await;
    assert_eq!(stopped.unwrap(), stamps(&[1, 2, 3]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C01/messages"),
        vec![history()],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3]));
    expect_history(&requests[0], "C01", None);
}

/// The removal of a channel for `channel_not_found` and for
/// `not_in_channel`, of the history, of a message by its `ts`, and of the
/// replies of a message, the error of its kind for each other `error` of
/// `ok` of `false`, and the wait of `Retry-After` after `429`.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_errors_of_slack_remove_fail_and_wait() {
    let harness = slack_harness();
    channels(
        &harness,
        &[
            channel("C01", "general"),
            channel("C02", "random"),
            channel("C03", "design"),
            channel("C04", "left"),
            channel("C05", "closed"),
        ],
    )
    .await;
    for (id, error) in [("C01", "channel_not_found"), ("C02", "not_in_channel")] {
        let (listing, requests) = answering(
            &harness,
            names(&harness, &format!("/slack/channels/{id}/messages")),
            vec![failure(error)],
        )
        .await;
        assert!(
            matches!(listing, Err(NsError::NotFound)),
            "{error}: {listing:?}"
        );
        expect_history(&requests[0], id, None);
        assert!(
            served(&harness, lookup(&harness, &format!("/slack/channels/{id}")))
                .await
                .unwrap()
                .is_missing(),
            "{id}"
        );
    }
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &format!("/slack/channels/C04/messages/{}", ts(1))),
        vec![failure("not_in_channel")],
    )
    .await;
    assert!(found.unwrap().is_missing());
    expect_message(&requests[0], "C04", &ts(1));
    let question = thread(message(1, Some("U01"), "Question"), 1, 11);
    let (listing, requests) = answering(
        &harness,
        names(
            &harness,
            &format!("/slack/channels/C05/messages/{}/replies", ts(1)),
        ),
        vec![
            ok(&page("messages", &[question], None)),
            failure("channel_not_found"),
        ],
    )
    .await;
    assert!(matches!(listing, Err(NsError::NotFound)), "{listing:?}");
    expect_message(&requests[0], "C05", &ts(1));
    expect_replies(&requests[1], "C05", &ts(1), None);
    assert_eq!(
        served(&harness, names(&harness, "/slack/channels"))
            .await
            .unwrap(),
        ["C03"]
    );

    for (error, kind) in [
        ("missing_scope", "permission denied"),
        ("invalid_auth", "mount credential is unavailable"),
        ("internal_error", "network error"),
        ("request_timeout", "timeout"),
        ("invalid_arguments", "invalid argument"),
        ("an_error_without_a_kind", "internal error"),
    ] {
        let (listing, requests) = answering(
            &harness,
            names(&harness, "/slack/channels/C03/messages"),
            vec![failure(error)],
        )
        .await;
        let refused = listing.unwrap_err().to_string();
        assert!(refused.starts_with(kind), "{error}: {refused}");
        expect_history(&requests[0], "C03", None);
        // The next request waits for the end of the window of a failure for
        // now.
        if matches!(kind, "network error" | "timeout") {
            harness.begin_sync();
        }
    }

    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C03/messages"),
        vec![rate_limited("1")],
    )
    .await;
    assert!(
        matches!(listing, Err(NsError::RateLimited { .. })),
        "{listing:?}"
    );
    expect_history(&requests[0], "C03", None);
    let listing = served(&harness, names(&harness, "/slack/channels/C03/messages")).await;
    assert!(
        matches!(listing, Err(NsError::RateLimited { .. })),
        "{listing:?}"
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C03/messages"),
        vec![ok(&page("messages", &[], None))],
    )
    .await;
    assert!(listing.unwrap().is_empty());
    expect_history(&requests[0], "C03", None);
}

/// A sweep of each page from its first one, without the kept pages of its
/// answer, for a channel whose messages the projection lost: one that left
/// the listing of the channels and came back after an unfinished first
/// sweep, after an unfinished answer of the messages after the latest one,
/// whose kept page had a message that Slack has no more, and without a
/// message, which has `messages/` complete and empty again; and one that the
/// feed of a message removed during its first sweep, which a read of a
/// message by its `ts` after its return does not list.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_channel_that_left_and_came_back_takes_each_page_again() {
    let harness = slack_harness();
    let both = [channel("C01", "general"), channel("C02", "quiet")];
    channels(&harness, &both).await;
    let pages = |newest: &[u32], older: &[u32]| {
        let of = |ns: &[u32]| {
            ns.iter()
                .map(|n| message(*n, Some("U01"), "A message"))
                .collect::<Vec<_>>()
        };
        vec![
            ok(&page("messages", &of(newest), Some("cursor-2"))),
            ok(&page("messages", &of(older), None)),
        ]
    };
    // The channels leave at a full listing of the channels and come back at
    // the next one.
    let leave_and_come_back = || async {
        harness.begin_sync();
        let (listing, _) = answering(
            &harness,
            names(&harness, "/slack/channels"),
            vec![ok(&page("channels", &[], None))],
        )
        .await;
        assert!(listing.unwrap().is_empty());
        harness.begin_sync();
        channels(&harness, &both).await;
    };
    let messages = "/slack/channels/C01/messages";
    first_full_listing(&harness, "C02", &[]).await;

    let (stopped, requests) = answering(
        &harness,
        names(&harness, messages),
        vec![
            ok(&page(
                "messages",
                &[
                    message(4, Some("U01"), "A message"),
                    message(3, Some("U01"), "A message"),
                ],
                Some("cursor-2"),
            )),
            network(),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect_history(&requests[1], "C01", Some("cursor-2"));
    leave_and_come_back().await;
    let (listing, requests) =
        answering(&harness, names(&harness, messages), pages(&[4, 3], &[2, 1])).await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3, 4]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels/C02/messages"),
        vec![ok(&page("messages", &[], None))],
    )
    .await;
    assert!(listing.unwrap().is_empty());
    expect_history(&requests[0], "C02", None);
    assert!(
        served(&harness, names(&harness, "/slack/channels/C02/messages"))
            .await
            .unwrap()
            .is_empty()
    );

    // The kept page of the new messages has message 6, which Slack has no
    // more when the channel comes back.
    harness.begin_sync();
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &format!("{messages}/{}/text", ts(1))),
        vec![
            ok(&page(
                "messages",
                &[message(6, Some("U01"), "A message")],
                Some("cursor-2"),
            )),
            network(),
        ],
    )
    .await;
    // The read of the failure is served what the projection has.
    assert_eq!(stopped.unwrap(), "A message");
    expect_new(&requests[0], "C01", &ts(4), None);
    expect_new(&requests[1], "C01", &ts(4), Some("cursor-2"));
    leave_and_come_back().await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, messages),
        pages(&[5, 4], &[3, 2, 1]),
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3, 4, 5]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    assert!(
        served(&harness, lookup(&harness, &format!("{messages}/{}", ts(6))))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The removal of a channel by the feed of a message during the first sweep
/// of the channel, after whose return a read of a message that the removal
/// took asks Slack for nothing within the deadline, and a read of
/// `messages/` sweeps each page from the first one.
#[tokio::test(flavor = "multi_thread")]
async fn a_channel_that_the_feed_of_a_message_removed_takes_each_page_again() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let messages = "/slack/channels/C01/messages";
    let of = |ns: &[u32]| {
        ns.iter()
            .map(|n| message(*n, Some("U01"), "A message"))
            .collect::<Vec<_>>()
    };
    let (stopped, _) = answering(
        &harness,
        names(&harness, messages),
        vec![
            ok(&page("messages", &of(&[4, 3]), Some("cursor-2"))),
            network(),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    harness.begin_sync();
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &format!("{messages}/{}", ts(5))),
        vec![failure("channel_not_found")],
    )
    .await;
    assert!(found.unwrap().is_missing());
    expect_message(&requests[0], "C01", &ts(5));
    harness.begin_sync();
    channels(&harness, &[channel("C01", "general")]).await;
    let text = served(
        &harness,
        read(&harness, &format!("{messages}/{}/text", ts(3))),
    )
    .await;
    assert!(matches!(text, Err(NsError::NotFound)), "{text:?}");
    let (listing, requests) = answering(
        &harness,
        names(&harness, messages),
        vec![
            ok(&page("messages", &of(&[4, 3]), Some("cursor-2"))),
            ok(&page("messages", &of(&[2, 1]), None)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3, 4]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
}

/// A walk of the mount, each listing and each file, and a second walk past
/// the deadline with nothing changed in Slack: the second asks for the
/// channels and for each page of the history alone, with no request of the
/// replies of a thread and no request of the bytes of a file, of a message
/// or of a reply, with an `edit_timestamp` or without one.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_second_walk_with_nothing_changed_asks_for_no_replies_and_no_file() {
    let harness = slack_harness();
    let messages = "/slack/channels/C01/messages";
    let at = format!("{messages}/{}", ts(1));
    let parent = || {
        thread(
            with_files(
                message(1, Some("U01"), "Notes"),
                &[
                    file("F01", "notes.txt", 1_700_000_000),
                    unedited("F02", "plain.txt"),
                ],
            ),
            1,
            11,
        )
    };
    let history = || {
        vec![
            ok(&page(
                "messages",
                &[message(2, Some("U02"), "Second")],
                Some("cursor-2"),
            )),
            ok(&page("messages", &[parent()], None)),
        ]
    };
    let answer = || {
        with_files(
            reply(11, 1, "U02", "A reply"),
            &[unedited("F03", "reply.txt")],
        )
    };
    let reply_file = format!("{at}/replies/{}/files/F03/content", ts(11));
    // Each file that a walk reads.
    let walk = || async {
        for path in [
            format!("{at}/text"),
            format!("{at}/files/F01/content"),
            format!("{at}/files/F02/content"),
            format!("{at}/replies/{}/text", ts(11)),
            reply_file.clone(),
        ] {
            read(&harness, &path).await.unwrap();
        }
    };

    channels(&harness, &[channel("C01", "general")]).await;
    let (listing, _) = answering(&harness, names(&harness, messages), history()).await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2]));
    let (listing, _) = answering(
        &harness,
        names(&harness, &format!("{at}/replies")),
        vec![ok(&page("messages", &[parent(), answer()], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[11]));
    let ((), requests) = answering(
        &harness,
        walk(),
        vec![
            blob(200, b"notes"),
            blob(200, b"plain"),
            blob(200, b"of a reply"),
        ],
    )
    .await;
    expect_file(&requests[0], "F01", "notes.txt", Some("1700000000"));
    expect_file(&requests[1], "F02", "plain.txt", None);

    harness.begin_sync();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "/slack/channels"),
        vec![ok(&page("channels", &[channel("C01", "general")], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["C01"]);
    expect_channels(&requests[0], None);
    let (listing, requests) = answering(&harness, names(&harness, messages), history()).await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2]));
    expect_history(&requests[0], "C01", None);
    expect_history(&requests[1], "C01", Some("cursor-2"));
    assert_eq!(
        served(&harness, names(&harness, &format!("{at}/replies")))
            .await
            .unwrap(),
        stamps(&[11])
    );
    served(&harness, walk()).await;
    assert_eq!(
        served(&harness, read(&harness, &reply_file)).await.unwrap(),
        "of a reply"
    );
}

/// The replies of a thread with more than a page of them, each page written
/// at its answer: a read after a failure of the next page has the replies of
/// the pages before it.
#[tokio::test(flavor = "multi_thread")]
async fn each_page_of_the_replies_is_written_at_its_answer() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let messages = "/slack/channels/C01/messages";
    first_full_listing(&harness, "C01", &[message(1, Some("U01"), "First")]).await;
    // A message of the messages after the latest one, whose feed has no
    // state yet.
    harness.begin_sync();
    let question = thread(message(2, Some("U01"), "Question"), 2, 22);
    let (text, requests) = answering(
        &harness,
        read(&harness, &format!("{messages}/{}/text", ts(1))),
        vec![ok(&page("messages", std::slice::from_ref(&question), None))],
    )
    .await;
    assert_eq!(text.unwrap(), "First");
    expect_new(&requests[0], "C01", &ts(1), None);
    let replies = format!("{messages}/{}/replies", ts(2));
    let (stopped, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![
            ok(&page(
                "messages",
                &[question, reply(21, 2, "U02", "An answer")],
                Some("cursor-2"),
            )),
            network(),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect_replies(&requests[0], "C01", &ts(2), None);
    expect_replies(&requests[1], "C01", &ts(2), Some("cursor-2"));
    assert_eq!(
        served(
            &harness,
            read(&harness, &format!("{replies}/{}/text", ts(21)))
        )
        .await
        .unwrap(),
        "An answer"
    );
}
