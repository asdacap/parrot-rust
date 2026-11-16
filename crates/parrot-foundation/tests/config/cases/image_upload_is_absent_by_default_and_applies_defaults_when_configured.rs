use crate::support::ConfigFixture;
use parrot_foundation::config::runtime::ImageUploadConfig;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let minimal = fixture
        .configured(
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: parrot-images\n",
        )
        .unwrap()
        .runtime
        .image_upload;
    let full=fixture.configured("image_upload:\n  endpoint: https://minio.example.com:9000/base\n  bucket: b.1-2\n  region: eu-west-1\n  key_prefix: images/\n  public_base_url: https://cdn.example.com\n  access_key_env: AK\n  secret_key_env: SK\n").unwrap().runtime.image_upload;
    assert!(
        fixture
            .configured("")
            .unwrap()
            .runtime
            .image_upload
            .is_none()
    );
    assert_eq!(
        minimal,
        Some(ImageUploadConfig {
            endpoint: "https://minio.example.com".into(),
            bucket: "parrot-images".into(),
            region: "us-east-1".into(),
            key_prefix: "parrot/".into(),
            public_base_url: String::new(),
            access_key_env: String::new(),
            secret_key_env: String::new()
        })
    );
    assert_eq!(
        full,
        Some(ImageUploadConfig {
            endpoint: "https://minio.example.com:9000/base".into(),
            bucket: "b.1-2".into(),
            region: "eu-west-1".into(),
            key_prefix: "images/".into(),
            public_base_url: "https://cdn.example.com".into(),
            access_key_env: "AK".into(),
            secret_key_env: "SK".into()
        })
    );
}
