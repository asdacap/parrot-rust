use parrot_provider::{
    http_stream::Endpoint,
    image_generation::{
        signer::{AwsV4Signer, sha256},
        transport::ImageHttpRequest,
    },
};

#[test]
fn signs_the_three_published_s3_examples() {
    for (method, url, headers, body, signed, signature) in [
        (
            "GET",
            "https://examplebucket.s3.amazonaws.com/test.txt",
            "Range: bytes=0-9",
            "",
            "host;range;x-amz-content-sha256;x-amz-date",
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        ),
        (
            "PUT",
            "https://examplebucket.s3.amazonaws.com/test$file.text",
            "Date: Fri, 24 May 2013 00:00:00 GMT|x-amz-storage-class: REDUCED_REDUNDANCY",
            "Welcome to Amazon S3.",
            "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class",
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
        ),
        (
            "GET",
            "https://examplebucket.s3.amazonaws.com/?lifecycle",
            "",
            "",
            "host;x-amz-content-sha256;x-amz-date",
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
        ),
    ] {
        let headers = headers
            .split('|')
            .filter(|h| !h.is_empty())
            .map(|h| {
                let (k, v) = h.split_once(": ").unwrap();
                (k.into(), v.into())
            })
            .collect();
        let mut request = ImageHttpRequest {
            method: method.into(),
            endpoint: Endpoint::parse(url).unwrap(),
            headers,
            content_type: Some("ignored/content-header".into()),
            body: body.as_bytes().to_vec(),
        };
        let hash = sha256(body.as_bytes());
        AwsV4Signer::new(
            "AKIAIOSFODNN7EXAMPLE".into(),
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            "us-east-1".into(),
        )
        .sign(&mut request, &hash, "20130524T000000Z")
        .unwrap();
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .unwrap()
                .1
                .as_str()
        };
        assert_eq!(
            header("Authorization"),
            format!(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders={signed}, Signature={signature}"
            )
        );
        assert_eq!(header("x-amz-date"), "20130524T000000Z");
        assert_eq!(header("x-amz-content-sha256"), hash);
    }
}
#[test]
fn query_sorting_escapes_plus_once_and_content_headers_are_not_signed() {
    let signer = AwsV4Signer::new("a".into(), "s".into(), "eu-west-1".into());
    let signed = |url: &str| {
        let mut request = ImageHttpRequest {
            method: "GET".into(),
            endpoint: Endpoint::parse(url).unwrap(),
            headers: vec![
                ("X-Test".into(), "  a  b  ".into()),
                ("X-Test".into(), " c ".into()),
            ],
            body: vec![],
            content_type: Some("image/png".into()),
        };
        signer
            .sign(&mut request, &sha256(&[]), "20200102T030405Z")
            .unwrap();
        request.headers.last().unwrap().1.clone()
    };
    let first = signed("https://s3.test:9000/a%20b/%2F?z=+&a=2&a=1&lifecycle");
    let second = signed("https://s3.test:9000/a%20b/%2F?lifecycle=&a=1&a=2&z=%2B");
    assert_eq!(first, second);
    assert!(first.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-test,"));
    assert!(!first.contains("content-type"));
}

#[test]
fn pinned_source_native_uri_vectors_preserve_invalid_utf8_and_host_override() {
    for (url, headers, signature) in [
        (
            "https://s3.example.test:9443/b/%FF/%C3%28?x=%FF&y=%C3%28&same=a+b&same=a%20b",
            vec![],
            "4ecc5e63f31867ed98fb8c34543c01cbcd136ec837bfaa78fc7a6e1e678377c2",
        ),
        (
            "https://s3.example.test:9443/b/a%2Fb?z=3&a=2&a=1",
            vec![("Host".into(), "override.example.test:8443".into())],
            "06275b254b2d50ffbb01a51c31af808994ebd8d6695fd2d6f6af309fbdcf8436",
        ),
    ] {
        let mut request = ImageHttpRequest {
            method: "GET".into(),
            endpoint: Endpoint::parse(url).unwrap(),
            headers,
            content_type: None,
            body: vec![],
        };
        AwsV4Signer::new(
            "synthetic-access".into(),
            "synthetic-secret".into(),
            "eu-west-1".into(),
        )
        .sign(&mut request, &sha256(&[]), "20260102T030405Z")
        .unwrap();
        assert!(
            request
                .headers
                .last()
                .unwrap()
                .1
                .ends_with(&format!("Signature={signature}")),
            "{}",
            request.headers.last().unwrap().1
        );
    }
}

#[test]
fn all_source_uri_constructor_vectors_match_actual_request_uri_and_signature() {
    use parrot_provider::http_stream::endpoint_url;
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/source/uri-cases-v1/inputs.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/source/uri-cases-v1/expected.json")).unwrap();
    let signer = AwsV4Signer::new(
        "synthetic-access".into(),
        "synthetic-secret".into(),
        "eu-west-1".into(),
    );
    let mut signed = 0;
    let mut denied = 0;
    for input in inputs["cases"].as_array().unwrap() {
        let family = input["family"].as_str().unwrap();
        if family == "response_read_order" {
            continue;
        }
        let id = input["case_id"].as_str().unwrap();
        let expected = &oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["case_id"] == id)
            .unwrap()["observation"];
        let endpoint = if family == "endpoint_policy" {
            endpoint_url(
                input["base_url"].as_str().unwrap(),
                input["endpoint"].as_str().unwrap(),
                input["allow_local"].as_bool().unwrap(),
                input["allow_remote"].as_bool().unwrap(),
            )
        } else {
            Ok(Endpoint::parse(input["url"].as_str().unwrap()).unwrap())
        };
        if !expected["error"].is_null() {
            assert_eq!(
                endpoint.unwrap_err().to_string(),
                expected["error"]["message"].as_str().unwrap(),
                "{id}"
            );
            assert_eq!(expected["request_count"], 0, "{id}");
            denied += 1;
            continue;
        }
        let endpoint = endpoint.unwrap();
        let authority = endpoint.authority().to_owned();
        let host = endpoint.as_url().host_str().unwrap().to_owned();
        let mut headers = vec![];
        if let Some(header) = input["host_header"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            headers.push(("Host".into(), header.into()));
        }
        let mut request = ImageHttpRequest {
            method: "GET".into(),
            endpoint,
            headers,
            content_type: None,
            body: vec![],
        };
        signer
            .sign(&mut request, &sha256(&[]), "20260102T030405Z")
            .unwrap();
        let uri = &expected["request_uri"];
        assert_eq!(
            request.endpoint.as_str(),
            uri["absolute_uri"].as_str().unwrap(),
            "{id}"
        );
        assert_eq!(
            request.endpoint.as_url().path(),
            uri["absolute_path"].as_str().unwrap(),
            "{id}"
        );
        let query = request
            .endpoint
            .as_url()
            .query()
            .map(|value| format!("?{value}"))
            .unwrap_or_default();
        let fragment = request
            .endpoint
            .as_url()
            .fragment()
            .map(|value| format!("#{value}"))
            .unwrap_or_default();
        assert_eq!(query, uri["query"].as_str().unwrap(), "{id}");
        assert_eq!(fragment, uri["fragment"].as_str().unwrap(), "{id}");
        assert_eq!(
            request.endpoint.authority(),
            authority,
            "authority identity changed: {id}"
        );
        assert_eq!(
            request.endpoint.as_url().host_str(),
            Some(host.as_str()),
            "host changed: {id}"
        );
        assert_eq!(
            request.endpoint.as_url().host_str().unwrap(),
            uri["host"].as_str().unwrap(),
            "{id}"
        );
        assert_eq!(
            request.endpoint.as_url().scheme(),
            uri["scheme"].as_str().unwrap(),
            "{id}"
        );
        assert_eq!(
            u64::from(request.endpoint.as_url().port_or_known_default().unwrap()),
            uri["port"].as_u64().unwrap(),
            "{id}"
        );
        assert_eq!(
            request.endpoint.as_url().port().is_none(),
            uri["is_default_port"].as_bool().unwrap(),
            "{id}"
        );
        for (key, value) in expected["signed_headers"].as_object().unwrap() {
            let actual = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .unwrap();
            assert_eq!(&actual.1, value.as_str().unwrap(), "{id}: {key}");
        }
        assert_eq!(
            request.endpoint.as_str(),
            expected["sent_uri"]["absolute_uri"].as_str().unwrap(),
            "{id}"
        );
        signed += 1;
    }
    assert_eq!((signed, denied), (37, 6));
}

