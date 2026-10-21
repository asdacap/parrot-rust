use crate::support::Context;

#[test]
fn source_assertions() {
    for (yaml, expected) in [
        ("image_upload: null", "image_upload must be a mapping"),
        (
            "image_upload:\n  bucket: bkt",
            "image_upload.endpoint must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: http://minio.example.com\n  bucket: bkt",
            "image_upload.endpoint must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: ftp://minio.example.com\n  bucket: bkt",
            "image_upload.endpoint must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: not a url\n  bucket: bkt",
            "image_upload.endpoint must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com/?x=1\n  bucket: bkt",
            "image_upload.endpoint must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: https://user@minio.example.com\n  bucket: bkt",
            "image_upload.endpoint must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: bkt\n  public_base_url: http://cdn.example.com",
            "image_upload.public_base_url must be an https URL",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com",
            "image_upload.bucket must be a valid S3 bucket name",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: ab",
            "image_upload.bucket must be a valid S3 bucket name",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: Bucket",
            "image_upload.bucket must be a valid S3 bucket name",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: -bucket",
            "image_upload.bucket must be a valid S3 bucket name",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: bkt\n  key_prefix: 'a b/'",
            "image_upload.key_prefix may contain only letters, digits, '/', '_', '.', and '-'",
        ),
        (
            "image_upload:\n  endpoint: https://minio.example.com\n  bucket: bkt\n  secret_key: literal",
            "image_upload contains an unsupported key",
        ),
    ] {
        let context = Context::new();
        let error = context.configured(&format!("{yaml}\n")).unwrap_err();
        assert_eq!(error.0, expected);
    }
}
