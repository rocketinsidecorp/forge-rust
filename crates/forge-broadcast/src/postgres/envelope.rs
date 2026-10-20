//! The notification format, identical to Forge-Go's.
//!
//! PostgreSQL limits a `NOTIFY` payload to less than 8000 bytes, so a message
//! is sent as one or more chunks, each its own notification, all queued in
//! one transaction. Notifications of a transaction are delivered together and
//! in order, which lets the receiver reassemble them without timers.
//!
//! A chunk is one line of ASCII fields separated by single spaces:
//!
//! ```text
//! v1 ORIGIN MESSAGE-ID INDEX COUNT CHANNEL-KEY BASE64-DATA
//! ```
//!
//! `ORIGIN` identifies the publishing broker (`-` for a transactional
//! publish), `MESSAGE-ID` is random hex, `INDEX`/`COUNT` number the chunks,
//! `CHANNEL-KEY` is [`Channel::key`](crate::Channel::key) (no spaces) and the
//! data is padded standard base64, which keeps arbitrary bytes safe inside a
//! notification payload.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};

const VERSION: &str = "v1";
/// Message bytes per notification: 4500 bytes become 6000 base64
/// characters, leaving ample room under 8000 bytes for the header and the
/// longest channel key (about 140 bytes).
pub(crate) const CHUNK_BYTES: usize = 4500;
/// The largest payload sent, a safety margin below PostgreSQL's limit.
pub(crate) const MAX_NOTIFY_BYTES: usize = 7500;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chunk {
    pub(crate) origin: String,
    pub(crate) id: String,
    pub(crate) index: usize,
    pub(crate) count: usize,
    pub(crate) key: String,
    pub(crate) data: Vec<u8>,
}

/// A notification that does not parse or does not fit its message.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Malformed;

impl Chunk {
    pub(crate) fn encode(&self) -> String {
        format!(
            "{VERSION} {} {} {} {} {} {}",
            self.origin,
            self.id,
            self.index,
            self.count,
            self.key,
            STANDARD.encode(&self.data)
        )
    }

    pub(crate) fn parse(payload: &str, max_chunks: usize) -> Result<Self, Malformed> {
        let fields: Vec<&str> = payload.split(' ').collect();
        let [version, origin, id, index, count, key, data] = fields[..] else {
            return Err(Malformed);
        };
        if version != VERSION || origin.is_empty() || id.is_empty() || key.is_empty() {
            return Err(Malformed);
        }
        let index: usize = index.parse().map_err(|_| Malformed)?;
        let count: usize = count.parse().map_err(|_| Malformed)?;
        if count == 0 || count > max_chunks || index >= count {
            return Err(Malformed);
        }
        let data = STANDARD.decode(data).map_err(|_| Malformed)?;
        if data.len() > CHUNK_BYTES {
            return Err(Malformed);
        }
        Ok(Self {
            origin: origin.to_owned(),
            id: id.to_owned(),
            index,
            count,
            key: key.to_owned(),
            data,
        })
    }
}

/// Cuts `data` into chunks of at most [`CHUNK_BYTES`]; empty data is one
/// empty chunk, so every message produces at least one notification.
pub(crate) fn split(origin: &str, id: &str, key: &str, data: &[u8]) -> Vec<Chunk> {
    let count = data.len().div_ceil(CHUNK_BYTES).max(1);
    (0..count)
        .map(|index| {
            let start = index * CHUNK_BYTES;
            let end = data.len().min(start + CHUNK_BYTES);
            Chunk {
                origin: origin.to_owned(),
                id: id.to_owned(),
                index,
                count,
                key: key.to_owned(),
                data: data.get(start..end).unwrap_or_default().to_vec(),
            }
        })
        .collect()
}

/// A finished message.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Assembled {
    pub(crate) key: String,
    pub(crate) data: Vec<u8>,
}

struct Partial {
    key: String,
    parts: Vec<Vec<u8>>,
    bytes: usize,
    started: Instant,
}

/// Reassembles chunked messages with bounded memory: at most `max_pending`
/// partial messages of at most `max_bytes` each, none older than
/// `lifetime`. Used by the single listener; needs no locking.
pub(crate) struct Assembler {
    max_pending: usize,
    max_bytes: usize,
    lifetime: Duration,
    pending: HashMap<String, Partial>,
    order: VecDeque<String>,
}

