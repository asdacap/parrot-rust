use super::{FetchError, GzipDecoder, ResponseBody};
use async_compression::tokio::bufread::GzipDecoder as Decoder;
use parrot_domain::contracts::async_ops::BoxFuture;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader, ReadBuf};

#[derive(Clone, Copy, Debug, Default)]
pub struct GzipStreamDecoder;

impl GzipDecoder for GzipStreamDecoder {
    fn decode(
        &self,
        compressed: Box<dyn ResponseBody>,
    ) -> Result<Box<dyn ResponseBody>, FetchError> {
        let mut decoder = Decoder::new(BufReader::new(BodyReader::new(compressed)));
        decoder.multiple_members(true);
        Ok(Box::new(GzipBody(decoder)))
    }
}

struct GzipBody(Decoder<BufReader<BodyReader>>);

impl ResponseBody for GzipBody {
    fn read<'a>(&'a mut self, output: &'a mut [u8]) -> BoxFuture<'a, Result<usize, FetchError>> {
        Box::pin(async move {
            self.0.read(output).await.map_err(|error| {
                error
                    .downcast::<FetchError>()
                    .unwrap_or_else(FetchError::Io)
            })
        })
    }
}

type ReadCompletion = (
    Box<dyn ResponseBody>,
    Box<[u8; 8192]>,
    Result<usize, FetchError>,
);

// The pending read owns its body and buffer, so cancellation drops all work in place.
struct BodyReader {
    body: Option<Box<dyn ResponseBody>>,
    buffer: Option<Box<[u8; 8192]>>,
    position: usize,
    length: usize,
    pending: Option<BoxFuture<'static, ReadCompletion>>,
}

impl BodyReader {
    fn new(body: Box<dyn ResponseBody>) -> Self {
        Self {
            body: Some(body),
            buffer: Some(Box::new([0; 8192])),
            position: 0,
            length: 0,
            pending: None,
        }
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.position == this.length {
            if this.pending.is_none() {
                let mut body = this
                    .body
                    .take()
                    .expect("body retained outside pending read");
                let mut buffer = this
                    .buffer
                    .take()
                    .expect("buffer retained outside pending read");
                this.pending = Some(Box::pin(async move {
                    let result = body.read(buffer.as_mut_slice()).await;
                    (body, buffer, result)
                }));
            }
            let (body, buffer, result) = std::task::ready!(
                this.pending
                    .as_mut()
                    .expect("pending read initialized")
                    .as_mut()
                    .poll(context)
            );
            this.body = Some(body);
            this.buffer = Some(buffer);
            this.pending = None;
            this.position = 0;
            this.length = result.map_err(io::Error::other)?;
        }
        let length = output.remaining().min(this.length - this.position);
        output.put_slice(
            &this.buffer.as_ref().expect("completed read retains buffer")
                [this.position..this.position + length],
        );
        this.position += length;
        Poll::Ready(Ok(()))
    }
}
