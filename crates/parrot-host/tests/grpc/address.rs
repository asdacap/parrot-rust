use parrot_host::grpc::address::TransportAddress;
use std::{net::SocketAddr, path::Path};

#[test]
fn transport_addresses_are_explicit_and_authority_only() {
    for input in [
        "",
        "  ",
        "127.0.0.1:8710",
        "http:localhost",
        "https:/localhost",
        "unix:",
        "unix:relative.sock",
        "UNIX:/tmp/parrot.sock",
        "ftp://127.0.0.1:21",
        "http://user@localhost:8710",
        "http://:secret@localhost:8710",
        "http://localhost:8710?",
        "http://localhost:8710#",
        "http://localhost:8710/path",
    ] {
        assert!(TransportAddress::parse(input).is_err(), "accepted {input}");
    }
    let unix = TransportAddress::parse("unix:/tmp/control/../parrot.sock").unwrap();
    assert_eq!(unix.unix_path(), Some(Path::new("/tmp/parrot.sock")));
    assert!(!unix.is_tcp());
    assert!(!unix.is_external());
    assert!(unix.tcp_url().is_none());
    assert!(unix.socket_addr().is_err());
    assert_eq!(unix.to_string(), "unix:/tmp/parrot.sock");
    let tcp = TransportAddress::parse("http://localhost:8710/").unwrap();
    assert!(tcp.is_tcp());
    assert!(tcp.unix_path().is_none());
    assert_eq!(tcp.to_string(), "http://localhost:8710");
    assert_eq!(
        tcp.socket_addr().unwrap(),
        "127.0.0.1:8710".parse().unwrap()
    );
    assert!(TransportAddress::parse("https://example.test:8710").is_ok());
}

#[test]
fn loopback_classification_never_resolves_dns() {
    for value in [
        "http://localhost:8710",
        "http://LOCALHOST:8710",
        "http://127.0.0.1:8710",
        "http://127.255.255.254:8710",
        "https://[::1]:8710",
    ] {
        assert!(
            !TransportAddress::parse(value).unwrap().is_external(),
            "{value}"
        );
    }
    for value in [
        "http://example.test:8710",
        "http://localhost.example.test:8710",
        "http://192.0.2.1:8710",
        "http://0.0.0.0:8710",
        "http://[::]:8710",
    ] {
        assert!(
            TransportAddress::parse(value).unwrap().is_external(),
            "{value}"
        );
    }
}

#[test]
fn server_address_requires_localhost_or_literal_ip() {
    let ipv6 = TransportAddress::parse("https://[::1]:8710").unwrap();
    assert_eq!(
        ipv6.socket_addr().unwrap(),
        "[::1]:8710".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(
        TransportAddress::parse("http://localhost")
            .unwrap()
            .socket_addr()
            .unwrap()
            .port(),
        80
    );
    assert_eq!(
        TransportAddress::parse("https://localhost")
            .unwrap()
            .socket_addr()
            .unwrap()
            .port(),
        443
    );
    assert!(
        TransportAddress::parse("https://example.test")
            .unwrap()
            .socket_addr()
            .is_err()
    );
}

#[test]
fn only_external_plaintext_listening_requires_the_unsafe_flag() {
    let external = TransportAddress::parse("http://192.0.2.1:8710").unwrap();
    assert!(external.validate_external_listen(false).is_err());
    assert!(external.validate_external_listen(true).is_ok());
    for value in [
        "http://127.0.0.1:8710",
        "https://192.0.2.1:8710",
        "unix:/tmp/parrot.sock",
    ] {
        assert!(
            TransportAddress::parse(value)
                .unwrap()
                .validate_external_listen(false)
                .is_ok()
        );
    }
}

#[test]
fn unix_path_normalization_does_not_walk_above_root() {
    assert_eq!(
        TransportAddress::parse("unix:/../../tmp//./parrot.sock")
            .unwrap()
            .unix_path(),
        Some(Path::new("/tmp/parrot.sock"))
    );
}
