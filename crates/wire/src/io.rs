//! Tokio adapters: a bounded frame reader and the two handshake sides.

use prost::Message as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{Decoder, Item, Kind, encode_frame};
use crate::handshake::{Identity, Negotiated, Verdict, evaluate, evaluate_preamble, hello_for};
use crate::records::{Hello, Reject, Welcome};
use crate::{Limits, Outbox, PROTOCOL_MAJOR, WireError, preamble};

/// Bytes read from the socket at a time.
pub const READ_CHUNK: usize = 64 << 10;

#[derive(Debug)]
pub struct Reader<R> {
    inner: R,
    decoder: Decoder,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(inner: R, limits: Limits) -> Reader<R> {
        Reader {
            inner,
            decoder: Decoder::new(limits),
            buf: vec![0; READ_CHUNK],
        }
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.decoder.set_limits(limits);
    }

    pub async fn next_item(&mut self) -> Result<Item, WireError> {
        loop {
            if let Some(item) = self.decoder.next_item()? {
                return Ok(item);
            }
            let n = self.inner.read(&mut self.buf).await?;
            if n == 0 {
                return Err(WireError::Closed);
            }
            self.decoder.feed(&self.buf[..n]);
        }
    }

    pub async fn next_frame(&mut self) -> Result<(Kind, Vec<u8>), WireError> {
        match self.next_item().await? {
            Item::Frame { kind, payload } => Ok((kind, payload)),
            Item::Preamble { .. } => Err(WireError::Unexpected("second preamble")),
        }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: Kind,
    payload: &[u8],
    max_frame: u32,
) -> Result<(), WireError> {
    w.write_all(&encode_frame(kind, 0, payload, max_frame)?)
        .await?;
    w.flush().await?;
    Ok(())
}

/// Write everything queued, priority first.
pub async fn drain<W: AsyncWrite + Unpin>(w: &mut W, outbox: &mut Outbox) -> Result<(), WireError> {
    while let Some(frame) = outbox.next_frame() {
        w.write_all(&frame).await?;
    }
    w.flush().await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum HandshakeError {
    Wire(WireError),
    /// The peer speaks another protocol major version. Found from its
    /// preamble; nothing else it sent was decoded.
    Protocol {
        major: u16,
        minor: u16,
    },
    /// The server refused this client.
    Rejected(Reject),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Wire(e) => write!(f, "{e}"),
            HandshakeError::Protocol { major, minor } => write!(
                f,
                "peer speaks protocol {major}.{minor}, this build speaks {PROTOCOL_MAJOR}.x"
            ),
            HandshakeError::Rejected(r) => write!(f, "{}", r.message),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<WireError> for HandshakeError {
    fn from(e: WireError) -> Self {
        HandshakeError::Wire(e)
    }
}

/// Client side: send preamble and `Hello`, then expect `Welcome`.
pub async fn client_handshake<R, W>(
    reader: &mut Reader<R>,
    writer: &mut W,
    me: &Identity,
) -> Result<(Welcome, Limits), HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(&preamble())
        .await
        .map_err(WireError::from)?;
    write_frame(
        writer,
        Kind::Hello,
        &hello_for(me).encode_to_vec(),
        me.limits.max_frame,
    )
    .await?;
    match reader.next_item().await? {
        Item::Preamble { major, .. } if major == PROTOCOL_MAJOR => {}
        Item::Preamble { major, minor } => return Err(HandshakeError::Protocol { major, minor }),
        Item::Frame { .. } => return Err(WireError::Unexpected("frame before preamble").into()),
    }
    match reader.next_frame().await? {
        (Kind::Welcome, p) => {
            let w = Welcome::decode(p.as_slice()).map_err(WireError::from)?;
            let limits = me.limits.negotiate(w.max_frame, w.max_message);
            reader.set_limits(limits);
            Ok((w, limits))
        }
        (Kind::Reject, p) => Err(HandshakeError::Rejected(
            Reject::decode(p.as_slice()).map_err(WireError::from)?,
        )),
        _ => Err(WireError::Unexpected("frame instead of Welcome").into()),
    }
}

/// Server side: send our preamble, judge the peer's version from its
/// preamble before decoding anything, then judge its `Hello`. On success
/// `welcome` builds the reply.
pub async fn server_handshake<R, W>(
    reader: &mut Reader<R>,
    writer: &mut W,
    me: &Identity,
    welcome: impl FnOnce(&Negotiated) -> Welcome,
) -> Result<(Hello, Negotiated), HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(&preamble())
        .await
        .map_err(WireError::from)?;
    let (major, minor) = match reader.next_item().await? {
        Item::Preamble { major, minor } => (major, minor),
        Item::Frame { .. } => return Err(WireError::Unexpected("frame before preamble").into()),
    };
    if let Some(Verdict::Reject(r)) = evaluate_preamble(me, major, minor) {
        write_frame(
            writer,
            Kind::Reject,
            &r.encode_to_vec(),
            me.limits.max_frame,
        )
        .await?;
        return Err(HandshakeError::Rejected(r));
    }
    let hello = match reader.next_frame().await? {
        (Kind::Hello, p) => Hello::decode(p.as_slice()).map_err(WireError::from)?,
        _ => return Err(WireError::Unexpected("frame instead of Hello").into()),
    };
    match evaluate(me, &hello) {
        Verdict::Reject(r) => {
            write_frame(
                writer,
                Kind::Reject,
                &r.encode_to_vec(),
                me.limits.max_frame,
            )
            .await?;
            Err(HandshakeError::Rejected(r))
        }
        Verdict::Accept(n) => {
            let w = welcome(&n);
            write_frame(
                writer,
                Kind::Welcome,
                &w.encode_to_vec(),
                me.limits.max_frame,
            )
            .await?;
            reader.set_limits(n.limits);
            Ok((hello, n))
        }
    }
}
