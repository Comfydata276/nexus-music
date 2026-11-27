//! A small HTTP/1.1 client over a socket the caller opened, for the container's own health
//! check and for the tests: it writes a request exactly as given and reads the answer's head
//! and body, without following anything.

use std::io::{self, BufRead, Read};

use crate::bounds::HEAD_MAX_BYTES;

/// The head of an answer.
#[derive(Debug, Clone)]
pub struct Head {
    pub status: u16,
    /// Each header as it came, the name in lower case.
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// Every value of the header `name` (lower case).
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.headers.iter().filter(|(have, _)| have == name).map(|(_, value)| value.as_str()).collect()
    }

    /// The one value of `name`, if it came exactly once.
    pub fn one(&self, name: &str) -> Option<&str> {
        match self.all(name)[..] {
            [only] => Some(only),
            _ => None,
        }
    }
}

fn wrong(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Reads an answer's head. `None` if the connection ended before a byte of it.
pub fn read_head(from: &mut impl BufRead) -> io::Result<Option<Head>> {
    let mut taken = 0usize;
    let mut line = Vec::new();
    let mut next = |from: &mut dyn BufRead, line: &mut Vec<u8>| -> io::Result<usize> {
        line.clear();
        let read = from.take((HEAD_MAX_BYTES - taken.min(HEAD_MAX_BYTES)) as u64).read_until(b'\n', line)?;
        taken += read;
        if read > 0 && !line.ends_with(b"\n") {
            return Err(wrong("an answer's head too long, or cut"));
        }
        Ok(read)
    };
    if next(from, &mut line)? == 0 {
        return Ok(None);
    }
    let status_line = String::from_utf8(line.clone()).map_err(|_| wrong("a status line not in UTF-8"))?;
    let mut words = status_line.split(' ');
    if !words.next().is_some_and(|version| version.starts_with("HTTP/1.")) {
        return Err(wrong("not an HTTP/1 answer"));
    }
    let status = words.next().and_then(|code| code.parse::<u16>().ok()).ok_or_else(|| wrong("no status"))?;
    let mut headers = Vec::new();
    loop {
        if next(from, &mut line)? == 0 {
            return Err(wrong("an answer's head cut"));
        }
        let text = String::from_utf8(line.clone()).map_err(|_| wrong("a header not in UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break;
        }
        let (name, value) = text.split_once(':').ok_or_else(|| wrong("a header without a colon"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Some(Head { status, headers }))
}

/// The body that follows `head`: as long as its `Content-Length` says, **failing if it ends
/// before then**, or to the end of the connection if it gives none. (The health check asks
/// with `Connection: close`, so either is the whole body.)
pub fn body<'a>(head: &Head, from: impl BufRead + 'a) -> io::Result<Box<dyn Read + 'a>> {
    match head.all("content-length")[..] {
        [] => Ok(Box::new(from)),
        [length] => {
            let length: u64 = length.parse().map_err(|_| wrong("a Content-Length that is not a number"))?;
            Ok(Box::new(Exact { from, left: length }))
        }
        _ => Err(wrong("more than one Content-Length")),
    }
}

/// Exactly `left` bytes more of `from`, or an error.
struct Exact<R> {
    from: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let most = into.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let read = self.from.read(&mut into[..most])?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a body shorter than its Content-Length"));
        }
        self.left -= read as u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_and_its_body() {
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nContent-Length: 5\r\nCache-Control: no-store\r\n\r\nhello and more";
        let mut from = &answer[..];
        let head = read_head(&mut from).unwrap().unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.one("cache-control"), Some("no-store"));
        let mut text = String::new();
        body(&head, from).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
    }

    #[test]
    fn a_body_shorter_than_it_said_is_an_error() {
        let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nhello";
        let mut from = &answer[..];
        let head = read_head(&mut from).unwrap().unwrap();
        let mut text = Vec::new();
        assert_eq!(body(&head, from).unwrap().read_to_end(&mut text).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn nothing_at_all_is_none_and_a_cut_head_is_an_error() {
        assert!(read_head(&mut &b""[..]).unwrap().is_none());
        assert!(read_head(&mut &b"HTTP/1.1 200 OK\r\nx: y"[..]).is_err());
        assert!(read_head(&mut &b"SSH-2.0\r\n\r\n"[..]).is_err());
    }
}
