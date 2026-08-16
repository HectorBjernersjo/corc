//! A minimal RFC 6455 client, just enough to speak CDP to a headless Chromium.
//!
//! corc has no async runtime and deliberately few dependencies (`usage.rs`
//! shells out to curl rather than pulling in an HTTP stack), and CDP needs
//! exactly one thing a plain socket cannot do: the WebSocket framing. Only
//! what Chromium actually uses is implemented — text frames, continuation,
//! ping/pong, close — and the read side is timeout-driven so the caller can
//! interleave a screencast with resize polling on one thread.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct WebSocket {
    stream: TcpStream,
    /// Bytes read from the socket but not yet consumed as a complete frame.
    buf: Vec<u8>,
    /// Payload of a fragmented message accumulated so far.
    fragment: Vec<u8>,
}

impl WebSocket {
    /// Open a connection to `ws://host:port/path`. `read_timeout` bounds every
    /// later `recv`, which is what lets a single-threaded caller stay
    /// responsive while no frames are arriving.
    pub fn connect(host: &str, port: u16, path: &str, read_timeout: Duration) -> Result<Self> {
        let mut stream = TcpStream::connect((host, port))
            .with_context(|| format!("connecting to {host}:{port}"))?;
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(read_timeout))?;

        let key = crate::base64::encode(&random_bytes()?);
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;

        // Read just the response head. The server sends nothing else until we
        // do, so anything past the blank line belongs to the frame stream.
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte)? == 0 {
                bail!("connection closed during websocket handshake");
            }
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf);
        if !head.starts_with("HTTP/1.1 101") {
            bail!(
                "websocket upgrade refused: {}",
                head.lines().next().unwrap_or("(empty response)")
            );
        }

        Ok(Self {
            stream,
            buf: Vec::new(),
            fragment: Vec::new(),
        })
    }

    /// Send one masked text frame. Client frames must always be masked.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        let payload = text.as_bytes();
        let mut frame = vec![0x81]; // FIN | text
        let mask_bit = 0x80;
        match payload.len() {
            n if n < 126 => frame.push(mask_bit | n as u8),
            n if n <= u16::MAX as usize => {
                frame.push(mask_bit | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(mask_bit | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = random_bytes()?[..4].to_vec();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame)?;
        Ok(())
    }

    /// Next complete text message, or `None` when the read timed out with
    /// nothing to deliver — the caller's cue to go do something else.
    /// Fragmented messages are reassembled; ping and close are answered here
    /// and never surface.
    pub fn recv(&mut self) -> Result<Option<String>> {
        loop {
            match self.take_frame()? {
                Some(frame) => {
                    match frame.opcode {
                        // Continuation of the message in progress.
                        0x0 => self.fragment.extend_from_slice(&frame.payload),
                        0x1 | 0x2 => {
                            self.fragment = frame.payload;
                        }
                        0x8 => bail!("websocket closed by peer"),
                        0x9 => {
                            self.send_pong(&frame.payload)?;
                            continue;
                        }
                        0xa => continue, // pong
                        other => bail!("unexpected websocket opcode {other}"),
                    }
                    if frame.fin {
                        let message = std::mem::take(&mut self.fragment);
                        return Ok(Some(String::from_utf8_lossy(&message).into_owned()));
                    }
                }
                None => match self.fill()? {
                    // Timed out with no more bytes: hand control back.
                    false => return Ok(None),
                    true => continue,
                },
            }
        }
    }

    fn send_pong(&mut self, payload: &[u8]) -> Result<()> {
        let mask = random_bytes()?[..4].to_vec();
        let mut frame = vec![0x8a, 0x80 | payload.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame)?;
        Ok(())
    }

    /// Read more bytes into the buffer. `Ok(false)` means the read timed out,
    /// which is a normal idle tick rather than an error.
    fn fill(&mut self) -> Result<bool> {
        let mut chunk = [0u8; 32 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => bail!("connection closed by peer"),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Pull one whole frame out of the buffer, or `None` when the bytes for a
    /// complete frame have not arrived yet.
    fn take_frame(&mut self) -> Result<Option<Frame>> {
        let Some((frame, consumed)) = parse_frame(&self.buf)? else {
            return Ok(None);
        };
        self.buf.drain(..consumed);
        Ok(Some(frame))
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Decode one frame from the front of `buf`, returning it with the number of
/// bytes it occupied. Server-to-client frames are never masked, so a masked
/// one is a protocol violation rather than something to unmask.
fn parse_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    let opcode = buf[0] & 0x0f;
    if buf[1] & 0x80 != 0 {
        bail!("server sent a masked websocket frame");
    }
    let short_len = (buf[1] & 0x7f) as usize;
    let (len, header) = match short_len {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(b) as usize, 10)
        }
        n => (n, 2),
    };
    if buf.len() < header + len {
        return Ok(None);
    }
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload: buf[header..header + len].to_vec(),
        },
        header + len,
    )))
}

fn random_bytes() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::parse_frame;

    #[test]
    fn frames_are_decoded_across_every_length_encoding() {
        // Short form.
        let (frame, used) = parse_frame(&[0x81, 0x03, b'a', b'b', b'c'])
            .unwrap()
            .unwrap();
        assert!(frame.fin);
        assert_eq!(frame.opcode, 1);
        assert_eq!(frame.payload, b"abc");
        assert_eq!(used, 5);

        // 16-bit form, and a non-final fragment.
        let mut buf = vec![0x01, 126, 0x01, 0x00];
        buf.extend(std::iter::repeat_n(b'x', 256));
        let (frame, used) = parse_frame(&buf).unwrap().unwrap();
        assert!(!frame.fin);
        assert_eq!(frame.payload.len(), 256);
        assert_eq!(used, 260);

        // 64-bit form.
        let mut buf = vec![0x80, 127, 0, 0, 0, 0, 0, 0, 0, 2];
        buf.extend_from_slice(b"hi");
        let (frame, used) = parse_frame(&buf).unwrap().unwrap();
        assert_eq!(frame.opcode, 0);
        assert_eq!(frame.payload, b"hi");
        assert_eq!(used, 12);
    }

    #[test]
    fn a_partial_frame_yields_nothing_and_consumes_nothing() {
        assert!(parse_frame(&[]).unwrap().is_none());
        assert!(parse_frame(&[0x81]).unwrap().is_none());
        assert!(parse_frame(&[0x81, 0x05, b'a']).unwrap().is_none());
        // Length header itself still incomplete.
        assert!(parse_frame(&[0x81, 126, 0x01]).unwrap().is_none());
    }

    #[test]
    fn a_masked_server_frame_is_rejected() {
        assert!(parse_frame(&[0x81, 0x81, 1, 2, 3, 4, b'a']).is_err());
    }
}
