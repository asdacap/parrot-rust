//! Independent libc ABI fixtures and source raw-mode oracle, never used in production.

/// libc termios on Linux x86_64/aarch64 (not the shorter kernel ioctl termios).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinuxTermios {
    pub input_flags: u32,
    pub output_flags: u32,
    pub control_flags: u32,
    pub local_flags: u32,
    pub line: u8,
    pub control_characters: [u8; 32],
    pub input_speed: u32,
    pub output_speed: u32,
}

/// Darwin's 64-bit libc termios, on both Intel and Apple Silicon.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DarwinTermios {
    pub input_flags: u64,
    pub output_flags: u64,
    pub control_flags: u64,
    pub local_flags: u64,
    pub control_characters: [u8; 20],
    pub input_speed: u64,
    pub output_speed: u64,
}

pub fn make_linux_raw(mut original: LinuxTermios) -> LinuxTermios {
    original.input_flags &= !0x532;
    original.control_flags = (original.control_flags & !0x30) | 0x30;
    original.local_flags &= !0x800b;
    original.control_characters[6] = 0;
    original.control_characters[5] = 1;
    original
}

pub fn make_darwin_raw(mut original: DarwinTermios) -> DarwinTermios {
    original.input_flags &= !0x332;
    original.control_flags = (original.control_flags & !0x300) | 0x300;
    original.local_flags &= !0x588;
    original.control_characters[16] = 0;
    original.control_characters[17] = 1;
    original
}
