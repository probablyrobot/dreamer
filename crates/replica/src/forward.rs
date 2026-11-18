//! A forward to a port on a connected host's loopback (`hosts.forward`),
//! held for as long as the value lives: a web pane in a host's session
//! holds one for the page it shows. The hold is the connection it was made
//! on, kept by this value's own thread; dropping the value closes that
//! connection, and the daemon closes the forward once nothing holds it.

use std::path::PathBuf;

use api::{Request, Response};

/// A held forward. Drop it to let go.
#[derive(Debug)]
pub struct Held {
    _stop: async_channel::Sender<()>,
}

/// Hold a forward to `port` on host `host` through the daemon at `socket`.
/// The receiver gets the port on this machine's 127.0.0.1 that reaches it,
/// or why there is none.
pub fn hold(
    socket: PathBuf,
    host: String,
    port: u16,
) -> std::io::Result<(Held, async_channel::Receiver<Result<u16, String>>)> {
    let (stop, stopped) = async_channel::bounded::<()>(1);
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("dreamer-forward".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = tx.try_send(Err("no runtime".into()));
                return;
            };
            rt.block_on(async move {
                let c = match client::Client::connect(&socket, "gui").await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.try_send(Err(e.to_string()));
                        return;
                    }
                };
                let asked = Request::HostsForward(api::HostForward {
                    name: host,
                    port: u32::from(port),
                });
                let got = match c.call(asked).await {
                    Ok(Response::HostsForward(f)) => {
                        u16::try_from(f.local_port).map_err(|e| e.to_string())
                    }
                    Ok(_) => Err("a reply to another method".into()),
                    Err(e) => Err(e.to_string()),
                };
                let ok = got.is_ok();
                let _ = tx.try_send(got);
                if ok {
                    // Held until the value goes.
                    let _ = stopped.recv().await;
                }
            });
        })?;
    Ok((Held { _stop: stop }, rx))
}
