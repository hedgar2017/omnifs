#![cfg(not(target_os = "wasi"))]

//! The requests of the mount `slack` to Slack: the full listings of the users
//! and of the channels, the history of each channel, and the messages, the
//! replies, and the content of the files of their own feeds.

mod support;

use std::time::Duration;

use omnifs_engine::NsError;
use omnifs_itest::RuntimeHarness;
use serde_json::Value;
use support::{
    PAST_DEADLINE, answering, blob, channel, expect_channels, expect_file, expect_history,
    expect_message, expect_replies, expect_users, failure, file, hidden, lookup, message, names,
    names_at, network, ok, page, rate_limited, read, reply, served, slack_harness, thread, ts,
    user, with_files,
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
        names(harness, "channels"),
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
        names(harness, &format!("channels/{channel}/messages")),
        vec![ok(&page("messages", messages, None))],
    )
    .await;
    listing.unwrap();
    expect_history(&requests[0], channel, None, None);
}

/// The first full listing of the users at the first read of `users/`, by
/// `next_cursor`, with the users after its last page and `name` of the
/// `real_name` of a user or nothing; past the deadline no request for a read
/// of a user that the source filesystem has, a full listing for `users/` and
/// one for a user that it lacks, which removes a user that the listing lacks,
/// and no request for a user that it lacks within the deadline.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_users_are_the_full_listings_of_users_list() {
    let harness = slack_harness();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "users"),
        vec![
            ok(&page(
                "members",
                &[user("U01", Some("Ada Lovelace")), user("U02", None)],
                Some("cursor-2"),
            )),
            ok(&page("members", &[user("U03", Some("Grace Hopper"))], None)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["U01", "U02", "U03"]);
    expect_users(&requests[0], None);
    expect_users(&requests[1], Some("cursor-2"));
    assert_eq!(
        served(&harness, names(&harness, "users/U01"))
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
            served(&harness, read(&harness, &format!("users/{id}/name")))
                .await
                .unwrap(),
            name,
            "{id}"
        );
    }
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, "users/U02/user.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, user("U02", None));

    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        served(&harness, read(&harness, "users/U01/name"))
            .await
            .unwrap(),
        "Ada Lovelace"
    );
    let (listing, requests) = answering(
        &harness,
        names(&harness, "users"),
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
        served(&harness, read(&harness, "users/U01/name"))
            .await
            .unwrap(),
        "Ada King"
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "users/U04"),
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
        served(&harness, names(&harness, "users")).await.unwrap(),
        ["U01", "U03", "U04"]
    );
    assert!(
        served(&harness, lookup(&harness, "users/U05"))
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
/// within the deadline.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_channels_are_the_full_listings_of_users_conversations() {
    let harness = slack_harness();
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels"),
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
        served(&harness, names(&harness, "channels/C01"))
            .await
            .unwrap(),
        ["channel.json", "messages", "name"]
    );
    assert_eq!(
        served(&harness, read(&harness, "channels/C02/name"))
            .await
            .unwrap(),
        "random"
    );
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, "channels/C01/channel.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, channel("C01", "general"));
    first_full_listing(&harness, "C02", &[message(1, Some("U01"), "Hello")]).await;

    let folder = lookup(&harness, "channels").await.unwrap();
    assert_eq!(
        served(&harness, lookup(&harness, "channels"))
            .await
            .unwrap()
            .attrs()
            .unwrap()
            .change,
        folder.attrs().unwrap().change
    );
    tokio::time::sleep(PAST_DEADLINE).await;
    assert_eq!(
        served(&harness, read(&harness, "channels/C01/name"))
            .await
            .unwrap(),
        "general"
    );
    assert_ne!(
        served(&harness, lookup(&harness, "channels"))
            .await
            .unwrap()
            .attrs()
            .unwrap()
            .change,
        folder.attrs().unwrap().change
    );
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels"),
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
        served(&harness, read(&harness, "channels/C01/name"))
            .await
            .unwrap(),
        "announcements"
    );

    tokio::time::sleep(PAST_DEADLINE).await;
    let (found, requests) = answering(
        &harness,
        lookup(&harness, "channels/C03"),
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
        served(&harness, names(&harness, "channels")).await.unwrap(),
        ["C01", "C03"]
    );
    assert!(
        served(
            &harness,
            lookup(&harness, &format!("channels/C02/messages/{}", ts(1)))
        )
        .await
        .unwrap()
        .is_missing()
    );
    assert!(
        served(&harness, lookup(&harness, "channels/C04"))
            .await
            .unwrap()
            .is_missing()
    );
}

