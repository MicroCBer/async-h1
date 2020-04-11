//! Process HTTP connections on the server.

use std::pin::Pin;

use async_std::io::Read;
use async_std::io::{self};
use async_std::task::{Context, Poll};
use http_types::Response;

use crate::date::fmt_http_date;

const CR: u8 = b'\r';
const LF: u8 = b'\n';

/// A streaming HTTP encoder.
///
/// This is returned from [`encode`].
#[derive(Debug)]
pub(crate) struct Encoder {
    /// HTTP headers to be sent.
    res: Response,
    /// The state of the encoding process
    state: EncoderState,
    /// Track bytes read in a call to poll_read.
    bytes_read: usize,
    /// The data we're writing as part of the head section.
    head: Vec<u8>,
    /// The amount of bytes read from the head section.
    head_bytes_read: usize,
    /// The total length of the body.
    /// This is only used in the known-length body encoder.
    body_len: usize,
    /// The amount of bytes read from the body.
    /// This is only used in the known-length body encoder.
    body_bytes_read: usize,
    /// The current chunk being re
    /// This is only used in the chunked body encoder.
    chunk: Option<io::Cursor<Vec<u8>>>,
    /// Determine whether this is the last chunk
    /// This is only used in the chunked body encoder.
    is_last: bool,
}

#[derive(Debug)]
enum EncoderState {
    Start,
    Head,
    Body,
    UncomputedChunked,
    ComputedChunked,
    Done,
}

impl Encoder {
    /// Create a new instance.
    pub(crate) fn encode(res: Response) -> Self {
        Self {
            res,
            state: EncoderState::Start,
            bytes_read: 0,
            head: vec![],
            head_bytes_read: 0,
            body_len: 0,
            body_bytes_read: 0,
            chunk: None,
            is_last: false,
        }
    }
}

impl Encoder {
    // Encode the headers to a buffer, the first time we poll.
    fn encode_start(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        log::trace!("Server response encoding: start");
        self.state = EncoderState::Head;

        let reason = self.res.status().canonical_reason();
        let status = self.res.status();
        std::io::Write::write_fmt(
            &mut self.head,
            format_args!("HTTP/1.1 {} {}\r\n", status, reason),
        )?;

        // If the body isn't streaming, we can set the content-length ahead of time. Else we need to
        // send all items in chunks.
        if let Some(len) = self.res.len() {
            std::io::Write::write_fmt(&mut self.head, format_args!("content-length: {}\r\n", len))?;
        } else {
            std::io::Write::write_fmt(
                &mut self.head,
                format_args!("transfer-encoding: chunked\r\n"),
            )?;
        }

        let date = fmt_http_date(std::time::SystemTime::now());
        std::io::Write::write_fmt(&mut self.head, format_args!("date: {}\r\n", date))?;

        for (header, values) in self.res.iter() {
            for value in values.iter() {
                std::io::Write::write_fmt(
                    &mut self.head,
                    format_args!("{}: {}\r\n", header, value),
                )?
            }
        }

        std::io::Write::write_fmt(&mut self.head, format_args!("\r\n"))?;
        self.encode_head(cx, buf)
    }

    /// Encode the status code + headers.
    fn encode_head(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        // Read from the serialized headers, url and methods.
        let head_len = self.head.len();
        let len = std::cmp::min(head_len - self.head_bytes_read, buf.len());
        let range = self.head_bytes_read..self.head_bytes_read + len;
        buf[0..len].copy_from_slice(&self.head[range]);
        self.bytes_read += len;
        self.head_bytes_read += len;

        // If we've read the total length of the head we're done
        // reading the head and can transition to reading the body
        if self.head_bytes_read == head_len {
            // The response length lets us know if we are encoding
            // our body in chunks or not
            match self.res.len() {
                Some(body_len) => {
                    self.body_len = body_len;
                    self.state = EncoderState::Body;
                    log::trace!("Server response encoding: exact size body");
                    return self.encode_body(cx, buf);
                }
                None => {
                    self.state = EncoderState::UncomputedChunked;
                    log::trace!("Server response encoding: chunked body");
                    return self.encode_uncomputed_chunked(cx, buf);
                }
            };
        } else {
            // If we haven't read the entire header it means `buf` isn't
            // big enough. Break out of loop and return from `poll_read`
            return Poll::Ready(Ok(self.bytes_read));
        }
    }

    /// Encode the body with a known length.
    fn encode_body(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        // Double check that we didn't somehow read more bytes than
        // can fit in our buffer
        debug_assert!(self.bytes_read <= buf.len());

        // ensure we have at least room for 1 more byte in our buffer
        if self.bytes_read == buf.len() {
            return Poll::Ready(Ok(self.bytes_read));
        }

        // Figure out how many bytes we can read.
        let upper_bound = (self.bytes_read + self.body_len - self.body_bytes_read).min(buf.len());
        // Read bytes from body
        let range = self.bytes_read..upper_bound;
        let inner_poll_result = Pin::new(&mut self.res).poll_read(cx, &mut buf[range]);
        let new_body_bytes_read = match inner_poll_result {
            Poll::Ready(Ok(n)) => n,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => {
                if self.bytes_read == 0 {
                    return Poll::Pending;
                } else {
                    return Poll::Ready(Ok(self.bytes_read));
                }
            }
        };
        self.body_bytes_read += new_body_bytes_read;
        self.bytes_read += new_body_bytes_read;

        // Double check we did not read more body bytes than the total
        // length of the body
        debug_assert!(
            self.body_bytes_read <= self.body_len,
            "Too many bytes read. Expected: {}, read: {}",
            self.body_len,
            self.body_bytes_read
        );

        if self.body_len == self.body_bytes_read {
            // If we've read the `len` number of bytes, end
            self.state = EncoderState::Done;
            return Poll::Ready(Ok(self.bytes_read));
        } else if new_body_bytes_read == 0 {
            // If we've reached unexpected EOF, end anyway
            // TODO: do something?
            self.state = EncoderState::Done;
            return Poll::Ready(Ok(self.bytes_read));
        } else {
            self.encode_body(cx, buf)
        }
    }

