//! Tool-result metadata codecs for domain protocol messages.

use crate::protocol::messages::parrot::YieldedShellProcess;
use parrot_core::{Error, ToolResultMetadata};

impl ToolResultMetadata for YieldedShellProcess {
    fn encode(&self) -> Result<Vec<u8>, Error> {
        protobuf::Message::write_to_bytes(self).map_err(|error| Error::failed(error.to_string()))
    }
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        <Self as protobuf::Message>::parse_from_bytes(bytes)
            .map_err(|error| Error::failed(error.to_string()))
    }
}