/// The first full listing of a channel at the first read of `messages/`,
/// each page at its answer: a `429` in it gives each read of `messages/` the
/// error of the stop while the files of the messages that it took are
/// served, and the next read continues from the next `cursor`.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_full_listing_of_a_channel_takes_each_page_at_its_answer() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
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
    expect_history(&requests[0], "C01", None, None);
    expect_history(&requests[1], "C01", None, Some("cursor-2"));
    let listing = served(&harness, names(&harness, "channels/C01/messages")).await;
    assert!(
        matches!(listing, Err(NsError::RateLimited { .. })),
        "{listing:?}"
    );
    for (n, text) in [(3, "Third"), (2, "Second")] {
        assert_eq!(
            served(
                &harness,
                read(&harness, &format!("channels/C01/messages/{}/text", ts(n)))
            )
            .await
            .unwrap(),
            text
        );
    }

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
        vec![ok(&page("messages", &[message(1, None, "First")], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2, 3]));
    expect_history(&requests[0], "C01", None, Some("cursor-2"));
    let at = |n: u32| format!("channels/C01/messages/{}", ts(n));
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

/// A full listing again at a read of `messages/` past the deadline, after its
/// last page, with the removal of a message with its replies; a full listing
/// for a channel without a message; and the new messages after the latest
/// `ts` by `next_cursor` at a read of a file of a message past the deadline.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_full_listing_again_removes_what_it_lacks_and_the_new_messages_follow_the_latest() {
    let harness = slack_harness();
    channels(
        &harness,
        &[channel("C01", "general"), channel("C02", "quiet")],
    )
    .await;
    first_full_listing(
        &harness,
        "C01",
        &[
            message(2, Some("U02"), "Second"),
            thread(message(1, Some("U01"), "First"), 1, 11),
        ],
    )
    .await;
    first_full_listing(&harness, "C02", &[]).await;
    let replies = format!("channels/C01/messages/{}/replies", ts(1));
    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![ok(&page(
            "messages",
            &[
                thread(message(1, Some("U01"), "First"), 1, 11),
                reply(11, 1, "U02", "A reply"),
            ],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[11]));
    expect_replies(&requests[0], "C01", &ts(1), None);

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
        vec![
            ok(&page(
                "messages",
                &[message(3, Some("U01"), "Third")],
                Some("cursor-2"),
            )),
            ok(&page(
                "messages",
                &[message(2, Some("U02"), "Second")],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[2, 3]));
    expect_history(&requests[0], "C01", None, None);
    expect_history(&requests[1], "C01", None, Some("cursor-2"));
    assert!(
        served(&harness, lookup(&harness, &format!("{replies}/{}", ts(11))))
            .await
            .unwrap()
            .is_missing()
    );
    // The lookup of a message of a channel without a message past the
    // deadline takes a full listing.
    let (found, requests) = answering(
        &harness,
        lookup(&harness, &format!("channels/C02/messages/{}", ts(9))),
        vec![ok(&page("messages", &[], None))],
    )
    .await;
    assert!(found.unwrap().is_missing());
    expect_history(&requests[0], "C02", None, None);

    tokio::time::sleep(PAST_DEADLINE).await;
    let (text, requests) = answering(
        &harness,
        read(&harness, &format!("channels/C01/messages/{}/text", ts(2))),
        vec![
            ok(&page(
                "messages",
                &[message(5, Some("U01"), "Fifth")],
                Some("cursor-2"),
            )),
            ok(&page(
                "messages",
                &[message(4, Some("U02"), "Fourth")],
                None,
            )),
        ],
    )
    .await;
    assert_eq!(text.unwrap(), "Second");
    expect_history(&requests[0], "C01", Some(ts(3).as_str()), None);
    expect_history(&requests[1], "C01", Some(ts(3).as_str()), Some("cursor-2"));
    assert_eq!(
        served(&harness, names(&harness, "channels/C01/messages"))
            .await
            .unwrap(),
        stamps(&[2, 3, 4, 5])
    );

    // The next new messages are those after the latest of these.
    tokio::time::sleep(PAST_DEADLINE).await;
    let (text, requests) = answering(
        &harness,
        read(&harness, &format!("channels/C01/messages/{}/text", ts(4))),
        vec![ok(&page("messages", &[], None))],
    )
    .await;
    assert_eq!(text.unwrap(), "Fourth");
    expect_history(&requests[0], "C01", Some(ts(5).as_str()), None);
}

/// A message by its `ts` before the end of the first full listing of its
/// channel: for a message that the source filesystem lacks, and again 3
/// seconds or more after the answer with it, and none of it for an answer
/// without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_by_its_ts_before_the_end_of_the_first_full_listing() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let text = format!("channels/C01/messages/{}/text", ts(1));
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

    tokio::time::sleep(PAST_DEADLINE).await;
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

    let gone = format!("channels/C01/messages/{}", ts(2));
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

/// The replies by `next_cursor` at the first read of `replies/`, with the
/// message itself as the message and without a reply of another thread, no
/// request for a reply that they lack or for a `reply_count` of `0`, the
/// removal of the message for `thread_not_found`, and, before the end of the
/// first full listing of its channel, the replies again for a read of
/// `replies/` by its path alone past the deadline of a message with another
/// `latest_reply`, without a reply that they lack.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn the_replies_take_the_pages_of_conversations_replies() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
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
            network(),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Network)), "{stopped:?}");
    expect_history(&requests[0], "C01", None, None);
    expect_history(&requests[1], "C01", None, Some("cursor-2"));

    let replies = |n: u32| format!("channels/C01/messages/{}/replies", ts(n));
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
    // The message itself of the replies is the message of the last answer
    // with it.
    let at = format!("channels/C01/messages/{}", ts(1));
    assert_eq!(
        served(&harness, read(&harness, &format!("{at}/text")))
            .await
            .unwrap(),
        "Question, edited"
    );
    let object: Value = serde_json::from_str(
        &served(&harness, read(&harness, &format!("{at}/message.json")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(object, asked);

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
            lookup(&harness, &format!("channels/C01/messages/{}", ts(5)))
        )
        .await
        .unwrap()
        .is_missing()
    );

    tokio::time::sleep(PAST_DEADLINE).await;
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
}

