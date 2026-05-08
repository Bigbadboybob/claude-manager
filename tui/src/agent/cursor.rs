//! Opaque cursor used by `Agent::read_messages`. Encodes a generation +
//! line offset so the parser can detect cursor-vs-file mismatch after a
//! transcript rebind (e.g. `/clear`). Format `v1:<generation>:<offset>`
//! lets us evolve the encoding without breaking persisted callers.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Wire format: `v1:<generation>:<offset>`. Internal — produced by
    /// the parser and parsed back on the next call. Callers treat it
    /// as opaque.
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorParseError;

impl fmt::Display for CursorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed cursor")
    }
}
impl std::error::Error for CursorParseError {}

impl Cursor {
    pub fn new(generation: u64, offset: usize) -> Self {
        Cursor {
            raw: format!("v1:{}:{}", generation, offset),
        }
    }

    /// Parse `raw` into `(generation, offset)`. Returns `Err` only on
    /// malformed input — a `None` cursor means "from start" and is
    /// handled by `agent::resolve_cursor` before this is called.
    pub fn parse(&self) -> Result<(u64, usize), CursorParseError> {
        let mut it = self.raw.splitn(3, ':');
        let v = it.next().ok_or(CursorParseError)?;
        if v != "v1" {
            return Err(CursorParseError);
        }
        let generation: u64 = it
            .next()
            .ok_or(CursorParseError)?
            .parse()
            .map_err(|_| CursorParseError)?;
        let offset: usize = it
            .next()
            .ok_or(CursorParseError)?
            .parse()
            .map_err(|_| CursorParseError)?;
        Ok((generation, offset))
    }
}
