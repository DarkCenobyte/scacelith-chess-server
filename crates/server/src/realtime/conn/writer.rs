//! The writer task of an authenticated connection: drains its outbound queue to the socket in
//! batches (one write each), then closes the WebSocket with the queue's close request. The frames
//! are queued as this server's minor encodes them, whatever the session's minor: here, a session
//! of an older minor gets the values it knows in place of later ones, and never a message type of
//! a later minor (`scacelith_protocol::frame_for_minor`: the opponent's `Stance`, minor 2, is
//! withheld from a session of minor 0 or 1). The senders (the host actors, the lobby) never need
//! to know a session's minor.

use std::sync::Arc;

use bytes::Bytes;
use scacelith_protocol::{ForMinor, MINOR, frame_for_minor};

use crate::net::ws::{CLOSE_ABNORMAL, WsWriter};
use crate::realtime::endpoint::Outbound;

/// Writes what is queued on `out` until the close request (written after every frame queued
/// before it) or until the socket refuses a write (the queue is then closed, so that senders stop
/// queueing for a dead connection). `minor`: the session's negotiated minor.
pub(crate) async fn run(mut writer: WsWriter, out: Arc<Outbound>, minor: u16) {
    loop {
        let mut batch = out.take();
        if !batch.frames.is_empty() {
            // The bytes taken from the queue, withheld frames included: all of them leave it.
            let bytes: usize = batch.frames.iter().map(Bytes::len).sum();
            if minor < MINOR {
                batch.frames.retain_mut(|frame| match frame_for_minor(frame, minor) {
                    ForMinor::Keep => true,
                    ForMinor::Replace(older) => {
                        *frame = older;
                        true
                    }
                    ForMinor::Withhold => false,
                });
            }
            let sent = if batch.frames.is_empty() {
                Ok(())
            } else {
                let frames: Vec<&[u8]> = batch.frames.iter().map(|f| &f[..]).collect();
                writer.send_batch(&frames).await
            };
            out.written(bytes);
            if sent.is_err() {
                out.close(CLOSE_ABNORMAL, "");
                return;
            }
        }
        if let Some(close) = batch.close {
            writer.close(close.code, &close.reason);
            return;
        }
        out.ready().await;
    }
}
