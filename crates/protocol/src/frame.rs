//! Length-limited newline-delimited framing.
//!
//! A TCP chunk is not a frame: one chunk may contain many complete messages,
//! and a message can span chunks. The decoder retains only an owned copy of an
//! unfinished tail, never a view of a large read (the TypeScript header's
//! memory rule, mirrored here by `Vec<u8>`'s ownership).

use crate::error::FrameError;

/// Splits a byte stream into newline-delimited frames with a hard cap.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
    max_frame_bytes: usize,
}

impl FrameDecoder {
    /// A decoder whose frames never exceed `max_frame_bytes`.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_frame_bytes,
        }
    }

    /// Feeds one received chunk, returning every frame completed by it.
    ///
    /// The cap is enforced across all chunks belonging to one frame: a peer
    /// cannot bypass it by sending a large frame in small pieces. On breach
    /// the decoder returns [`FrameError::ExceededLimit`] and the caller must
    /// destroy the connection (both directions, both languages).
    pub fn append(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        let mut frames = Vec::new();
        let mut start = 0;
        while start < chunk.len() {
            let Some(rel) = chunk[start..].iter().position(|&b| b == b'\n') else {
                // No newline: everything left is an unfinished tail.
                let tail = &chunk[start..];
                self.push_tail(tail)?;
                return Ok(frames);
            };
            let newline = start + rel;
            let part = &chunk[start..newline];
            self.push_tail(part)?;
            frames.push(std::mem::take(&mut self.pending));
            start = newline + 1;
        }
        Ok(frames)
    }

    /// Bytes of the frame currently being assembled.
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    fn push_tail(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.pending.len() + bytes.len() > self.max_frame_bytes {
            return Err(FrameError::ExceededLimit);
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }
}
