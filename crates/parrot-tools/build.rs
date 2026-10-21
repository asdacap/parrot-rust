#[path = "build/native_pty.rs"]
mod native_pty;

fn main() {
    native_pty::build();
}