#[test]
fn original_source_case58_uses_normalized_dispatch_uri_and_same_signature() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/source/inputs.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/source/expected.json")).unwrap();
    let input = inputs["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == "signer-escaped-2")
        .unwrap();
    let expected = &oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["case_id"] == "signer-escaped-2")
        .unwrap()["observation"];
    let mut headers = vec![];
    for header in input["headers"].as_array().unwrap() {
        for value in header["values"].as_array().unwrap() {
            headers.push((
                header["name"].as_str().unwrap().to_owned(),
                value.as_str().unwrap().to_owned(),
            ));
        }
    }
    let body = input["body_hex"]
        .as_str()
        .unwrap()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let mut request = ImageHttpRequest {
        method: input["method"].as_str().unwrap().into(),
        endpoint: Endpoint::parse(input["url"].as_str().unwrap()).unwrap(),
        headers,
        content_type: None,
        body,
    };
    let payload_hash = sha256(&request.body);
    AwsV4Signer::new(
        input["access_key"].as_str().unwrap().into(),
        input["secret_key"].as_str().unwrap().into(),
        input["region"].as_str().unwrap().into(),
    )
    .sign(
        &mut request,
        &payload_hash,
        input["amz_date"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(request.endpoint.as_str(), expected["url"].as_str().unwrap());
    assert_eq!(
        request.endpoint.as_str(),
        "https://s3.example.test/b/~/%2f?b=%2B&a=%2520&a=&=x"
    );
    assert_eq!(
        request
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .unwrap()
            .1,
        expected["headers"]["authorization"].as_str().unwrap()
    );
}
