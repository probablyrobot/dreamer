//! A call on a connection that has already ended fails at once instead of
//! waiting for a reply that cannot come. (The coordinator hit this: a host
//! link that had just dropped still looked usable for a moment.)

use std::time::Duration;

use client::{Client, Pushed};
use wire::Identity;
use wire::io::{Reader, server_handshake};

#[tokio::test]
async fn calls_after_the_connection_closed_fail_at_once() {
    let (client_side, server_side) = tokio::io::duplex(1 << 16);
    let server = tokio::spawn(async move {
        let (r, mut w) = tokio::io::split(server_side);
        let me = Identity::current("test-daemon");
        let mut reader = Reader::new(r, me.limits);
        server_handshake(&mut reader, &mut w, &me, |n| {
            wire::welcome(&me, n, "instance", 1, "host", "default")
        })
        .await
        .expect("handshake");
        // Then stop answering: end the stream towards the client but keep
        // accepting what it writes, as an SSH child does in the moment
        // after the remote side went away.
        use tokio::io::AsyncWriteExt as _;
        let _ = w.shutdown().await;
        reader
    });
    let (r, w) = tokio::io::split(client_side);
    let mut c = Client::over(r, w, "test", 8).await.expect("connect");
    let _keep_reading = server.await.expect("server");
    let mut pushed = c.pushed().expect("pushed");
    // Wait until the client has seen the end.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), pushed.recv()).await {
            Ok(Some(Pushed::Closed { .. })) | Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => panic!("the client never saw the connection close"),
        }
    }
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        c.call(api::Request::DaemonVersion(api::Empty {})),
    )
    .await
    .expect("a call after the close returns at once");
    assert!(r.is_err(), "{r:?}");
}
