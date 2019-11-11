//! Process HTTP connections on the server.

use async_std::io::{self, BufReader};
use async_std::prelude::*;
use async_std::task::{Context, Poll};
use futures_core::ready;
use futures_io::AsyncRead;
use http::{Request, Response, Version};

use std::pin::Pin;

use crate::{Body, Exception, MAX_HEADERS};

/// A streaming HTTP encoder.
///
/// This is returned from [`encode`].
#[derive(Debug)]
pub struct Encoder<R: AsyncRead> {
    /// Keep track how far we've indexed into the headers + body.
    cursor: usize,
    /// HTTP headers to be sent.
    headers: Vec<u8>,
    /// Check whether we're done sending headers.
    headers_done: bool,
    /// HTTP body to be sent.
    body: Body<R>,
    /// Check whether we're done with the body.
    body_done: bool,
    /// Keep track of how many bytes have been read from the body stream.
    body_bytes_read: usize,
}

impl<R: AsyncRead> Encoder<R> {
    /// Create a new instance.
    pub(crate) fn new(headers: Vec<u8>, body: Body<R>) -> Self {
        Self {
            body,
            headers,
            cursor: 0,
            headers_done: false,
            body_done: false,
            body_bytes_read: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Encoder<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Send the headers. As long as the headers aren't fully sent yet we
        // keep sending more of the headers.
        let mut bytes_read = 0;
        if !self.headers_done {
            let len = std::cmp::min(self.headers.len() - self.cursor, buf.len());
            let range = self.cursor..self.cursor + len;
            buf[0..len].copy_from_slice(&mut self.headers[range]);
            self.cursor += len;
            if self.cursor == self.headers.len() {
                self.headers_done = true;
            }
            bytes_read += len;
        }

        if !self.body_done {
            let n = ready!(Pin::new(&mut self.body).poll_read(cx, &mut buf[bytes_read..]))?;
            bytes_read += n;
            self.body_bytes_read += n;
            if bytes_read == 0 {
                self.body_done = true;
            }
        }

        Poll::Ready(Ok(bytes_read as usize))
    }
}

/// Encode an HTTP request on the server.
// TODO: return a reader in the response
pub async fn encode<R>(res: Response<Body<R>>) -> io::Result<Encoder<R>>
where
    R: AsyncRead,
{
    let mut buf: Vec<u8> = vec![];

    let reason = res.status().canonical_reason().unwrap();
    let status = res.status();
    write!(&mut buf, "HTTP/1.1 {} {}\r\n", status.as_str(), reason).await?;

    // If the body isn't streaming, we can set the content-length ahead of time. Else we need to
    // send all items in chunks.
    if let Some(len) = res.body().len() {
        write!(&mut buf, "Content-Length: {}\r\n", len).await?;
    } else {
        write!(&mut buf, "Transfer-Encoding: chunked\r\n").await?;
        panic!("chunked encoding is not implemented yet");
        // See: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Transfer-Encoding
        //      https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Trailer
    }

    for (header, value) in res.headers() {
        write!(
            &mut buf,
            "{}: {}\r\n",
            header.as_str(),
            value.to_str().unwrap()
        )
        .await?;
    }

    write!(&mut buf, "\r\n").await?;
    Ok(Encoder::new(buf, res.into_body()))
}

/// Decode an HTTP request on the server.
pub async fn decode<R>(reader: R) -> Result<Option<Request<Body<BufReader<R>>>>, Exception>
where
    R: AsyncRead + Unpin + Send,
{
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut httparse_req = httparse::Request::new(&mut headers);

    // Keep reading bytes from the stream until we hit the end of the stream.
    loop {
        let bytes_read = reader.read_until(b'\n', &mut buf).await?;
        // No more bytes are yielded from the stream.
        if bytes_read == 0 {
            return Ok(None);
        }

        // We've hit the end delimiter of the stream.
        let idx = buf.len() - 1;
        if idx >= 3 && &buf[idx - 3..=idx] == b"\r\n\r\n" {
            break;
        }
    }

    // Convert our header buf into an httparse instance, and validate.
    let status = httparse_req.parse(&buf)?;
    if status.is_partial() {
        dbg!(String::from_utf8(buf).unwrap());
        return Err("Malformed HTTP head".into());
    }

    // Convert httparse headers + body into a `http::Request` type.
    let mut req = Request::builder();
    for header in httparse_req.headers.iter() {
        req.header(header.name, header.value);
    }
    if let Some(method) = httparse_req.method {
        req.method(method);
    }
    if let Some(path) = httparse_req.path {
        req.uri(path);
    }
    if let Some(version) = httparse_req.version {
        req.version(match version {
            1 => Version::HTTP_11,
            _ => return Err("Unsupported HTTP version".into()),
        });
    }

    // Process the body if `Content-Length` was passed.
    let body = match httparse_req
        .headers
        .iter()
        .find(|h| h.name == "Content-Length")
    {
        Some(_header) => Body::new(reader), // TODO: use the header value
        None => Body::empty(),
    };

    // Return the request.
    Ok(Some(req.body(body)?))
}