/// The content of a file at the first read of `content`, as a blob fetch with
/// the version of its `edit_timestamp`, no request again for the same one,
/// the content again for another one, and no folder of a file without
/// `url_private_download`.
#[tokio::test(flavor = "multi_thread")]
async fn the_content_of_a_file_is_the_blob_of_its_edit_timestamp() {
    let harness = slack_harness();
    channels(&harness, &[channel("C01", "general")]).await;
    let notes = |edited| {
        with_files(
            message(1, Some("U01"), "Notes"),
            &[file("F01", "notes.txt", edited), hidden("F02")],
        )
    };
    first_full_listing(&harness, "C01", &[notes(1_700_000_000)]).await;
    let files = format!("channels/C01/messages/{}/files", ts(1));
    assert_eq!(
        served(&harness, names(&harness, &files)).await.unwrap(),
        ["F01"]
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

    tokio::time::sleep(PAST_DEADLINE).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
        vec![ok(&page("messages", &[notes(1_700_000_100)], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1]));
    expect_history(&requests[0], "C01", None, None);
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
/// `cursor`: of a full listing of the users, of a full listing of a channel,
/// of the replies of a message, and of the new messages of a channel, each
/// with the error of its read.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn invalid_cursor_begins_the_requests_again_from_the_first_one() {
    let harness = slack_harness();
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "users"),
        vec![
            ok(&page("members", &[user("U01", None)], Some("cursor-2"))),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Permission)), "{stopped:?}");
    expect_users(&requests[0], None);
    expect_users(&requests[1], Some("cursor-2"));
    let (listing, requests) = answering(
        &harness,
        names(&harness, "users"),
        vec![ok(&page("members", &[user("U01", None)], None))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["U01"]);
    expect_users(&requests[0], None);

    channels(&harness, &[channel("C01", "general")]).await;
    let question = || thread(message(1, Some("U01"), "Question"), 1, 11);
    let (stopped, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
        vec![
            ok(&page("messages", &[question()], Some("cursor-2"))),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Permission)), "{stopped:?}");
    expect_history(&requests[0], "C01", None, None);
    expect_history(&requests[1], "C01", None, Some("cursor-2"));
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C01/messages"),
        vec![ok(&page(
            "messages",
            &[message(2, Some("U02"), "Second"), question()],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[1, 2]));
    expect_history(&requests[0], "C01", None, None);

    let replies = format!("channels/C01/messages/{}/replies", ts(1));
    let (stopped, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![
            ok(&page("messages", &[question()], Some("cursor-2"))),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Permission)), "{stopped:?}");
    expect_replies(&requests[0], "C01", &ts(1), None);
    expect_replies(&requests[1], "C01", &ts(1), Some("cursor-2"));
    let (listing, requests) = answering(
        &harness,
        names(&harness, &replies),
        vec![ok(&page(
            "messages",
            &[question(), reply(11, 1, "U02", "An answer")],
            None,
        ))],
    )
    .await;
    assert_eq!(listing.unwrap(), stamps(&[11]));
    expect_replies(&requests[0], "C01", &ts(1), None);

    tokio::time::sleep(PAST_DEADLINE).await;
    let text = format!("channels/C01/messages/{}/text", ts(2));
    let (stopped, requests) = answering(
        &harness,
        read(&harness, &text),
        vec![
            ok(&page(
                "messages",
                &[message(3, Some("U01"), "Third")],
                Some("cursor-2"),
            )),
            failure("invalid_cursor"),
        ],
    )
    .await;
    assert!(matches!(stopped, Err(NsError::Permission)), "{stopped:?}");
    expect_history(&requests[0], "C01", Some(ts(2).as_str()), None);
    expect_history(&requests[1], "C01", Some(ts(2).as_str()), Some("cursor-2"));
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
    expect_history(&requests[0], "C01", Some(ts(2).as_str()), None);
}