    /// Compute a "chunk", which is the value from the stream between CRLFs.
    fn encode_uncomputed_chunked(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // We can read a maximum of the buffer's total size
        // minus what we've already filled the buffer with
        let buffer_remaining = buf.len() - self.bytes_read;

        // ensure we have at least room for 1 byte in our buffer
        if buffer_remaining == 0 {
            return Poll::Ready(Ok(self.bytes_read));
        }
        // we must allocate a separate buffer for the chunk data
        // since we first need to know its length before writing
        // it into the actual buffer
        let mut chunk_buf = vec![0; buffer_remaining];
        // Read bytes from body reader
        let inner_poll_result = Pin::new(&mut self.res).poll_read(cx, &mut chunk_buf);
        let chunk_length = match inner_poll_result {
            Poll::Ready(Ok(n)) => n,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => {
                if self.bytes_read == 0 {
                    return Poll::Pending;
                } else {
                    return Poll::Ready(Ok(self.bytes_read));
                }
            }
        };

        // serialize chunk length as hex
        let chunk_length_string = format!("{:X}", chunk_length);
        let chunk_length_bytes = chunk_length_string.as_bytes();
        let chunk_length_bytes_len = chunk_length_bytes.len();
        const CRLF_LENGTH: usize = 2;

        // calculate the total size of the chunk including serialized
        // length and the CRLF padding
        let total_chunk_size =
            self.bytes_read + chunk_length_bytes_len + CRLF_LENGTH + chunk_length + CRLF_LENGTH;

        // See if we can write the chunk out in one go
        if total_chunk_size < buffer_remaining {
            // Write the chunk length into the buffer
            buf[self.bytes_read..(self.bytes_read + chunk_length_bytes_len)]
                .copy_from_slice(chunk_length_bytes);
            self.bytes_read += chunk_length_bytes_len;

            // follow chunk length with CRLF
            buf[self.bytes_read] = CR;
            buf[self.bytes_read + 1] = LF;
            self.bytes_read += 2;

            // copy chunk into buf
            buf[self.bytes_read..(self.bytes_read + chunk_length)]
                .copy_from_slice(&chunk_buf[..chunk_length]);
            self.bytes_read += chunk_length;

            // follow chunk with CRLF
            buf[self.bytes_read] = CR;
            buf[self.bytes_read + 1] = LF;
            self.bytes_read += 2;

            if chunk_length == 0 {
                self.state = EncoderState::Done;
            }
            return Poll::Ready(Ok(self.bytes_read));
        } else {
            let mut chunk = vec![0; total_chunk_size];
            let mut bytes_written = 0;
            // Write the chunk length into the buffer
            chunk[0..chunk_length_bytes_len].copy_from_slice(chunk_length_bytes);
            bytes_written += chunk_length_bytes_len;

            // follow chunk length with CRLF
            chunk[bytes_written] = CR;
            chunk[bytes_written + 1] = LF;
            bytes_written += 2;

            // copy chunk into buf
            chunk[bytes_written..bytes_written + chunk_length]
                .copy_from_slice(&chunk_buf[..chunk_length]);
            bytes_written += chunk_length;

            // follow chunk with CRLF
            chunk[bytes_written] = CR;
            chunk[bytes_written + 1] = LF;
            self.bytes_read += 2;
            self.state = EncoderState::ComputedChunked;
            self.chunk = Some(io::Cursor::new(chunk));
            self.is_last = chunk_length == 0;
            return self.encode_computed_chunked(cx, buf);
        }
    }

    /// We already have a chunk stored in memory; write it back out.
    fn encode_computed_chunked(
        &mut self,
        cx: &mut Context<'_>,
        mut buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut chunk = self.chunk.as_mut().unwrap();
        let inner_poll_result = Pin::new(&mut chunk).poll_read(cx, &mut buf);
        self.bytes_read += match inner_poll_result {
            Poll::Ready(Ok(n)) => n,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => {
                if self.bytes_read == 0 {
                    return Poll::Pending;
                } else {
                    return Poll::Ready(Ok(self.bytes_read));
                }
            }
        };
        if self.bytes_read == 0 {
            self.state = match self.is_last {
                true => EncoderState::Done,
                false => EncoderState::UncomputedChunked,
            }
        }
        return Poll::Ready(Ok(self.bytes_read));
    }
}

impl Read for Encoder {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // we keep track how many bytes of the head and body we've read
        // in this call of `poll_read`
        self.bytes_read = 0;
        match self.state {
            EncoderState::Start => self.encode_start(cx, buf),
            EncoderState::Head => self.encode_head(cx, buf),
            EncoderState::Body => self.encode_body(cx, buf),
            EncoderState::UncomputedChunked => self.encode_uncomputed_chunked(cx, buf),
            EncoderState::ComputedChunked => self.encode_computed_chunked(cx, buf),
            EncoderState::Done => Poll::Ready(Ok(0)),
        }
    }
}
