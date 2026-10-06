//! Parse and re-encode kitty graphics control data (`k=v,k=v;payload`).

/// Base64 payload bytes per forwarded chunk. Kitty requires a multiple of
/// four for every chunk except the last.
pub const CHUNK_BYTES: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    keys: Vec<(u8, String)>,
    pub payload: Vec<u8>,
}

impl Command {
    /// Parse the bytes between `ESC _ G` and `ESC \`.
    pub fn parse(body: &[u8]) -> Self {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(p) => (&body[..p], body[p + 1..].to_vec()),
            None => (body, Vec::new()),
        };
        let keys = control
            .split(|&b| b == b',')
            .filter_map(|pair| {
                let (&key, rest) = pair.split_first()?;
                let value = rest.strip_prefix(b"=")?;
                Some((key, String::from_utf8_lossy(value).into_owned()))
            })
            .collect();
        Self { keys, payload }
    }

    pub fn get(&self, key: u8) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_u32(&self, key: u8) -> Option<u32> {
        self.get(key)?.parse().ok()
    }

    /// Like [`Self::get_u32`], but kitty treats `0` as "not given" for ids.
    pub fn id(&self, key: u8) -> Option<u32> {
        self.get_u32(key).filter(|&v| v != 0)
    }

    pub fn char(&self, key: u8) -> Option<u8> {
        self.get(key).and_then(|v| v.bytes().next())
    }

    /// True when the control data uses no keys outside `allowed`.
    pub fn only_keys(&self, allowed: &[u8]) -> bool {
        self.keys.iter().all(|(k, _)| allowed.contains(k))
    }

    pub fn set(&mut self, key: u8, value: impl ToString) {
        let value = value.to_string();
        match self.keys.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.keys.push((key, value)),
        }
    }

    pub fn remove(&mut self, key: u8) {
        self.keys.retain(|(k, _)| *k != key);
    }

    /// One complete `ESC _ G … ESC \` sequence.
    pub fn encode(&self) -> Vec<u8> {
        encode_parts(&self.keys, &self.payload)
    }

    /// Encode with `payload` split into chunks: the first carries every key
    /// plus `m=1`, the rest only `m` and `q`.
    pub fn encode_chunked(&self, payload: &[u8]) -> Vec<u8> {
        if payload.len() <= CHUNK_BYTES {
            let mut keys = self.keys.clone();
            keys.retain(|(k, _)| *k != b'm');
            return encode_parts(&keys, payload);
        }
        let q = self.get(b'q').map(str::to_string);
        let mut out = Vec::with_capacity(payload.len() + payload.len() / CHUNK_BYTES * 24);
        let mut chunks = payload.chunks(CHUNK_BYTES).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let more = if chunks.peek().is_some() { "1" } else { "0" };
            let mut keys = if first {
                let mut keys = self.keys.clone();
                keys.retain(|(k, _)| *k != b'm');
                keys
            } else {
                q.iter().map(|q| (b'q', q.clone())).collect()
            };
            keys.push((b'm', more.to_string()));
            out.extend(encode_parts(&keys, chunk));
            first = false;
        }
        out
    }
}

fn encode_parts(keys: &[(u8, String)], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 64);
    out.extend_from_slice(b"\x1b_G");
    for (n, (key, value)) in keys.iter().enumerate() {
        if n > 0 {
            out.push(b',');
        }
        out.push(*key);
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
    }
    if !payload.is_empty() {
        out.push(b';');
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_encode_round_trip() {
        let cmd = Command::parse(b"a=T,U=1,i=42,f=100;QUJD");
        assert_eq!(cmd.get(b'a'), Some("T"));
        assert_eq!(cmd.get_u32(b'i'), Some(42));
        assert_eq!(cmd.payload, b"QUJD");
        assert_eq!(cmd.encode(), b"\x1b_Ga=T,U=1,i=42,f=100;QUJD\x1b\\");
    }

    #[test]
    fn set_replaces_in_place_and_remove_drops() {
        let mut cmd = Command::parse(b"i=1,I=9,q=0");
        cmd.set(b'i', 77);
        cmd.remove(b'I');
        cmd.set(b'q', 2);
        assert_eq!(cmd.encode(), b"\x1b_Gi=77,q=2\x1b\\");
    }

    #[test]
    fn chunked_encoding_splits_on_four_byte_multiples() {
        let mut cmd = Command::parse(b"a=t,i=5,f=100,m=1");
        cmd.set(b'q', 2);
        let payload = vec![b'A'; CHUNK_BYTES * 2 + 8];
        let encoded = String::from_utf8(cmd.encode_chunked(&payload)).unwrap();
        let parts: Vec<&str> = encoded.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].starts_with("\x1b_Ga=t,i=5,f=100,q=2,m=1;"));
        assert!(parts[1].starts_with("\x1b_Gq=2,m=1;"));
        assert!(parts[2].starts_with("\x1b_Gq=2,m=0;"));
        let total: usize = parts.iter().map(|p| p.split_once(';').unwrap().1.len()).sum();
        assert_eq!(total, payload.len());
    }
}
