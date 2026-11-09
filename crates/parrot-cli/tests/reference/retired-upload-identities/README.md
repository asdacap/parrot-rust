# Retired upload identity assertion

`native_frames.rs` preserves the original unique-upload-ID frame test without Cargo registration. The approved upload registry removal also removes upload IDs from the protocol. The active replacement in `tests/attachments/native.rs` checks session binding, exact repeated frames, bounded chunks, terminal descriptions, and original byte content.
