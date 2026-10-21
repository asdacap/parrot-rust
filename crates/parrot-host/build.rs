#[path = "build/web_assets.rs"]
mod web_assets;

fn main() -> std::io::Result<()> {
    web_assets::run()
}