/// The removal of a channel for `channel_not_found` and for
/// `not_in_channel`, the error of `ok` of `false` for a read, and the wait of
/// `Retry-After` after `429`.
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
        ],
    )
    .await;
    for (id, error) in [("C01", "channel_not_found"), ("C02", "not_in_channel")] {
        let (listing, requests) = answering(
            &harness,
            names(&harness, &format!("channels/{id}/messages")),
            vec![failure(error)],
        )
        .await;
        assert!(
            matches!(listing, Err(NsError::NotFound)),
            "{error}: {listing:?}"
        );
        expect_history(&requests[0], id, None, None);
        assert!(
            served(&harness, lookup(&harness, &format!("channels/{id}")))
                .await
                .unwrap()
                .is_missing(),
            "{id}"
        );
    }
    assert_eq!(
        served(&harness, names(&harness, "channels")).await.unwrap(),
        ["C03"]
    );

    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C03/messages"),
        vec![failure("missing_scope")],
    )
    .await;
    assert!(matches!(listing, Err(NsError::Permission)), "{listing:?}");
    expect_history(&requests[0], "C03", None, None);

    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C03/messages"),
        vec![rate_limited("1")],
    )
    .await;
    assert!(
        matches!(listing, Err(NsError::RateLimited { .. })),
        "{listing:?}"
    );
    expect_history(&requests[0], "C03", None, None);
    let listing = served(&harness, names(&harness, "channels/C03/messages")).await;
    assert!(
        matches!(listing, Err(NsError::RateLimited { .. })),
        "{listing:?}"
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (listing, requests) = answering(
        &harness,
        names(&harness, "channels/C03/messages"),
        vec![ok(&page("messages", &[], None))],
    )
    .await;
    assert!(listing.unwrap().is_empty());
    expect_history(&requests[0], "C03", None, None);
}