impl Assembler {
    pub(crate) fn new(max_pending: usize, max_bytes: usize, lifetime: Duration) -> Self {
        Self {
            max_pending,
            max_bytes,
            lifetime,
            pending: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Takes one chunk; returns the message it completed, if any.
    /// Out-of-order, oversized or inconsistent chunks discard their message.
    pub(crate) fn add(
        &mut self,
        chunk: Chunk,
        now: Instant,
    ) -> Result<Option<Assembled>, Malformed> {
        self.sweep(now);
        if chunk.count == 1 {
            return if chunk.data.len() > self.max_bytes {
                Err(Malformed)
            } else {
                Ok(Some(Assembled {
                    key: chunk.key,
                    data: chunk.data,
                }))
            };
        }
        let id = format!("{}/{}", chunk.origin, chunk.id);
        if chunk.index == 0 {
            self.drop_pending(&id);
            while self.pending.len() >= self.max_pending {
                let Some(oldest) = self.order.front().cloned() else {
                    break;
                };
                self.drop_pending(&oldest);
            }
            self.pending.insert(
                id.clone(),
                Partial {
                    key: chunk.key.clone(),
                    parts: Vec::with_capacity(chunk.count),
                    bytes: 0,
                    started: now,
                },
            );
            self.order.push_back(id.clone());
        }
        let fits = self.pending.get(&id).is_some_and(|partial| {
            chunk.index == partial.parts.len()
                && chunk.key == partial.key
                && partial.bytes + chunk.data.len() <= self.max_bytes
        });
        if !fits {
            self.drop_pending(&id);
            return Err(Malformed);
        }
        let last = chunk.index + 1 == chunk.count;
        if let Some(partial) = self.pending.get_mut(&id) {
            partial.bytes += chunk.data.len();
            partial.parts.push(chunk.data);
        }
        if !last {
            return Ok(None);
        }
        let Some(partial) = self.pending.remove(&id) else {
            return Err(Malformed);
        };
        self.order.retain(|queued| *queued != id);
        Ok(Some(Assembled {
            key: partial.key,
            data: partial.parts.concat(),
        }))
    }

    fn drop_pending(&mut self, id: &str) {
        if self.pending.remove(id).is_some() {
            self.order.retain(|queued| queued != id);
        }
    }

    fn sweep(&mut self, now: Instant) {
        while let Some(oldest) = self.order.front() {
            let expired = self
                .pending
                .get(oldest)
                .is_none_or(|partial| now.duration_since(partial.started) >= self.lifetime);
            if !expired {
                return;
            }
            if let Some(oldest) = self.order.pop_front() {
                self.pending.remove(&oldest);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "global/posts";

    fn assembler() -> Assembler {
        Assembler::new(4, 3 * CHUNK_BYTES, Duration::from_secs(30))
    }

    #[test]
    fn chunks_round_trip_and_match_forge_go() {
        let chunk = Chunk {
            origin: "0011223344556677".to_owned(),
            id: "8899aabbccddeeff".to_owned(),
            index: 0,
            count: 1,
            key: KEY.to_owned(),
            data: b"<turbo-stream>".to_vec(),
        };
        let encoded = chunk.encode();
        assert_eq!(
            encoded,
            "v1 0011223344556677 8899aabbccddeeff 0 1 global/posts PHR1cmJvLXN0cmVhbT4="
        );
        assert_eq!(Chunk::parse(&encoded, 1), Ok(chunk));
    }

    #[test]
    fn malformed_notifications_are_refused() {
        let big = STANDARD.encode(vec![0_u8; CHUNK_BYTES + 1]);
        for payload in [
            "",
            "v1",
            "v2 o i 0 1 global/posts AA==",
            "v1 o i 0 1 global/posts AA== extra",
            "v1  i 0 1 global/posts AA==",
            "v1 o i x 1 global/posts AA==",
            "v1 o i 0 0 global/posts AA==",
            "v1 o i 1 1 global/posts AA==",
            "v1 o i 0 99 global/posts AA==",
            "v1 o i 0 1 global/posts not-base64!",
            "v1 o i 0 1 global/posts AA",
            &format!("v1 o i 0 1 global/posts {big}"),
        ] {
            assert_eq!(Chunk::parse(payload, 8), Err(Malformed), "{payload:?}");
        }
    }

    #[test]
    fn split_messages_reassemble_in_order() {
        let data: Vec<u8> = (0..(2 * CHUNK_BYTES + 10))
            .map(|i| (i % 251) as u8)
            .collect();
        let chunks = split("origin", "id", KEY, &data);
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode().len() <= MAX_NOTIFY_BYTES)
        );
        assert_eq!(split("o", "i", KEY, b"").len(), 1);

        let mut assembler = assembler();
        let now = Instant::now();
        let mut done = None;
        for chunk in chunks {
            done = assembler.add(chunk, now).expect("in order");
        }
        assert_eq!(
            done,
            Some(Assembled {
                key: KEY.to_owned(),
                data
            })
        );
        assert_eq!(assembler.pending(), 0);
    }

    #[test]
    fn out_of_order_or_inconsistent_chunks_discard_their_message() {
        let data = vec![7_u8; 2 * CHUNK_BYTES];
        let chunks = split("o", "i", KEY, &data);
        let now = Instant::now();

        let mut skipped = assembler();
        assert_eq!(skipped.add(chunks[1].clone(), now), Err(Malformed));

        let mut moved = assembler();
        assert_eq!(moved.add(chunks[0].clone(), now), Ok(None));
        let mut other_channel = chunks[1].clone();
        other_channel.key = "global/comments".to_owned();
        assert_eq!(moved.add(other_channel, now), Err(Malformed));
        assert_eq!(moved.pending(), 0);
    }

    #[test]
    fn buffers_are_bounded_in_count_size_and_age() {
        let now = Instant::now();
        let mut assembler = assembler();
        for id in 0..10 {
            let chunks = split("o", &id.to_string(), KEY, &vec![0; 2 * CHUNK_BYTES]);
            assert_eq!(assembler.add(chunks[0].clone(), now), Ok(None));
            assert!(assembler.pending() <= 4);
        }
        // Old partial messages expire.
        let later = now + Duration::from_secs(31);
        let fresh = split("o", "fresh", KEY, b"x");
        assert!(assembler.add(fresh[0].clone(), later).is_ok());
        assert_eq!(assembler.pending(), 0);

        // A message larger than the byte budget is discarded.
        let mut small = Assembler::new(4, CHUNK_BYTES, Duration::from_secs(30));
        let chunks = split("o", "big", KEY, &vec![0; 2 * CHUNK_BYTES]);
        assert_eq!(small.add(chunks[0].clone(), now), Ok(None));
        assert_eq!(small.add(chunks[1].clone(), now), Err(Malformed));
        let single = split("o", "one", KEY, &vec![0; CHUNK_BYTES]);
        let mut tiny = Assembler::new(4, 10, Duration::from_secs(30));
        assert_eq!(tiny.add(single[0].clone(), now), Err(Malformed));
    }
}
