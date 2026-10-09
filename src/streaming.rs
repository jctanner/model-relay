//! Bounded SSE observation. Forwarded bytes never depend on parsing success.
use serde_json::Value;

const MAX_EVENT: usize = 64 * 1024;

#[derive(Default)]
pub struct Observer {
    line: Vec<u8>,
    data: Vec<u8>,
    line_len: usize,
    last_cr: bool,
    discard: bool,
    pub error: Option<&'static str>,
}

impl Observer {
    pub fn feed(&mut self, bytes: &[u8], mut event: impl FnMut(Value)) {
        for &byte in bytes {
            if byte != b'\n' {
                self.line_len = self.line_len.saturating_add(1);
                self.last_cr = byte == b'\r';
                if !self.discard && self.line.len() < MAX_EVENT {
                    self.line.push(byte);
                } else {
                    self.discard = true;
                    self.error = Some("sse_event_too_large");
                    self.line.clear();
                    self.data.clear();
                }
                continue;
            }
            let blank = self.line_len == 0 || (self.line_len == 1 && self.last_cr);
            if blank {
                if !self.discard && !self.data.is_empty() {
                    match serde_json::from_slice(&self.data) {
                        Ok(value) => event(value),
                        Err(_) => self.error = Some("invalid_sse_json"),
                    }
                }
                self.data.clear();
                self.discard = false;
            } else if !self.discard {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if let Some(data) = self.line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if self.data.len() + data.len() + 1 > MAX_EVENT {
                        self.error = Some("sse_event_too_large");
                        self.discard = true;
                        self.data.clear();
                    } else {
                        if !self.data.is_empty() {
                            self.data.push(b'\n');
                        }
                        self.data.extend_from_slice(data);
                    }
                }
            }
            self.line.clear();
            self.line_len = 0;
            self.last_cr = false;
        }
    }
    pub fn has_pending(&self) -> bool {
        self.line_len > 0 || !self.data.is_empty() || self.discard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_boundaries_utf8_crlf_and_multiline() {
        let wire = "event: message_delta\r\ndata: {\"type\":\"message_delta\",\r\ndata: \"usage\":{\"output_tokens\":3},\"text\":\"héllo\"}\r\n\r\n";
        for width in 1..wire.len() {
            let mut observer = Observer::default();
            let mut events = vec![];
            for chunk in wire.as_bytes().chunks(width) {
                observer.feed(chunk, |e| events.push(e));
            }
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["usage"]["output_tokens"], 3);
            assert!(observer.error.is_none() && !observer.has_pending());
        }
    }
    #[test]
    fn oversized_events_recover_without_unbounded_buffering() {
        let mut observer = Observer::default();
        observer.feed(&vec![b'x'; MAX_EVENT * 3], |_| panic!("no complete event"));
        assert!(observer.line.len() <= MAX_EVENT && observer.data.len() <= MAX_EVENT);
        let mut events = vec![];
        observer.feed(b"\n\ndata: {\"type\":\"message_stop\"}\n\n", |e| {
            events.push(e)
        });
        assert_eq!(events.len(), 1);
        assert_eq!(observer.error, Some("sse_event_too_large"));
    }
}
