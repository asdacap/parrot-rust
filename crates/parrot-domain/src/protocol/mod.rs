//! Lossless protobuf message and RPC bindings for the pinned source schemas.
//!
//! Generated messages use the content-pinned A001.2 rust-protobuf fork. Keep
//! `special_fields` intact when mutating or forwarding generated values.

pub mod codec;

/// Generated protobuf message types. Module names match schema filenames.
pub mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages/mod.rs"));
}

/// Descriptor-driven Tonic service/client bindings for all source RPC methods.
pub mod bindings {
    /// `parrot.v1.Parrot` service bindings.
    pub mod parrot {
        include!(concat!(env!("OUT_DIR"), "/bindings/parrot.v1.Parrot.rs"));
    }

    /// `parrot.web.v1.ParrotWeb` service bindings.
    pub mod web {
        include!(concat!(
            env!("OUT_DIR"),
            "/bindings/parrot.web.v1.ParrotWeb.rs"
        ));
    }
}
