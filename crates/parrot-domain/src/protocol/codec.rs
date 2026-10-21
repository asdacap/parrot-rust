//! Tonic's proven A001.2 codec adapter for generated rust-protobuf messages.

use bytes::{Buf, BufMut};
use protobuf::Message;
use std::marker::PhantomData;
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// A Tonic codec over the selected lossless rust-protobuf runtime.
pub struct RustProtobufCodec<E, D>(PhantomData<(E, D)>);

impl<E, D> Default for RustProtobufCodec<E, D> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// Encodes a generated message without reconstructing its known fields.
pub struct ProtobufEncoder<T>(PhantomData<T>);

/// Decodes complete Tonic message payloads into generated messages.
pub struct ProtobufDecoder<T>(PhantomData<T>);

impl<E: Message + 'static, D: Message + 'static> Codec for RustProtobufCodec<E, D> {
    type Encode = E;
    type Decode = D;
    type Encoder = ProtobufEncoder<E>;
    type Decoder = ProtobufDecoder<D>;

    fn encoder(&mut self) -> Self::Encoder {
        ProtobufEncoder(PhantomData)
    }

    fn decoder(&mut self) -> Self::Decoder {
        ProtobufDecoder(PhantomData)
    }
}

impl<T: Message> Encoder for ProtobufEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: T, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        let bytes = item
            .write_to_bytes()
            .map_err(|error| Status::internal(error.to_string()))?;
        dst.put_slice(&bytes);
        Ok(())
    }
}

impl<T: Message> Decoder for ProtobufDecoder<T> {
    type Item = T;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<T>, Status> {
        let bytes = src.copy_to_bytes(src.remaining());
        T::parse_from_bytes(&bytes)
            .map(Some)
            .map_err(|error| Status::internal(error.to_string()))
    }
}
