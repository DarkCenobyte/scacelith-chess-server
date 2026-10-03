//! The writer task of an authenticated connection: drains its outbound queue to the socket in
//! batches (one write each), then closes the WebSocket with the queue's close request.

use std::sync::Arc;

use bytes::Bytes;

use crate::net::ws::{CLOSE_ABNORMAL, WsWriter};
use crate::realtime::endpoint::Outbound;

/// Writes what is queued on `out` until the close request (written after every frame queued
/// before it) or until the socket refuses a write (the queue is then closed, so that senders stop
/// queueing for a dead connection).
pub(crate) async fn run(mut writer: WsWriter, out: Arc<Outbound>) {
    loop {
        let batch = out.take();
        if !batch.frames.is_empty() {
            let bytes: usize = batch.frames.iter().map(Bytes::len).sum();
            let sent = {
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
