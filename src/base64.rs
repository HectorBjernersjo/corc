//! Standard base64 encoding. Only the encoder exists: CDP hands screencast
//! frames over already base64-encoded and the kitty graphics protocol wants
//! them the same way, so corc forwards that string untouched and never needs
//! to decode a frame. The encoder is here for the WebSocket handshake key.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let digits = [
            ALPHABET[(n >> 18 & 0x3f) as usize],
            ALPHABET[(n >> 12 & 0x3f) as usize],
            ALPHABET[(n >> 6 & 0x3f) as usize],
            ALPHABET[(n & 0x3f) as usize],
        ];
        for (i, d) in digits.iter().enumerate() {
            // Two padding chars for a 1-byte tail, one for a 2-byte tail.
            out.push(if i > chunk.len() { '=' } else { *d as char });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn encodes_every_padding_case() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(&[0xff, 0xfe, 0xfd]), "//79");
    }
}
