//! `dream bridge --stdio`: the daemon's protocol over standard streams.
//!
//! This is the fixed command a coordinator runs on a remote host through
//! `ssh -T` (spec 7.1, 11.2). It connects to the user's daemon socket,
//! starting the daemon if nothing answers, and copies bytes both ways. It
//! never parses or rewrites the protocol: the handshake travels end to end,
//! so version and build-family checks happen between the two real peers.
//!
//! Standard output carries protocol bytes and nothing else; every
//! diagnostic goes to standard error. The bridge is short-lived. A daemon
//! it starts is detached from it, so closing the bridge (or SSH) leaves the
//! daemon, tmux, and its panes running (spec 11.3).

use std::time::Duration;

use client::{Launch, ensure_running_raw};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::daemon::{DaemonArgs, explain};

/// How long to wait for the daemon to close its side after our standard
/// input ends, so a final reply still reaches the peer.
const LINGER: Duration = Duration::from_secs(2);

pub fn stdio(d: &DaemonArgs) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        let (socket, started) = ensure_running_raw(&Launch::new(d.start.clone()))
            .await
            .map_err(explain)?;
        if started {
            eprintln!("dream bridge: started dreamerd");
        }
        let (from_daemon, to_daemon) = socket.into_split();
        splice(
            tokio::io::stdin(),
            tokio::io::stdout(),
            from_daemon,
            to_daemon,
        )
        .await
    });
    // Standard input is read on a blocking thread that may still be waiting;
    // do not wait for it.
    rt.shutdown_background();
    result
}

/// Copy `input` to `to_daemon` and `from_daemon` to `output` until the
/// daemon's side ends. When `input` ends first, half-close towards the
/// daemon and give it a moment to finish.
async fn splice<I, O, R, W>(
    input: I,
    output: O,
    from_daemon: R,
    mut to_daemon: W,
) -> Result<(), String>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let down = pump(from_daemon, output);
    tokio::pin!(down);
    let up = async {
        let r = pump(input, &mut to_daemon).await;
        let _ = to_daemon.shutdown().await;
        r
    };
    tokio::select! {
        r = &mut down => r.map_err(|e| format!("daemon to stdout: {e}")),
        r = up => {
            r.map_err(|e| format!("stdin to daemon: {e}"))?;
            match tokio::time::timeout(LINGER, down).await {
                Ok(r) => r.map_err(|e| format!("daemon to stdout: {e}")),
                Err(_) => Ok(()),
            }
        }
    }
}

/// Copy until end of input, flushing after every read so nothing waits in
/// a buffer while the peer waits for it.
async fn pump<R, W>(mut from: R, mut to: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            return to.flush().await;
        }
        to.write_all(&buf[..n]).await?;
        to.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bytes_cross_both_ways_unchanged_and_input_end_half_closes() {
        let (mut peer, bridge_side) = tokio::io::duplex(1 << 16);
        let (daemon_side, mut daemon) = tokio::io::duplex(1 << 16);
        let (input, output) = tokio::io::split(bridge_side);
        let (from_daemon, to_daemon) = tokio::io::split(daemon_side);
        let task = tokio::spawn(splice(input, output, from_daemon, to_daemon));

        let all: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
        let mut got = vec![0; all.len()];
        let (w, r) = tokio::join!(peer.write_all(&all), daemon.read_exact(&mut got));
        w.expect("write");
        r.expect("daemon reads");
        assert_eq!(got, all);

        daemon.write_all(b"DRMR\0\x01").await.expect("reply");
        let mut back = [0; 6];
        peer.read_exact(&mut back).await.expect("peer reads");
        assert_eq!(&back, b"DRMR\0\x01");

        // The peer's end of input reaches the daemon as end of input, and the
        // bridge ends once the daemon closes too.
        peer.shutdown().await.expect("shutdown");
        let mut rest = Vec::new();
        daemon.read_to_end(&mut rest).await.expect("eof");
        assert!(rest.is_empty());
        drop(daemon);
        task.await.expect("join").expect("bridge ends cleanly");
    }
}
