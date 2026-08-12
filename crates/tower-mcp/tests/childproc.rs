//! Integration tests for [`ChildProcessConnection`] (#1334).
//!
//! `send_request` used to take the next line off the child's stdout
//! unconditionally and treat it as the answer, so a spec-compliant child that
//! writes a notification before its response desynchronized the connection
//! permanently: the notification failed to parse as a response, and the next
//! `send_request` got the *previous* call's answer. `read_line` also decoded
//! before framing, so one invalid UTF-8 byte on the child's stdout was fatal
//! instead of costing a single frame, and there was no BOM strip on the
//! receive path, unlike every other stdio transport in this crate.
//!
//! Every test here drives a real child process via `sh -c`, the same way
//! `client/stdio.rs`'s own tests do. The findings below landed as
//! `#[ignore]`d reproductions and have since been fixed, so each one now
//! guards a regression rather than describing a bug; the issue number stays
//! in the doc comment so what it guards stays legible.
//!
//! # Every script reads before it writes
//!
//! A child that never reads stdin at all is a race, not a fixture: `sh -c`
//! with nothing but `printf` runs to completion (and exits, closing its end
//! of the stdin pipe) as fast as the shell can fork, which can beat the
//! client's first `write_all` to the same pipe. That write then fails with
//! `Broken pipe` -- observed on Linux CI, not on macOS, because the two
//! platforms schedule the race differently; it is not a `childproc.rs`
//! defect and reproducing it locally on one platform proves nothing about
//! the other. Every script below starts with a blocking `read` so the child
//! cannot exit before the client's write lands, which makes the test's own
//! control flow decide the child's lifetime instead of the OS scheduler.

#![cfg(feature = "childproc")]

use tower_mcp::transport::childproc::ChildProcessTransport;

/// #1334, defect 1: a notification the child writes before its response must
/// not be mistaken for the response. `send_request` has to keep reading past
/// it and return the frame whose id matches the request it sent.
#[tokio::test]
async fn response_correlates_by_id_past_a_leading_notification() {
    let mut conn = ChildProcessTransport::new("sh")
        .arg("-c")
        .arg(concat!(
            r#"read -r _line; "#,
            r#"printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{}}\n'; "#,
            r#"printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'"#
        ))
        .spawn()
        .await
        .unwrap();

    let result = conn
        .send_request("ping", serde_json::json!({}))
        .await
        .expect("the response after the notification must still be delivered");

    assert_eq!(result, serde_json::json!({"ok": true}));

    conn.shutdown().await.unwrap();
}

/// #1334, defect 1: a response for a request other than the one currently
/// being waited on must not be discarded or mistaken for the answer. Two
/// sequential calls, answered out of order, must each get their own result --
/// the id decides which is which, not arrival order.
///
/// The child answers both ids from a single read: it cannot legitimately
/// have id 2's request yet (the client only sends it after id 1's call
/// returns), so both responses are canned output written in reply to
/// reading id 1's request, deliberately out of order. `send_request`
/// unconditionally writes the request before consulting anything it already
/// has buffered, so the second call still needs somewhere for its write to
/// land; `cat` drains and discards it (and anything further) until the
/// client closes stdin at `shutdown`.
#[tokio::test]
async fn out_of_order_responses_are_matched_by_id_not_by_arrival_order() {
    let mut conn = ChildProcessTransport::new("sh")
        .arg("-c")
        .arg(concat!(
            r#"read -r _line; "#,
            r#"printf '{"jsonrpc":"2.0","id":2,"result":{"which":2}}\n'; "#,
            r#"printf '{"jsonrpc":"2.0","id":1,"result":{"which":1}}\n'; "#,
            r#"cat >/dev/null"#
        ))
        .spawn()
        .await
        .unwrap();

    // First call sends id 1 but the child answers id 2 first; the id-2 frame
    // must be set aside, not returned as if it were id 1's answer.
    let first = conn
        .send_request("a", serde_json::json!({}))
        .await
        .expect("id 1's own response must be returned, not id 2's");
    assert_eq!(first, serde_json::json!({"which": 1}));

    // Second call sends id 2; its answer already arrived and must have been
    // held rather than lost.
    let second = conn
        .send_request("b", serde_json::json!({}))
        .await
        .expect("the response set aside for id 2 must be delivered");
    assert_eq!(second, serde_json::json!({"which": 2}));

    conn.shutdown().await.unwrap();
}

/// #1334, defect 2: `read_line` decoded before framing, so one byte that is
/// not valid UTF-8 turned into a fatal `Error::Transport` instead of costing
/// only the frame it landed in. `crate::framing::FrameReader` frames over
/// bytes precisely so this can be discarded and the connection can read on.
#[tokio::test]
async fn invalid_utf8_frame_is_discarded_and_the_response_still_arrives() {
    let mut conn = ChildProcessTransport::new("sh")
        .arg("-c")
        .arg(concat!(
            r#"read -r _line; "#,
            r#"printf '\377\376\n'; "#,
            r#"printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'"#
        ))
        .spawn()
        .await
        .unwrap();

    let result = conn
        .send_request("ping", serde_json::json!({}))
        .await
        .expect("a bad frame must cost only itself, not the connection");

    assert_eq!(result, serde_json::json!({"ok": true}));

    conn.shutdown().await.unwrap();
}

/// #1334, defect 3: a leading UTF-8 BOM on the response frame must be
/// stripped, the same as every other receive path in this crate
/// (`crate::framing::clean_input_line`, #1303).
#[tokio::test]
async fn a_leading_bom_on_the_response_is_stripped() {
    let mut conn = ChildProcessTransport::new("sh")
        .arg("-c")
        .arg(concat!(
            r#"read -r _line; "#,
            r#"printf '\357\273\277{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'"#
        ))
        .spawn()
        .await
        .unwrap();

    let result = conn
        .send_request("ping", serde_json::json!({}))
        .await
        .expect("a BOM-prefixed response must still parse");

    assert_eq!(result, serde_json::json!({"ok": true}));

    conn.shutdown().await.unwrap();
}
