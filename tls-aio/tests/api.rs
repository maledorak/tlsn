//! Assorted public API tests.
use std::cell::RefCell;
use std::convert::TryFrom;
#[cfg(feature = "tls12")]
use std::convert::TryInto;
use std::fmt;
use std::io::{self, IoSlice, Read, Write};
use std::mem;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use futures::{AsyncWriteExt, AsyncWrite};
use log;

use tls_aio::client::ResolvesClientCert;
#[cfg(feature = "quic")]
use tls_aio::quic::{self, ClientQuicExt, QuicExt, ServerQuicExt};
use tls_aio::{sign, ConnectionCommon, Error, KeyLog, SideData};
use tls_aio::{CipherSuite, ProtocolVersion, SignatureScheme};
use tls_aio::{ClientConfig, ClientConnection};
//use tls_aio::{Stream, StreamOwned};
use tls_aio::{SupportedCipherSuite, ALL_CIPHER_SUITES};

use rustls::server::{AllowAnyAnonymousOrAuthenticatedClient, ClientHello, ResolvesServerCert};
use rustls::{ServerConfig, ServerConnection};

mod common;
use crate::common::*;

async fn alpn_test_error(
    server_protos: Vec<Vec<u8>>,
    client_protos: Vec<Vec<u8>>,
    agreed: Option<&[u8]>,
    expected_error: Option<ErrorFromPeer>,
) {
    let mut server_config = make_server_config(KeyType::Rsa);
    server_config.alpn_protocols = server_protos;

    let server_config = Arc::new(server_config);

    for version in tls_aio::ALL_VERSIONS {
        let mut client_config = make_client_config_with_versions(KeyType::Rsa, &[version]);
        client_config.alpn_protocols = client_protos.clone();

        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;

        assert_eq!(client.alpn_protocol(), None);
        assert_eq!(server.alpn_protocol(), None);
        let error = do_handshake_until_error(&mut client, &mut server).await;
        assert_eq!(client.alpn_protocol(), agreed);
        assert_eq!(server.alpn_protocol(), agreed);
        assert_eq!(error.err(), expected_error);
    }
}

async fn alpn_test(server_protos: Vec<Vec<u8>>, client_protos: Vec<Vec<u8>>, agreed: Option<&[u8]>) {
    alpn_test_error(server_protos, client_protos, agreed, None).await
}

#[tokio::test]
async fn alpn() {
    // no support
    alpn_test(vec![], vec![], None).await;

    // server support
    alpn_test(vec![b"server-proto".to_vec()], vec![], None).await;

    // client support
    alpn_test(vec![], vec![b"client-proto".to_vec()], None).await;

    // no overlap
    alpn_test_error(
        vec![b"server-proto".to_vec()],
        vec![b"client-proto".to_vec()],
        None,
        Some(ErrorFromPeer::Server(rustls::Error::NoApplicationProtocol)),
    ).await;

    // server chooses preference
    alpn_test(
        vec![b"server-proto".to_vec(), b"client-proto".to_vec()],
        vec![b"client-proto".to_vec(), b"server-proto".to_vec()],
        Some(b"server-proto"),
    ).await;

    // case sensitive
    alpn_test_error(
        vec![b"PROTO".to_vec()],
        vec![b"proto".to_vec()],
        None,
        Some(ErrorFromPeer::Server(rustls::Error::NoApplicationProtocol)),
    ).await;
}

async fn version_test(
    client_versions: &[&'static tls_aio::SupportedProtocolVersion],
    server_versions: &[&'static rustls::SupportedProtocolVersion],
    result: Option<ProtocolVersion>,
) {
    let client_versions = if client_versions.is_empty() {
        &tls_aio::ALL_VERSIONS
    } else {
        client_versions
    };
    let server_versions = if server_versions.is_empty() {
        &rustls::ALL_VERSIONS
    } else {
        server_versions
    };

    let client_config = make_client_config_with_versions(KeyType::Rsa, client_versions);
    let server_config = make_server_config_with_versions(KeyType::Rsa, server_versions);

    println!(
        "version {:?} {:?} -> {:?}",
        client_versions, server_versions, result
    );

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;

    assert_eq!(client.protocol_version(), None);
    if result.is_none() {
        let err = do_handshake_until_error(&mut client, &mut server).await;
        assert!(err.is_err());
    } else {
        do_handshake(&mut client, &mut server).await;
        assert_eq!(client.protocol_version(), result);
    }
}

#[tokio::test]
async fn versions() {
    // default -> 1.3
    version_test(&[], &[], Some(ProtocolVersion::TLSv1_3)).await;

    // client default, server 1.2 -> 1.2
    #[cfg(feature = "tls12")]
    version_test(
        &[],
        &[&rustls::version::TLS12],
        Some(ProtocolVersion::TLSv1_2),
    ).await;

    // client 1.2, server default -> 1.2
    #[cfg(feature = "tls12")]
    version_test(
        &[&tls_aio::version::TLS12],
        &[],
        Some(ProtocolVersion::TLSv1_2),
    ).await;

    // client 1.2, server 1.3 -> fail
    #[cfg(feature = "tls12")]
    version_test(
        &[&tls_aio::version::TLS12],
        &[&rustls::version::TLS13],
        None,
    ).await;

    // client 1.3, server 1.2 -> fail
    #[cfg(feature = "tls12")]
    version_test(
        &[&tls_aio::version::TLS13],
        &[&rustls::version::TLS12],
        None,
    ).await;

    // client 1.3, server 1.2+1.3 -> 1.3
    #[cfg(feature = "tls12")]
    version_test(
        &[&tls_aio::version::TLS13],
        &[&rustls::version::TLS12, &rustls::version::TLS13],
        Some(ProtocolVersion::TLSv1_3),
    ).await;

    // client 1.2+1.3, server 1.2 -> 1.2
    #[cfg(feature = "tls12")]
    version_test(
        &[&tls_aio::version::TLS13, &tls_aio::version::TLS12],
        &[&rustls::version::TLS12],
        Some(ProtocolVersion::TLSv1_2),
    ).await;
}

fn check_read(reader: &mut dyn io::Read, bytes: &[u8]) {
    let mut buf = vec![0u8; bytes.len() + 1];
    assert_eq!(bytes.len(), reader.read(&mut buf).unwrap());
    assert_eq!(bytes, &buf[..bytes.len()]);
}

#[test]
fn config_builder_for_client_rejects_empty_kx_groups() {
    assert_eq!(
        ClientConfig::builder()
            .with_safe_default_cipher_suites()
            .with_kx_groups(&[])
            .with_safe_default_protocol_versions()
            .err(),
        Some(Error::General("no kx groups configured".into()))
    );
}

#[test]
fn config_builder_for_client_rejects_empty_cipher_suites() {
    assert_eq!(
        ClientConfig::builder()
            .with_cipher_suites(&[])
            .with_safe_default_kx_groups()
            .with_safe_default_protocol_versions()
            .err(),
        Some(Error::General("no usable cipher suites configured".into()))
    );
}

#[cfg(feature = "tls12")]
#[test]
fn config_builder_for_client_rejects_incompatible_cipher_suites() {
    assert_eq!(
        ClientConfig::builder()
            .with_cipher_suites(&[tls_aio::cipher_suite::TLS13_AES_256_GCM_SHA384])
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&tls_aio::version::TLS12])
            .err(),
        Some(Error::General("no usable cipher suites configured".into()))
    );
}

#[tokio::test]
async fn servered_client_data_sent() {
    let server_config = Arc::new(make_server_config(KeyType::Rsa));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(KeyType::Rsa, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;

        assert_eq!(5, client.write_plaintext(b"hello").await.unwrap());

        do_handshake(&mut client, &mut server).await;
        send(&mut client, &mut server);
        server.process_new_packets().unwrap();

        check_read(&mut server.reader(), b"hello");
    }
}

#[tokio::test]
async fn servered_server_data_sent() {
    let server_config = Arc::new(make_server_config(KeyType::Rsa));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(KeyType::Rsa, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;

        assert_eq!(5, server.writer().write(b"hello").unwrap());

        do_handshake(&mut client, &mut server).await;
        receive(&mut server, &mut client);
        client.process_new_packets().await.unwrap();

        check_read(&mut client.reader(), b"hello");
    }
}

#[tokio::test]
async fn servered_both_data_sent() {
    let server_config = Arc::new(make_server_config(KeyType::Rsa));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(KeyType::Rsa, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;

        assert_eq!(12, server.writer().write(b"from-server!").unwrap());
        assert_eq!(12, client.write_plaintext(b"from-client!").await.unwrap());

        do_handshake(&mut client, &mut server).await;

        receive(&mut server, &mut client);
        client.process_new_packets().await.unwrap();
        send(&mut client, &mut server);
        server.process_new_packets().unwrap();

        check_read(&mut client.reader(), b"from-server!");
        check_read(&mut server.reader(), b"from-client!");
    }
}

#[tokio::test]
async fn client_can_get_server_cert() {
    for kt in ALL_KEY_TYPES.iter() {
        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version]);
            let (mut client, mut server) =
                make_pair_for_configs(client_config, make_server_config(*kt)).await;
            do_handshake(&mut client, &mut server).await;

            let certs = client.peer_certificates();
            assert_eq!(certs, Some(kt.get_chain().as_slice()));
        }
    }
}

#[tokio::test]
async fn client_can_get_server_cert_after_resumption() {
    for kt in ALL_KEY_TYPES.iter() {
        let server_config = make_server_config(*kt);
        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version]);
            let (mut client, mut server) =
                make_pair_for_configs(client_config.clone(), server_config.clone()).await;
            do_handshake(&mut client, &mut server).await;

            let original_certs = client.peer_certificates();

            let (mut client, mut server) =
                make_pair_for_configs(client_config.clone(), server_config.clone()).await;
            do_handshake(&mut client, &mut server).await;

            let resumed_certs = client.peer_certificates();

            assert_eq!(original_certs, resumed_certs);
        }
    }
}

#[tokio::test]
async fn server_can_get_client_cert() {
    for kt in ALL_KEY_TYPES.iter() {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions_with_auth(*kt, &[version]);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
            do_handshake(&mut client, &mut server).await;

            let certs = server.peer_certificates();
            assert_eq!(certs, Some(kt.get_client_chain_rustls().as_slice()));
        }
    }
}

#[tokio::test]
async fn server_can_get_client_cert_after_resumption() {
    for kt in ALL_KEY_TYPES.iter() {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions_with_auth(*kt, &[version]);
            let client_config = Arc::new(client_config);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &server_config).await;
            do_handshake(&mut client, &mut server).await;
            let original_certs = server.peer_certificates();

            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &server_config).await;
            do_handshake(&mut client, &mut server).await;
            let resumed_certs = server.peer_certificates();
            assert_eq!(original_certs, resumed_certs);
        }
    }
}

// /// Test that the server handles combination of `offer_client_auth()` returning true
// /// and `client_auth_mandatory` returning `Some(false)`. This exercises both the
// /// client's and server's ability to "recover" from the server asking for a client
// /// certificate and not being given one. This also covers the implementation
// /// of `AllowAnyAnonymousOrAuthenticatedClient`.
// #[tokio::test]
// fn server_allow_any_anonymous_or_authenticated_client() {
//     let kt = KeyType::Rsa;
//     for client_cert_chain in [None, Some(kt.get_client_chain())].iter() {
//         let client_auth_roots = get_client_root_store(kt);
//         let client_auth = AllowAnyAnonymousOrAuthenticatedClient::new(client_auth_roots);

//         let server_config = ServerConfig::builder()
//             .with_safe_defaults()
//             .with_client_cert_verifier(client_auth)
//             .with_single_cert(kt.get_chain_rustls(), kt.get_key_rustls())
//             .unwrap();
//         let server_config = Arc::new(server_config);

//         for version in tls_aio::ALL_VERSIONS {
//             let client_config = if client_cert_chain.is_some() {
//                 make_client_config_with_versions_with_auth(kt, &[version])
//             } else {
//                 make_client_config_with_versions(kt, &[version])
//             };
//             let (mut client, mut server) =
//                 make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
//             do_handshake(&mut client, &mut server).await;

//             let certs = server.peer_certificates();
//             assert_eq!(certs, client_cert_chain.as_deref());
//         }
//     }
// }

fn check_read_and_close(reader: &mut dyn io::Read, expect: &[u8]) {
    check_read(reader, expect);
    assert!(matches!(reader.read(&mut [0u8; 5]), Ok(0)));
}

#[tokio::test]
async fn server_close_notify() {
    let kt = KeyType::Rsa;
    let server_config = Arc::new(make_server_config_with_mandatory_client_auth(kt));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions_with_auth(kt, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
        do_handshake(&mut client, &mut server).await;

        // check that alerts don't overtake appdata
        assert_eq!(12, server.writer().write(b"from-server!").unwrap());
        assert_eq!(12, client.write_plaintext(b"from-client!").await.unwrap());
        server.send_close_notify();

        receive(&mut server, &mut client);
        let io_state = client.process_new_packets().await.unwrap();
        assert!(io_state.peer_has_closed());
        check_read_and_close(&mut client.reader(), b"from-server!");

        send(&mut client, &mut server);
        server.process_new_packets().unwrap();
        check_read(&mut server.reader(), b"from-client!");
    }
}

#[tokio::test]
async fn client_close_notify() {
    let kt = KeyType::Rsa;
    let server_config = Arc::new(make_server_config_with_mandatory_client_auth(kt));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions_with_auth(kt, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
        do_handshake(&mut client, &mut server).await;

        // check that alerts don't overtake appdata
        assert_eq!(12, server.writer().write(b"from-server!").unwrap());
        assert_eq!(12, client.write_plaintext(b"from-client!").await.unwrap());
        client.send_close_notify().await;

        send(&mut client, &mut server);
        let io_state = server.process_new_packets().unwrap();
        assert!(io_state.peer_has_closed());
        check_read_and_close(&mut server.reader(), b"from-client!");

        receive(&mut server, &mut client);
        client.process_new_packets().await.unwrap();
        check_read(&mut client.reader(), b"from-server!");
    }
}

#[tokio::test]
async fn server_closes_uncleanly() {
    let kt = KeyType::Rsa;
    let server_config = Arc::new(make_server_config(kt));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
        do_handshake(&mut client, &mut server).await;

        // check that unclean EOF reporting does not overtake appdata
        assert_eq!(12, server.writer().write(b"from-server!").unwrap());
        assert_eq!(12, client.write_plaintext(b"from-client!").await.unwrap());

        receive(&mut server, &mut client);
        transfer_eof(&mut client);
        let io_state = client.process_new_packets().await.unwrap();
        assert!(!io_state.peer_has_closed());
        check_read(&mut client.reader(), b"from-server!");

        assert!(matches!(client.reader().read(&mut [0u8; 1]),
                         Err(err) if err.kind() == io::ErrorKind::UnexpectedEof));

        // may still transmit pending frames
        send(&mut client, &mut server);
        server.process_new_packets().unwrap();
        check_read(&mut server.reader(), b"from-client!");
    }
}

#[tokio::test]
async fn client_closes_uncleanly() {
    let kt = KeyType::Rsa;
    let server_config = Arc::new(make_server_config(kt));

    for version in tls_aio::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version]);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
        do_handshake(&mut client, &mut server).await;

        // check that unclean EOF reporting does not overtake appdata
        assert_eq!(12, server.writer().write(b"from-server!").unwrap());
        assert_eq!(12, client.write_plaintext(b"from-client!").await.unwrap());

        send(&mut client, &mut server);
        transfer_eof_rustls(&mut server);
        let io_state = server.process_new_packets().unwrap();
        assert!(!io_state.peer_has_closed());
        check_read(&mut server.reader(), b"from-client!");

        assert!(matches!(server.reader().read(&mut [0u8; 1]),
                         Err(err) if err.kind() == io::ErrorKind::UnexpectedEof));

        // may still transmit pending frames
        receive(&mut server, &mut client);
        client.process_new_packets().await.unwrap();
        check_read(&mut client.reader(), b"from-server!");
    }
}

#[derive(Default)]
struct ServerCheckCertResolve {
    expected_sni: Option<String>,
    expected_sigalgs: Option<Vec<rustls::SignatureScheme>>,
    expected_alpn: Option<Vec<Vec<u8>>>,
}

impl ResolvesServerCert for ServerCheckCertResolve {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<rustls::sign::CertifiedKey>> {
        if client_hello.signature_schemes().is_empty() {
            panic!("no signature schemes shared by client");
        }

        if let Some(expected_sni) = &self.expected_sni {
            let sni: &str = client_hello.server_name().expect("sni unexpectedly absent");
            assert_eq!(expected_sni, sni);
        }

        if let Some(expected_sigalgs) = &self.expected_sigalgs {
            if expected_sigalgs != client_hello.signature_schemes() {
                panic!(
                    "unexpected signature schemes (wanted {:?} got {:?})",
                    self.expected_sigalgs,
                    client_hello.signature_schemes()
                );
            }
        }

        if let Some(expected_alpn) = &self.expected_alpn {
            let alpn = client_hello
                .alpn()
                .expect("alpn unexpectedly absent")
                .collect::<Vec<_>>();
            assert_eq!(alpn.len(), expected_alpn.len());

            for (got, wanted) in alpn.iter().zip(expected_alpn.iter()) {
                assert_eq!(got, &wanted.as_slice());
            }
        }

        None
    }
}

#[tokio::test]
async fn server_cert_resolve_with_sni() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config(*kt);
        let mut server_config = make_server_config(*kt);

        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_sni: Some("the-value-from-sni".into()),
            ..Default::default()
        });

        let mut client =
            ClientConnection::new(Arc::new(client_config), dns_name("the-value-from-sni")).unwrap();
        client.start().await.unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        let err = do_handshake_until_error(&mut client, &mut server).await;
        assert!(err.is_err());
    }
}

#[tokio::test]
async fn server_cert_resolve_with_alpn() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        client_config.alpn_protocols = vec!["foo".into(), "bar".into()];

        let mut server_config = make_server_config(*kt);
        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_alpn: Some(vec![b"foo".to_vec(), b"bar".to_vec()]),
            ..Default::default()
        });

        let mut client =
            ClientConnection::new(Arc::new(client_config), dns_name("sni-value")).unwrap();
        client.start().await.unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        let err = do_handshake_until_error(&mut client, &mut server).await;
        assert!(err.is_err());
    }
}

#[tokio::test]
async fn client_trims_terminating_dot() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config(*kt);
        let mut server_config = make_server_config(*kt);

        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_sni: Some("some-host.com".into()),
            ..Default::default()
        });

        let mut client =
            ClientConnection::new(Arc::new(client_config), dns_name("some-host.com.")).unwrap();
        client.start().await.unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        let err = do_handshake_until_error(&mut client, &mut server).await;
        assert!(err.is_err());
    }
}

#[cfg(feature = "tls12")]
async fn check_sigalgs_reduced_by_ciphersuite(
    kt: KeyType,
    suite: CipherSuite,
    expected_sigalgs: Vec<rustls::SignatureScheme>,
) {
    let client_config = finish_client_config(
        kt,
        ClientConfig::builder()
            .with_cipher_suites(&[find_suite(suite)])
            .with_safe_default_kx_groups()
            .with_safe_default_protocol_versions()
            .unwrap(),
    );

    let mut server_config = make_server_config(kt);

    server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
        expected_sigalgs: Some(expected_sigalgs),
        ..Default::default()
    });

    let mut client = ClientConnection::new(Arc::new(client_config), dns_name("localhost")).unwrap();
    client.start().await.unwrap();
    let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

    let err = do_handshake_until_error(&mut client, &mut server).await;
    assert!(err.is_err());
}

#[cfg(feature = "tls12")]
#[tokio::test]
async fn server_cert_resolve_reduces_sigalgs_for_rsa_ciphersuite() {
    check_sigalgs_reduced_by_ciphersuite(
        KeyType::Rsa,
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        vec![
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
        ],
    ).await;
}

#[cfg(feature = "tls12")]
#[tokio::test]
async fn server_cert_resolve_reduces_sigalgs_for_ecdsa_ciphersuite() {
    check_sigalgs_reduced_by_ciphersuite(
        KeyType::Ecdsa,
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        vec![
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ],
    ).await;
}

struct ServerCheckNoSNI {}

impl ResolvesServerCert for ServerCheckNoSNI {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<rustls::sign::CertifiedKey>> {
        assert!(client_hello.server_name().is_none());

        None
    }
}

#[tokio::test]
async fn client_with_sni_disabled_does_not_send_sni() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut server_config = make_server_config(*kt);
        server_config.cert_resolver = Arc::new(ServerCheckNoSNI {});
        let server_config = Arc::new(server_config);

        for version in tls_aio::ALL_VERSIONS {
            let mut client_config = make_client_config_with_versions(*kt, &[version]);
            client_config.enable_sni = false;

            let mut client =
                ClientConnection::new(Arc::new(client_config), dns_name("value-not-sent")).unwrap();
            client.start().await.unwrap();
            let mut server = ServerConnection::new(Arc::clone(&server_config)).unwrap();

            let err = do_handshake_until_error(&mut client, &mut server).await;
            assert!(err.is_err());
        }
    }
}

#[tokio::test]
async fn client_checks_server_certificate_with_given_name() {
    for kt in ALL_KEY_TYPES.iter() {
        let server_config = Arc::new(make_server_config(*kt));

        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version]);
            let mut client = ClientConnection::new(
                Arc::new(client_config),
                dns_name("not-the-right-hostname.com"),
            ).unwrap();
            client.start().await.unwrap();
            let mut server = ServerConnection::new(Arc::clone(&server_config)).unwrap();

            let err = do_handshake_until_error(&mut client, &mut server).await;
            assert_eq!(
                err,
                Err(ErrorFromPeer::Client(Error::InvalidCertificateData(
                    "invalid peer certificate: CertNotValidForName".into(),
                )))
            );
        }
    }
}

struct ClientCheckCertResolve {
    query_count: AtomicUsize,
    expect_queries: usize,
}

impl ClientCheckCertResolve {
    fn new(expect_queries: usize) -> Self {
        ClientCheckCertResolve {
            query_count: AtomicUsize::new(0),
            expect_queries,
        }
    }
}

impl Drop for ClientCheckCertResolve {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let count = self.query_count.load(Ordering::SeqCst);
            assert_eq!(count, self.expect_queries);
        }
    }
}

impl ResolvesClientCert for ClientCheckCertResolve {
    fn resolve(
        &self,
        acceptable_issuers: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<sign::CertifiedKey>> {
        self.query_count.fetch_add(1, Ordering::SeqCst);

        if acceptable_issuers.is_empty() {
            panic!("no issuers offered by server");
        }

        if sigschemes.is_empty() {
            panic!("no signature schemes shared by server");
        }

        None
    }

    fn has_certs(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn client_cert_resolve() {
    for kt in ALL_KEY_TYPES.iter() {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for version in tls_aio::ALL_VERSIONS {
            let mut client_config = make_client_config_with_versions(*kt, &[version]);
            client_config.client_auth_cert_resolver = Arc::new(ClientCheckCertResolve::new(1));

            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;

            assert_eq!(
                do_handshake_until_error(&mut client, &mut server).await,
                Err(ErrorFromPeer::Server(
                    rustls::Error::NoCertificatesPresented
                ))
            );
        }
    }
}

#[tokio::test]
async fn client_auth_works() {
    for kt in ALL_KEY_TYPES.iter() {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions_with_auth(*kt, &[version]);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config).await;
            do_handshake(&mut client, &mut server).await;
        }
    }
}

#[tokio::test]
async fn client_error_is_sticky() {
    let (mut client, _) = make_pair(KeyType::Rsa).await;
    client
        .read_tls(&mut b"\x16\x03\x03\x00\x08\x0f\x00\x00\x04junk".as_ref())
        .unwrap();
    let mut err = client.process_new_packets().await;
    assert!(err.is_err());
    err = client.process_new_packets().await;
    assert!(err.is_err());
}

#[tokio::test]
async fn client_is_send_and_sync() {
    let (client, _) = make_pair(KeyType::Rsa).await;
    &client as &dyn Send;
    &client as &dyn Sync;
}

#[tokio::test]
async fn client_respects_buffer_limit_pre_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;

    client.set_buffer_limit(Some(32));

    assert_eq!(client.write_plaintext(b"01234567890123456789").await.unwrap(), 20);
    assert_eq!(client.write_plaintext(b"01234567890123456789").await.unwrap(), 12);

    do_handshake(&mut client, &mut server).await;
    send(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server.reader(), b"01234567890123456789012345678901");
}

// #[tokio::test]
// async fn client_respects_buffer_limit_pre_handshake_with_vectored_write() {
//     let (mut client, mut server) = make_pair(KeyType::Rsa).await;

//     client.set_buffer_limit(Some(32));

//     assert_eq!(
//         client
//             .write_vectored(&[
//                 IoSlice::new(b"01234567890123456789"),
//                 IoSlice::new(b"01234567890123456789")
//             ]).await
//             .unwrap(),
//         32
//     );

//     do_handshake(&mut client, &mut server).await;
//     send(&mut client, &mut server);
//     server.process_new_packets().unwrap();

//     check_read(&mut server.reader(), b"01234567890123456789012345678901");
// }

#[tokio::test]
async fn client_respects_buffer_limit_post_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;

    do_handshake(&mut client, &mut server).await;
    client.set_buffer_limit(Some(48));

    assert_eq!(client.write_plaintext(b"01234567890123456789").await.unwrap(), 20);
    assert_eq!(client.write_plaintext(b"01234567890123456789").await.unwrap(), 6);

    send(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server.reader(), b"01234567890123456789012345");
}

struct ServerSession<'a, C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
{
    sess: &'a mut C,
    pub reads: usize,
    pub writevs: Vec<Vec<usize>>,
    fail_ok: bool,
    pub short_writes: bool,
    pub last_error: Option<rustls::Error>,
}

impl<'a, C, S> ServerSession<'a, C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
{
    fn new(sess: &'a mut C) -> ServerSession<'a, C, S> {
        ServerSession {
            sess,
            reads: 0,
            writevs: vec![],
            fail_ok: false,
            short_writes: false,
            last_error: None,
        }
    }

    fn new_fails(sess: &'a mut C) -> ServerSession<'a, C, S> {
        let mut os = ServerSession::new(sess);
        os.fail_ok = true;
        os
    }
}

impl<'a, C, S> io::Read for ServerSession<'a, C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
{
    fn read(&mut self, mut b: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.sess.write_tls(b.by_ref())
    }
}

impl<'a, C, S> io::Write for ServerSession<'a, C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
{
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        unreachable!()
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write_vectored<'b>(&mut self, b: &[io::IoSlice<'b>]) -> io::Result<usize> {
        let mut total = 0;
        let mut lengths = vec![];
        for bytes in b {
            let write_len = if self.short_writes {
                if bytes.len() > 5 {
                    bytes.len() / 2
                } else {
                    bytes.len()
                }
            } else {
                bytes.len()
            };

            let l = self
                .sess
                .read_tls(&mut io::Cursor::new(&bytes[..write_len]))?;
            lengths.push(l);
            total += l;
            if bytes.len() != l {
                break;
            }
        }

        let rc = self.sess.process_new_packets();
        if !self.fail_ok {
            rc.unwrap();
        } else if rc.is_err() {
            self.last_error = rc.err();
        }

        self.writevs.push(lengths);
        Ok(total)
    }
}

struct ClientSession<'a, C>
where
    C: DerefMut + Deref<Target = tls_aio::ConnectionCommon>,
{
    sess: &'a mut C,
    pub reads: usize,
    pub writevs: Vec<Vec<usize>>,
    fail_ok: bool,
    pub short_writes: bool,
    pub last_error: Option<tls_aio::Error>,
}

impl<'a, C> ClientSession<'a, C>
where
    C: DerefMut + Deref<Target = tls_aio::ConnectionCommon>,
{
    fn new(sess: &'a mut C) -> ClientSession<'a, C> {
        ClientSession {
            sess,
            reads: 0,
            writevs: vec![],
            fail_ok: false,
            short_writes: false,
            last_error: None,
        }
    }

    fn new_fails(sess: &'a mut C) -> ClientSession<'a, C> {
        let mut os = ClientSession::new(sess);
        os.fail_ok = true;
        os
    }
}

impl<'a, C> io::Read for ClientSession<'a, C>
where
    C: DerefMut + Deref<Target = tls_aio::ConnectionCommon>,
{
    fn read(&mut self, mut b: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.sess.write_tls(b.by_ref())
    }
}

impl<'a, C> io::Write for ClientSession<'a, C>
where
    C: DerefMut + Deref<Target = tls_aio::ConnectionCommon>,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unreachable!()
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        let mut lengths = vec![];
        for bytes in bufs {
            let write_len = if self.short_writes {
                if bytes.len() > 5 {
                    bytes.len() / 2
                } else {
                    bytes.len()
                }
            } else {
                bytes.len()
            };

            let l = self
                .sess
                .read_tls(&mut io::Cursor::new(&bytes[..write_len]))?;
            lengths.push(l);
            total += l;
            if bytes.len() != l {
                break;
            }
        }

        let rc = futures::executor::block_on(self.sess.process_new_packets());
        if !self.fail_ok {
            rc.unwrap();
        } else if rc.is_err() {
            self.last_error = rc.err();
        }

        self.writevs.push(lengths);
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn client_read_returns_wouldblock_when_no_data() {
    let (mut client, _) = make_pair(KeyType::Rsa).await;
    assert!(matches!(client.reader().read(&mut [0u8; 1]),
                     Err(err) if err.kind() == io::ErrorKind::WouldBlock));
}

#[tokio::test]
async fn client_returns_initial_io_state() {
    let (mut client, _) = make_pair(KeyType::Rsa).await;
    let io_state = client.process_new_packets().await.unwrap();
    println!("IoState is Debug {:?}", io_state);
    assert_eq!(io_state.plaintext_bytes_to_read(), 0);
    assert!(!io_state.peer_has_closed());
    assert!(io_state.tls_bytes_to_write() > 200);
}

#[tokio::test]
async fn client_complete_io_for_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;

    assert!(client.is_handshaking());
    let (rdlen, wrlen) = client
        .complete_io(&mut ServerSession::new(&mut server))
        .await
        .unwrap();
    assert!(rdlen > 0 && wrlen > 0);
    assert!(!client.is_handshaking());
}

#[tokio::test]
async fn client_complete_io_for_handshake_eof() {
    let (mut client, _) = make_pair(KeyType::Rsa).await;
    let mut input = io::Cursor::new(Vec::new());

    assert!(client.is_handshaking());
    let err = client.complete_io(&mut input).await.unwrap_err();
    assert_eq!(io::ErrorKind::UnexpectedEof, err.kind());
}

#[tokio::test]
async fn client_complete_io_for_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt).await;

        do_handshake(&mut client, &mut server).await;

        client.write_plaintext(b"01234567890123456789").await.unwrap();
        client.write_plaintext(b"01234567890123456789").await.unwrap();
        {
            let mut pipe = ServerSession::new(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).await.unwrap();
            assert!(rdlen == 0 && wrlen > 0);
            println!("{:?}", pipe.writevs);
            assert_eq!(pipe.writevs, vec![vec![42, 42]]);
        }
        check_read(
            &mut server.reader(),
            b"0123456789012345678901234567890123456789",
        );
    }
}

#[tokio::test]
async fn client_complete_io_for_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt).await;

        do_handshake(&mut client, &mut server).await;

        server.writer().write_all(b"01234567890123456789").unwrap();
        {
            let mut pipe = ServerSession::new(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).await.unwrap();
            assert!(rdlen > 0 && wrlen == 0);
            assert_eq!(pipe.reads, 1);
        }
        check_read(&mut client.reader(), b"01234567890123456789");
    }
}

// #[tokio::test]
// async fn client_stream_write() {
//     for kt in ALL_KEY_TYPES.iter() {
//         let (mut client, mut server) = make_pair(*kt).await;

//         {
//             let mut pipe = ServerSession::new(&mut server);
//             let mut stream = Stream::new(&mut client, &mut pipe);
//             assert_eq!(stream.write(b"hello").unwrap(), 5);
//         }
//         check_read(&mut server.reader(), b"hello");
//     }
// }

// #[tokio::test]
// async fn client_streamowned_write() {
//     for kt in ALL_KEY_TYPES.iter() {
//         let (client, mut server) = make_pair(*kt).await;

//         {
//             let pipe = ServerSession::new(&mut server);
//             let mut stream = StreamOwned::new(client, pipe);
//             assert_eq!(stream.write(b"hello").unwrap(), 5);
//         }
//         check_read(&mut server.reader(), b"hello");
//     }
// }

// #[tokio::test]
// async fn client_stream_read() {
//     for kt in ALL_KEY_TYPES.iter() {
//         let (mut client, mut server) = make_pair(*kt).await;

//         server.writer().write_all(b"world").unwrap();

//         {
//             let mut pipe = ServerSession::new(&mut server);
//             let mut stream = Stream::new(&mut client, &mut pipe);
//             check_read(&mut stream, b"world");
//         }
//     }
// }

// #[tokio::test]
// async fn client_streamowned_read() {
//     for kt in ALL_KEY_TYPES.iter() {
//         let (client, mut server) = make_pair(*kt).await;

//         server.writer().write_all(b"world").unwrap();

//         {
//             let pipe = ServerSession::new(&mut server);
//             let mut stream = StreamOwned::new(client, pipe);
//             check_read(&mut stream, b"world");
//         }
//     }
// }

#[tokio::test]
async fn server_stream_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt).await;

        {
            let mut pipe = ClientSession::new(&mut client);
            let mut stream = rustls::Stream::new(&mut server, &mut pipe);
            assert_eq!(stream.write(b"hello").unwrap(), 5);
        }
        check_read(&mut client.reader(), b"hello");
    }
}

#[tokio::test]
async fn server_streamowned_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, server) = make_pair(*kt).await;

        {
            let pipe = ClientSession::new(&mut client);
            let mut stream = rustls::StreamOwned::new(server, pipe);
            assert_eq!(stream.write(b"hello").unwrap(), 5);
        }
        check_read(&mut client.reader(), b"hello");
    }
}

#[tokio::test]
async fn server_stream_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt).await;

        client.write_all_plaintext(b"world").await.unwrap();

        {
            let mut pipe = ClientSession::new(&mut client);
            let mut stream = rustls::Stream::new(&mut server, &mut pipe);
            check_read(&mut stream, b"world");
        }
    }
}

#[tokio::test]
async fn server_streamowned_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, server) = make_pair(*kt).await;

        client.write_all_plaintext(b"world").await.unwrap();

        {
            let pipe = ClientSession::new(&mut client);
            let mut stream = rustls::StreamOwned::new(server, pipe);
            check_read(&mut stream, b"world");
        }
    }
}

struct FailsWrites {
    errkind: io::ErrorKind,
    after: usize,
}

impl io::Read for FailsWrites {
    fn read(&mut self, _b: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

impl io::Write for FailsWrites {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.after > 0 {
            self.after -= 1;
            Ok(b.len())
        } else {
            Err(io::Error::new(self.errkind, "oops"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// #[tokio::test]
// async fn stream_write_reports_underlying_io_error_before_plaintext_processed() {
//     let (mut client, mut server) = make_pair(KeyType::Rsa).await;
//     do_handshake(&mut client, &mut server).await;

//     let mut pipe = FailsWrites {
//         errkind: io::ErrorKind::ConnectionAborted,
//         after: 0,
//     };
//     client.write_all_plaintext(b"hello").await.unwrap();
//     let mut client_stream = Stream::new(&mut client, &mut pipe);
//     let rc = client_stream.write(b"world");
//     assert!(rc.is_err());
//     let err = rc.err().unwrap();
//     assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
// }

// #[tokio::test]
// async fn stream_write_swallows_underlying_io_error_after_plaintext_processed() {
//     let (mut client, mut server) = make_pair(KeyType::Rsa).await;
//     do_handshake(&mut client, &mut server).await;

//     let mut pipe = FailsWrites {
//         errkind: io::ErrorKind::ConnectionAborted,
//         after: 1,
//     };
//     client.write_all_plaintext(b"hello").await.unwrap();
//     let mut client_stream = Stream::new(&mut client, &mut pipe);
//     let rc = client_stream.write(b"world");
//     assert_eq!(format!("{:?}", rc), "Ok(5)");
// }

// fn make_disjoint_suite_configs() -> (ClientConfig, ServerConfig) {
//     let kt = KeyType::Rsa;
//     let server_config = finish_server_config(
//         kt,
//         ServerConfig::builder()
//             .with_cipher_suites(&[rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256])
//             .with_safe_default_kx_groups()
//             .with_safe_default_protocol_versions()
//             .unwrap(),
//     );

//     let client_config = finish_client_config(
//         kt,
//         ClientConfig::builder()
//             .with_cipher_suites(&[tls_aio::cipher_suite::TLS13_AES_256_GCM_SHA384])
//             .with_safe_default_kx_groups()
//             .with_safe_default_protocol_versions()
//             .unwrap(),
//     );

//     (client_config, server_config)
// }

// #[tokio::test]
// async fn client_stream_handshake_error() {
//     let (client_config, server_config) = make_disjoint_suite_configs();
//     let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;

//     {
//         let mut pipe = ServerSession::new_fails(&mut server);
//         let mut client_stream = Stream::new(&mut client, &mut pipe);
//         let rc = client_stream.write(b"hello");
//         assert!(rc.is_err());
//         assert_eq!(
//             format!("{:?}", rc),
//             "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
//         );
//         let rc = client_stream.write(b"hello");
//         assert!(rc.is_err());
//         assert_eq!(
//             format!("{:?}", rc),
//             "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
//         );
//     }
// }

// #[tokio::test]
// async fn client_streamowned_handshake_error() {
//     let (client_config, server_config) = make_disjoint_suite_configs();
//     let (client, mut server) = make_pair_for_configs(client_config, server_config).await;

//     let pipe = ServerSession::new_fails(&mut server);
//     let mut client_stream = StreamOwned::new(client, pipe);
//     let rc = client_stream.write(b"hello");
//     assert!(rc.is_err());
//     assert_eq!(
//         format!("{:?}", rc),
//         "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
//     );
//     let rc = client_stream.write(b"hello");
//     assert!(rc.is_err());
//     assert_eq!(
//         format!("{:?}", rc),
//         "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
//     );
// }

#[tokio::test]
async fn client_config_is_clone() {
    let _ = make_client_config(KeyType::Rsa);
}

#[tokio::test]
async fn client_connection_is_debug() {
    let (client, _) = make_pair(KeyType::Rsa).await;
    println!("{:?}", client);
}

async fn do_exporter_test(client_config: ClientConfig, server_config: ServerConfig) {
    let mut client_secret = [0u8; 64];
    let mut server_secret = [0u8; 64];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;

    assert_eq!(
        Err(Error::HandshakeNotComplete),
        client.export_keying_material(&mut client_secret, b"label", Some(b"context"))
    );
    assert_eq!(
        Err(rustls::Error::HandshakeNotComplete),
        server.export_keying_material(&mut server_secret, b"label", Some(b"context"))
    );
    do_handshake(&mut client, &mut server).await;

    assert_eq!(
        Ok(()),
        client.export_keying_material(&mut client_secret, b"label", Some(b"context"))
    );
    assert_eq!(
        Ok(()),
        server.export_keying_material(&mut server_secret, b"label", Some(b"context"))
    );
    assert_eq!(client_secret.to_vec(), server_secret.to_vec());

    assert_eq!(
        Ok(()),
        client.export_keying_material(&mut client_secret, b"label", None)
    );
    assert_ne!(client_secret.to_vec(), server_secret.to_vec());
    assert_eq!(
        Ok(()),
        server.export_keying_material(&mut server_secret, b"label", None)
    );
    assert_eq!(client_secret.to_vec(), server_secret.to_vec());
}

#[cfg(feature = "tls12")]
#[tokio::test]
async fn test_tls12_exporter() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config_with_versions(*kt, &[&tls_aio::version::TLS12]);
        let server_config = make_server_config(*kt);

        do_exporter_test(client_config, server_config).await;
    }
}

#[tokio::test]
async fn test_tls13_exporter() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config_with_versions(*kt, &[&tls_aio::version::TLS13]);
        let server_config = make_server_config(*kt);

        do_exporter_test(client_config, server_config).await;
    }
}

async fn do_suite_test(
    client_config: ClientConfig,
    server_config: ServerConfig,
    expect_suite: SupportedCipherSuite,
    expect_version: ProtocolVersion,
) {
    println!(
        "do_suite_test {:?} {:?}",
        expect_version,
        expect_suite.suite()
    );
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;

    assert_eq!(None, client.negotiated_cipher_suite());
    assert_eq!(None, server.negotiated_cipher_suite());
    assert_eq!(None, client.protocol_version());
    assert_eq!(None, version_compat(server.protocol_version()));
    assert!(client.is_handshaking());
    assert!(server.is_handshaking());

    send(&mut client, &mut server);
    server.process_new_packets().unwrap();

    assert!(client.is_handshaking());
    assert!(server.is_handshaking());
    assert_eq!(None, client.protocol_version());
    assert_eq!(
        Some(expect_version),
        version_compat(server.protocol_version())
    );
    assert_eq!(None, client.negotiated_cipher_suite());
    // assert_eq!(Some(expect_suite), server.negotiated_cipher_suite());

    receive(&mut server, &mut client);
    client.process_new_packets().await.unwrap();

    assert_eq!(Some(expect_suite), client.negotiated_cipher_suite());
    // assert_eq!(Some(expect_suite), server.negotiated_cipher_suite());

    send(&mut client, &mut server);
    server.process_new_packets().unwrap();
    receive(&mut server, &mut client);
    client.process_new_packets().await.unwrap();

    assert!(!client.is_handshaking());
    assert!(!server.is_handshaking());
    assert_eq!(Some(expect_version), client.protocol_version());
    assert_eq!(
        Some(expect_version),
        version_compat(server.protocol_version())
    );
    assert_eq!(Some(expect_suite), client.negotiated_cipher_suite());
    // assert_eq!(Some(expect_suite), server.negotiated_cipher_suite());
}

fn find_suite(suite: CipherSuite) -> SupportedCipherSuite {
    for scs in ALL_CIPHER_SUITES.iter().copied() {
        if scs.suite() == suite {
            return scs;
        }
    }

    panic!("find_suite given unsupported suite");
}

static TEST_CIPHERSUITES: &[(&tls_aio::SupportedProtocolVersion, KeyType, CipherSuite)] = &[
    (
        &tls_aio::version::TLS13,
        KeyType::Rsa,
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
    ),
    (
        &tls_aio::version::TLS13,
        KeyType::Rsa,
        CipherSuite::TLS13_AES_256_GCM_SHA384,
    ),
    (
        &tls_aio::version::TLS13,
        KeyType::Rsa,
        CipherSuite::TLS13_AES_128_GCM_SHA256,
    ),
    #[cfg(feature = "tls12")]
    (
        &tls_aio::version::TLS12,
        KeyType::Ecdsa,
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ),
    #[cfg(feature = "tls12")]
    (
        &tls_aio::version::TLS12,
        KeyType::Rsa,
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ),
    #[cfg(feature = "tls12")]
    (
        &tls_aio::version::TLS12,
        KeyType::Ecdsa,
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ),
    #[cfg(feature = "tls12")]
    (
        &tls_aio::version::TLS12,
        KeyType::Ecdsa,
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    ),
    #[cfg(feature = "tls12")]
    (
        &tls_aio::version::TLS12,
        KeyType::Rsa,
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    ),
    #[cfg(feature = "tls12")]
    (
        &tls_aio::version::TLS12,
        KeyType::Rsa,
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    ),
];

#[tokio::test]
async fn negotiated_ciphersuite_default() {
    for kt in ALL_KEY_TYPES.iter() {
        do_suite_test(
            make_client_config(*kt),
            make_server_config(*kt),
            find_suite(CipherSuite::TLS13_AES_256_GCM_SHA384),
            ProtocolVersion::TLSv1_3,
        ).await;
    }
}

#[test]
fn all_suites_covered() {
    assert_eq!(ALL_CIPHER_SUITES.len(), TEST_CIPHERSUITES.len());
}

#[tokio::test]
async fn negotiated_ciphersuite_client() {
    for item in TEST_CIPHERSUITES.iter() {
        let (version, kt, suite) = *item;
        let scs = find_suite(suite);
        let client_config = finish_client_config(
            kt,
            ClientConfig::builder()
                .with_cipher_suites(&[scs])
                .with_safe_default_kx_groups()
                .with_protocol_versions(&[version])
                .unwrap(),
        );

        do_suite_test(client_config, make_server_config(kt), scs, version.version).await;
    }
}

#[derive(Debug, PartialEq)]
struct KeyLogItem {
    label: String,
    client_random: Vec<u8>,
    secret: Vec<u8>,
}

struct KeyLogToVec {
    label: &'static str,
    items: Mutex<Vec<KeyLogItem>>,
}

impl KeyLogToVec {
    fn new(who: &'static str) -> Self {
        KeyLogToVec {
            label: who,
            items: Mutex::new(vec![]),
        }
    }

    fn take(&self) -> Vec<KeyLogItem> {
        std::mem::take(&mut self.items.lock().unwrap())
    }
}

impl KeyLog for KeyLogToVec {
    fn log(&self, label: &str, client: &[u8], secret: &[u8]) {
        let value = KeyLogItem {
            label: label.into(),
            client_random: client.into(),
            secret: secret.into(),
        };

        println!("key log {:?}: {:?}", self.label, value);

        self.items.lock().unwrap().push(value);
    }
}

impl rustls::KeyLog for KeyLogToVec {
    fn log(&self, label: &str, client: &[u8], secret: &[u8]) {
        let value = KeyLogItem {
            label: label.into(),
            client_random: client.into(),
            secret: secret.into(),
        };

        println!("key log {:?}: {:?}", self.label, value);

        self.items.lock().unwrap().push(value);
    }
}

#[cfg(feature = "tls12")]
#[tokio::test]
async fn key_log_for_tls12() {
    let client_key_log = Arc::new(KeyLogToVec::new("client"));
    let server_key_log = Arc::new(KeyLogToVec::new("server"));

    let kt = KeyType::Rsa;
    let mut client_config = make_client_config_with_versions(kt, &[&tls_aio::version::TLS12]);
    client_config.key_log = client_key_log.clone();
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
    server_config.key_log = server_key_log.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    do_handshake(&mut client, &mut server).await;

    let client_full_log = client_key_log.take();
    let server_full_log = server_key_log.take();
    assert_eq!(client_full_log, server_full_log);
    assert_eq!(1, client_full_log.len());
    assert_eq!("CLIENT_RANDOM", client_full_log[0].label);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    do_handshake(&mut client, &mut server).await;

    let client_resume_log = client_key_log.take();
    let server_resume_log = server_key_log.take();
    assert_eq!(client_resume_log, server_resume_log);
    assert_eq!(1, client_resume_log.len());
    assert_eq!("CLIENT_RANDOM", client_resume_log[0].label);
    assert_eq!(client_full_log[0].secret, client_resume_log[0].secret);
}

#[tokio::test]
async fn key_log_for_tls13() {
    let client_key_log = Arc::new(KeyLogToVec::new("client"));
    let server_key_log = Arc::new(KeyLogToVec::new("server"));

    let kt = KeyType::Rsa;
    let mut client_config = make_client_config_with_versions(kt, &[&tls_aio::version::TLS13]);
    client_config.key_log = client_key_log.clone();
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
    server_config.key_log = server_key_log.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    do_handshake(&mut client, &mut server).await;

    let client_full_log = client_key_log.take();
    let server_full_log = server_key_log.take();

    assert_eq!(5, client_full_log.len());
    assert_eq!("CLIENT_HANDSHAKE_TRAFFIC_SECRET", client_full_log[0].label);
    assert_eq!("SERVER_HANDSHAKE_TRAFFIC_SECRET", client_full_log[1].label);
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", client_full_log[2].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", client_full_log[3].label);
    assert_eq!("EXPORTER_SECRET", client_full_log[4].label);

    assert_eq!(client_full_log[0], server_full_log[0]);
    assert_eq!(client_full_log[1], server_full_log[1]);
    assert_eq!(client_full_log[2], server_full_log[2]);
    assert_eq!(client_full_log[3], server_full_log[3]);
    assert_eq!(client_full_log[4], server_full_log[4]);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    do_handshake(&mut client, &mut server).await;

    let client_resume_log = client_key_log.take();
    let server_resume_log = server_key_log.take();

    assert_eq!(5, client_resume_log.len());
    assert_eq!(
        "CLIENT_HANDSHAKE_TRAFFIC_SECRET",
        client_resume_log[0].label
    );
    assert_eq!(
        "SERVER_HANDSHAKE_TRAFFIC_SECRET",
        client_resume_log[1].label
    );
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", client_resume_log[2].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", client_resume_log[3].label);
    assert_eq!("EXPORTER_SECRET", client_resume_log[4].label);

    assert_eq!(6, server_resume_log.len());
    assert_eq!("CLIENT_EARLY_TRAFFIC_SECRET", server_resume_log[0].label);
    assert_eq!(
        "CLIENT_HANDSHAKE_TRAFFIC_SECRET",
        server_resume_log[1].label
    );
    assert_eq!(
        "SERVER_HANDSHAKE_TRAFFIC_SECRET",
        server_resume_log[2].label
    );
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", server_resume_log[3].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", server_resume_log[4].label);
    assert_eq!("EXPORTER_SECRET", server_resume_log[5].label);

    assert_eq!(client_resume_log[0], server_resume_log[1]);
    assert_eq!(client_resume_log[1], server_resume_log[2]);
    assert_eq!(client_resume_log[2], server_resume_log[3]);
    assert_eq!(client_resume_log[3], server_resume_log[4]);
    assert_eq!(client_resume_log[4], server_resume_log[5]);
}

#[tokio::test]
async fn servered_write_for_server_appdata() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;
    do_handshake(&mut client, &mut server).await;

    server.writer().write_all(b"01234567890123456789").unwrap();
    server.writer().write_all(b"01234567890123456789").unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert_eq!(84, wrlen);
        assert_eq!(pipe.writevs, vec![vec![42, 42]]);
    }
    check_read(
        &mut client.reader(),
        b"0123456789012345678901234567890123456789",
    );
}

#[tokio::test]
async fn servered_write_for_client_appdata() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;
    do_handshake(&mut client, &mut server).await;

    client.write_all_plaintext(b"01234567890123456789").await.unwrap();
    client.write_all_plaintext(b"01234567890123456789").await.unwrap();
    {
        let mut pipe = ServerSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert_eq!(84, wrlen);
        assert_eq!(pipe.writevs, vec![vec![42, 42]]);
    }
    check_read(
        &mut server.reader(),
        b"0123456789012345678901234567890123456789",
    );
}

#[tokio::test]
async fn servered_write_for_server_handshake_with_half_rtt_data() {
    let mut server_config = make_server_config(KeyType::Rsa);
    server_config.send_half_rtt_data = true;
    let (mut client, mut server) =
        make_pair_for_configs(make_client_config_with_auth(KeyType::Rsa), server_config).await;

    server.writer().write_all(b"01234567890123456789").unwrap();
    server.writer().write_all(b"0123456789").unwrap();

    send(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 4000); // its pretty big (contains cert chain)
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert_eq!(pipe.writevs[0].len(), 8); // at least a server hello/ccs/cert/serverkx/0.5rtt data
    }

    client.process_new_packets().await.unwrap();
    send(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 103);
        assert_eq!(pipe.writevs, vec![vec![103]]);
    }

    assert!(!server.is_handshaking());
    assert!(!client.is_handshaking());
    check_read(&mut client.reader(), b"012345678901234567890123456789");
}

async fn check_half_rtt_does_not_work(server_config: ServerConfig) {
    let (mut client, mut server) =
        make_pair_for_configs(make_client_config_with_auth(KeyType::Rsa), server_config).await;

    server.writer().write_all(b"01234567890123456789").unwrap();
    server.writer().write_all(b"0123456789").unwrap();

    send(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 4000); // its pretty big (contains cert chain)
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() >= 6); // at least a server hello/ccs/cert/serverkx data
    }

    // client second flight
    client.process_new_packets().await.unwrap();
    send(&mut client, &mut server);

    // when client auth is enabled, we don't sent 0.5-rtt data, as we'd be sending
    // it to an unauthenticated peer. so it happens here, in the server's second
    // flight (42 and 32 are lengths of appdata sent above).
    server.process_new_packets().unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 177);
        assert_eq!(pipe.writevs, vec![vec![103, 42, 32]]);
    }

    assert!(!server.is_handshaking());
    assert!(!client.is_handshaking());
    check_read(&mut client.reader(), b"012345678901234567890123456789");
}

#[tokio::test]
async fn servered_write_for_server_handshake_no_half_rtt_with_client_auth() {
    let mut server_config = make_server_config_with_mandatory_client_auth(KeyType::Rsa);
    server_config.send_half_rtt_data = true; // ask even though it will be ignored
    check_half_rtt_does_not_work(server_config).await;
}

#[tokio::test]
async fn servered_write_for_server_handshake_no_half_rtt_by_default() {
    let server_config = make_server_config(KeyType::Rsa);
    assert_eq!(server_config.send_half_rtt_data, false);
    check_half_rtt_does_not_work(server_config).await;
}

#[tokio::test]
async fn servered_write_for_client_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;

    client.write_all_plaintext(b"01234567890123456789").await.unwrap();
    client.write_all_plaintext(b"0123456789").await.unwrap();
    {
        let mut pipe = ServerSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 200); // just the client hello
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 1); // only a client hello
    }

    receive(&mut server, &mut client);
    client.process_new_packets().await.unwrap();

    {
        let mut pipe = ServerSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 154);
        // CCS, finished, then two application datas
        assert_eq!(pipe.writevs, vec![vec![6, 74, 42, 32]]);
    }

    assert!(!server.is_handshaking());
    assert!(!client.is_handshaking());
    check_read(&mut server.reader(), b"012345678901234567890123456789");
}

#[tokio::test]
async fn servered_write_with_slow_client() {
    let (mut client, mut server) = make_pair(KeyType::Rsa).await;

    client.set_buffer_limit(Some(32));

    do_handshake(&mut client, &mut server).await;
    server.writer().write_all(b"01234567890123456789").unwrap();

    {
        let mut pipe = ClientSession::new(&mut client);
        pipe.short_writes = true;
        let wrlen = server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap();
        assert_eq!(42, wrlen);
        assert_eq!(
            pipe.writevs,
            vec![vec![21], vec![10], vec![5], vec![3], vec![3]]
        );
    }
    check_read(&mut client.reader(), b"01234567890123456789");
}

struct ServerStorage {
    storage: Arc<dyn rustls::server::StoresServerSessions>,
    put_count: AtomicUsize,
    get_count: AtomicUsize,
    take_count: AtomicUsize,
}

impl ServerStorage {
    fn new() -> Self {
        ServerStorage {
            storage: rustls::server::ServerSessionMemoryCache::new(1024),
            put_count: AtomicUsize::new(0),
            get_count: AtomicUsize::new(0),
            take_count: AtomicUsize::new(0),
        }
    }

    fn puts(&self) -> usize {
        self.put_count.load(Ordering::SeqCst)
    }
    fn gets(&self) -> usize {
        self.get_count.load(Ordering::SeqCst)
    }
    fn takes(&self) -> usize {
        self.take_count.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for ServerStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(put: {:?}, get: {:?}, take: {:?})",
            self.put_count, self.get_count, self.take_count
        )
    }
}

impl rustls::server::StoresServerSessions for ServerStorage {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        self.storage.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        self.storage.get(key)
    }

    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.take_count.fetch_add(1, Ordering::SeqCst);
        self.storage.take(key)
    }

    fn can_cache(&self) -> bool {
        true
    }
}

struct ClientStorage {
    storage: Arc<dyn tls_aio::client::StoresClientSessions>,
    put_count: AtomicUsize,
    get_count: AtomicUsize,
    last_put_key: Mutex<Option<Vec<u8>>>,
}

impl ClientStorage {
    fn new() -> Self {
        ClientStorage {
            storage: tls_aio::client::ClientSessionMemoryCache::new(1024),
            put_count: AtomicUsize::new(0),
            get_count: AtomicUsize::new(0),
            last_put_key: Mutex::new(None),
        }
    }

    fn puts(&self) -> usize {
        self.put_count.load(Ordering::SeqCst)
    }
    fn gets(&self) -> usize {
        self.get_count.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for ClientStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(puts: {:?}, gets: {:?} )",
            self.put_count, self.get_count
        )
    }
}

impl tls_aio::client::StoresClientSessions for ClientStorage {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        *self.last_put_key.lock().unwrap() = Some(key.clone());
        self.storage.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        self.storage.get(key)
    }
}

#[tokio::test]
async fn tls13_stateful_resumption() {
    let kt = KeyType::Rsa;
    let client_config = make_client_config_with_versions(kt, &[&tls_aio::version::TLS13]);
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
    let storage = Arc::new(ServerStorage::new());
    server_config.session_storage = storage.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    let (full_c2s, full_s2c) = do_handshake(&mut client, &mut server).await;
    assert_eq!(storage.puts(), 1);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(client.peer_certificates().map(|certs| certs.len()), Some(3));

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    let (resume_c2s, resume_s2c) = do_handshake(&mut client, &mut server).await;
    assert!(resume_c2s > full_c2s);
    assert!(resume_s2c < full_s2c);
    assert_eq!(storage.puts(), 2);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 1);
    assert_eq!(client.peer_certificates().map(|certs| certs.len()), Some(3));

    // resumed again
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    let (resume2_c2s, resume2_s2c) = do_handshake(&mut client, &mut server).await;
    assert_eq!(resume_s2c, resume2_s2c);
    assert_eq!(resume_c2s, resume2_c2s);
    assert_eq!(storage.puts(), 3);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 2);
    assert_eq!(client.peer_certificates().map(|certs| certs.len()), Some(3));
}

#[tokio::test]
async fn tls13_stateless_resumption() {
    let kt = KeyType::Rsa;
    let client_config = make_client_config_with_versions(kt, &[&tls_aio::version::TLS13]);
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
    server_config.ticketer = rustls::Ticketer::new().unwrap();
    let storage = Arc::new(ServerStorage::new());
    server_config.session_storage = storage.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    let (full_c2s, full_s2c) = do_handshake(&mut client, &mut server).await;
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(client.peer_certificates().map(|certs| certs.len()), Some(3));

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    let (resume_c2s, resume_s2c) = do_handshake(&mut client, &mut server).await;
    assert!(resume_c2s > full_c2s);
    assert!(resume_s2c < full_s2c);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(client.peer_certificates().map(|certs| certs.len()), Some(3));

    // resumed again
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
    let (resume2_c2s, resume2_s2c) = do_handshake(&mut client, &mut server).await;
    assert_eq!(resume_s2c, resume2_s2c);
    assert_eq!(resume_c2s, resume2_c2s);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(client.peer_certificates().map(|certs| certs.len()), Some(3));
}

// #[tokio::test]
// async fn early_data_not_available() {
//     let (mut client, _) = make_pair(KeyType::Rsa).await;
//     assert!(client.early_data().is_none());
// }

// fn early_data_configs() -> (Arc<ClientConfig>, Arc<ServerConfig>) {
//     let kt = KeyType::Rsa;
//     let mut client_config = make_client_config(kt);
//     client_config.enable_early_data = true;
//     client_config.session_storage = Arc::new(ClientStorage::new());

//     let mut server_config = make_server_config(kt);
//     server_config.max_early_data_size = 1234;
//     (Arc::new(client_config), Arc::new(server_config))
// }

// #[tokio::test]
// async fn early_data_is_available_on_resumption() {
//     let (client_config, server_config) = early_data_configs();

//     let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
//     do_handshake(&mut client, &mut server).await;

//     let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
//     assert!(client.early_data().is_some());
//     assert_eq!(client.early_data().unwrap().bytes_left(), 1234);
//     client.early_data().unwrap().flush().unwrap();
//     assert_eq!(client.early_data().unwrap().write(b"hello").unwrap(), 5);
//     do_handshake(&mut client, &mut server).await;

//     let mut received_early_data = [0u8; 5];
//     assert_eq!(
//         server
//             .early_data()
//             .expect("early_data didn't happen")
//             .read(&mut received_early_data)
//             .expect("early_data failed unexpectedly"),
//         5
//     );
//     assert_eq!(&received_early_data[..], b"hello");
// }

// #[tokio::test]
// async fn early_data_can_be_rejected_by_server() {
//     let (client_config, server_config) = early_data_configs();

//     let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
//     do_handshake(&mut client, &mut server).await;

//     let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config).await;
//     assert!(client.early_data().is_some());
//     assert_eq!(client.early_data().unwrap().bytes_left(), 1234);
//     client.early_data().unwrap().flush().unwrap();
//     assert_eq!(client.early_data().unwrap().write(b"hello").unwrap(), 5);
//     server.reject_early_data();
//     do_handshake(&mut client, &mut server).await;

//     assert_eq!(client.is_early_data_accepted(), false);
// }

#[cfg(feature = "quic")]
mod test_quic {
    use super::*;
    use rustls::Connection;

    // Returns the sender's next secrets to use, or the receiver's error.
    fn step(
        send: &mut dyn QuicExt,
        recv: &mut dyn QuicExt,
    ) -> Result<Option<quic::KeyChange>, Error> {
        let mut buf = Vec::new();
        let change = loop {
            let prev = buf.len();
            if let Some(x) = send.write_hs(&mut buf) {
                break Some(x);
            }
            if prev == buf.len() {
                break None;
            }
        };
        if let Err(e) = recv.read_hs(&buf) {
            return Err(e);
        } else {
            assert_eq!(recv.alert(), None);
        }

        Ok(change)
    }

    #[tokio::test]
    fn test_quic_handshake() {
        fn equal_packet_keys(x: &quic::PacketKey, y: &quic::PacketKey) -> bool {
            // Check that these two sets of keys are equal.
            let mut buf = vec![0; 32];
            let (header, payload_tag) = buf.split_at_mut(8);
            let (payload, tag_buf) = payload_tag.split_at_mut(8);
            let tag = x.encrypt_in_place(42, &*header, payload).unwrap();
            tag_buf.copy_from_slice(tag.as_ref());

            let result = y.decrypt_in_place(42, &*header, payload_tag);
            match result {
                Ok(payload) => payload == &[0; 8],
                Err(_) => false,
            }
        }

        fn compatible_keys(x: &quic::KeyChange, y: &quic::KeyChange) -> bool {
            fn keys(kc: &quic::KeyChange) -> &quic::Keys {
                match kc {
                    quic::KeyChange::Handshake { keys } => keys,
                    quic::KeyChange::OneRtt { keys, .. } => keys,
                }
            }

            let (x, y) = (keys(x), keys(y));
            equal_packet_keys(&x.local.packet, &y.remote.packet)
                && equal_packet_keys(&x.remote.packet, &y.local.packet)
        }

        let kt = KeyType::Rsa;
        let mut client_config = make_client_config_with_versions(kt, &[&rustls::version::TLS13]);
        client_config.enable_early_data = true;
        let client_config = Arc::new(client_config);
        let mut server_config = make_server_config_with_versions(kt, &[&rustls::version::TLS13]);
        server_config.max_early_data_size = 0xffffffff;
        let server_config = Arc::new(server_config);
        let client_params = &b"client params"[..];
        let server_params = &b"server params"[..];

        // full handshake
        let mut client = Connection::from(
            ClientConnection::new_quic(
                Arc::clone(&client_config),
                quic::Version::V1,
                dns_name("localhost"),
                client_params.into(),
            )
            .unwrap(),
        );

        let mut server = Connection::from(
            ServerConnection::new_quic(
                Arc::clone(&server_config),
                quic::Version::V1,
                server_params.into(),
            )
            .unwrap(),
        );

        let client_initial = step(&mut client, &mut server).unwrap();
        assert!(client_initial.is_none());
        assert!(client.zero_rtt_keys().is_none());
        assert_eq!(server.quic_transport_parameters(), Some(client_params));
        let server_hs = step(&mut server, &mut client).unwrap().unwrap();
        assert!(server.zero_rtt_keys().is_none());
        let client_hs = step(&mut client, &mut server).unwrap().unwrap();
        assert!(compatible_keys(&server_hs, &client_hs));
        assert!(client.is_handshaking());
        let server_1rtt = step(&mut server, &mut client).unwrap().unwrap();
        assert!(!client.is_handshaking());
        assert_eq!(client.quic_transport_parameters(), Some(server_params));
        assert!(server.is_handshaking());
        let client_1rtt = step(&mut client, &mut server).unwrap().unwrap();
        assert!(!server.is_handshaking());
        assert!(compatible_keys(&server_1rtt, &client_1rtt));
        assert!(!compatible_keys(&server_hs, &server_1rtt));
        assert!(step(&mut client, &mut server).unwrap().is_none());
        assert!(step(&mut server, &mut client).unwrap().is_none());

        // 0-RTT handshake
        let mut client = ClientConnection::new_quic(
            Arc::clone(&client_config),
            quic::Version::V1,
            dns_name("localhost"),
            client_params.into(),
        )
        .unwrap();
        assert!(client.negotiated_cipher_suite().is_some());

        let mut server = ServerConnection::new_quic(
            Arc::clone(&server_config),
            quic::Version::V1,
            server_params.into(),
        )
        .unwrap();

        step(&mut client, &mut server).unwrap();
        assert_eq!(client.quic_transport_parameters(), Some(server_params));
        {
            let client_early = client.zero_rtt_keys().unwrap();
            let server_early = server.zero_rtt_keys().unwrap();
            assert!(equal_packet_keys(
                &client_early.packet,
                &server_early.packet
            ));
        }
        step(&mut server, &mut client).unwrap().unwrap();
        step(&mut client, &mut server).unwrap().unwrap();
        step(&mut server, &mut client).unwrap().unwrap();
        assert!(client.is_early_data_accepted());

        // 0-RTT rejection
        {
            let client_config = (*client_config).clone();
            let mut client = ClientConnection::new_quic(
                Arc::new(client_config),
                quic::Version::V1,
                dns_name("localhost"),
                client_params.into(),
            )
            .unwrap();

            let mut server = ServerConnection::new_quic(
                Arc::clone(&server_config),
                quic::Version::V1,
                server_params.into(),
            )
            .unwrap();

            step(&mut client, &mut server).unwrap();
            assert_eq!(client.quic_transport_parameters(), Some(server_params));
            assert!(client.zero_rtt_keys().is_some());
            assert!(server.zero_rtt_keys().is_none());
            step(&mut server, &mut client).unwrap().unwrap();
            step(&mut client, &mut server).unwrap().unwrap();
            step(&mut server, &mut client).unwrap().unwrap();
            assert!(!client.is_early_data_accepted());
        }

        // failed handshake
        let mut client = ClientConnection::new_quic(
            client_config,
            quic::Version::V1,
            dns_name("example.com"),
            client_params.into(),
        )
        .unwrap();

        let mut server =
            ServerConnection::new_quic(server_config, quic::Version::V1, server_params.into())
                .unwrap();

        step(&mut client, &mut server).unwrap();
        step(&mut server, &mut client).unwrap().unwrap();
        assert!(step(&mut server, &mut client).is_err());
        assert_eq!(
            client.alert(),
            Some(rustls::internal::msgs::enums::AlertDescription::BadCertificate)
        );

        // Key updates

        let (mut client_secrets, mut server_secrets) = match (client_1rtt, server_1rtt) {
            (quic::KeyChange::OneRtt { next: c, .. }, quic::KeyChange::OneRtt { next: s, .. }) => {
                (c, s)
            }
            _ => unreachable!(),
        };

        let mut client_next = client_secrets.next_packet_keys();
        let mut server_next = server_secrets.next_packet_keys();
        assert!(equal_packet_keys(&client_next.local, &server_next.remote));
        assert!(equal_packet_keys(&server_next.local, &client_next.remote));

        client_next = client_secrets.next_packet_keys();
        server_next = server_secrets.next_packet_keys();
        assert!(equal_packet_keys(&client_next.local, &server_next.remote));
        assert!(equal_packet_keys(&server_next.local, &client_next.remote));
    }

    #[tokio::test]
    fn test_quic_rejects_missing_alpn() {
        let client_params = &b"client params"[..];
        let server_params = &b"server params"[..];

        for &kt in ALL_KEY_TYPES.iter() {
            let client_config = make_client_config_with_versions(kt, &[&rustls::version::TLS13]);
            let client_config = Arc::new(client_config);

            let mut server_config =
                make_server_config_with_versions(kt, &[&rustls::version::TLS13]);
            server_config.alpn_protocols = vec!["foo".into()];
            let server_config = Arc::new(server_config);

            let mut client = ClientConnection::new_quic(
                client_config,
                quic::Version::V1,
                dns_name("localhost"),
                client_params.into(),
            )
            .unwrap();
            let mut server =
                ServerConnection::new_quic(server_config, quic::Version::V1, server_params.into())
                    .unwrap();

            assert_eq!(
                step(&mut client, &mut server).err().unwrap(),
                Error::NoApplicationProtocol
            );

            assert_eq!(
                server.alert(),
                Some(rustls::internal::msgs::enums::AlertDescription::NoApplicationProtocol)
            );
        }
    }

    #[tokio::test]
    fn test_quic_no_tls13_error() {
        let mut client_config =
            make_client_config_with_versions(KeyType::Ed25519, &[&rustls::version::TLS12]);
        client_config.alpn_protocols = vec!["foo".into()];
        let client_config = Arc::new(client_config);

        assert!(ClientConnection::new_quic(
            client_config,
            quic::Version::V1,
            dns_name("localhost"),
            b"client params".to_vec(),
        )
        .is_err());

        let mut server_config =
            make_server_config_with_versions(KeyType::Ed25519, &[&rustls::version::TLS12]);
        server_config.alpn_protocols = vec!["foo".into()];
        let server_config = Arc::new(server_config);

        assert!(ServerConnection::new_quic(
            server_config,
            quic::Version::V1,
            b"server params".to_vec(),
        )
        .is_err());
    }

    #[tokio::test]
    fn test_quic_invalid_early_data_size() {
        let mut server_config =
            make_server_config_with_versions(KeyType::Ed25519, &[&rustls::version::TLS13]);
        server_config.alpn_protocols = vec!["foo".into()];

        let cases = [
            (None, true),
            (Some(0u32), true),
            (Some(5), false),
            (Some(0xffff_ffff), true),
        ];

        for &(size, ok) in cases.iter() {
            println!("early data size case: {:?}", size);
            if let Some(new) = size {
                server_config.max_early_data_size = new;
            }

            let wrapped = Arc::new(server_config.clone());
            assert_eq!(
                ServerConnection::new_quic(wrapped, quic::Version::V1, b"server params".to_vec(),)
                    .is_ok(),
                ok
            );
        }
    }

    #[tokio::test]
    fn test_quic_server_no_params_received() {
        let server_config =
            make_server_config_with_versions(KeyType::Ed25519, &[&rustls::version::TLS13]);
        let server_config = Arc::new(server_config);

        let mut server =
            ServerConnection::new_quic(server_config, quic::Version::V1, b"server params".to_vec())
                .unwrap();

        use ring::rand::SecureRandom;
        use rustls::internal::msgs::base::PayloadU16;
        use rustls::internal::msgs::enums::{
            CipherSuite, Compression, HandshakeType, NamedGroup, SignatureScheme,
        };
        use rustls::internal::msgs::handshake::{
            ClientHelloPayload, HandshakeMessagePayload, KeyShareEntry, Random, SessionID,
        };
        use rustls::internal::msgs::message::PlainMessage;

        let rng = ring::rand::SystemRandom::new();
        let mut random = [0; 32];
        rng.fill(&mut random).unwrap();
        let random = Random::from(random);

        let kx = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::X25519, &rng)
            .unwrap()
            .compute_public_key()
            .unwrap();

        let client_hello = Message {
            version: ProtocolVersion::TLSv1_3,
            payload: MessagePayload::Handshake(HandshakeMessagePayload {
                typ: HandshakeType::ClientHello,
                payload: HandshakePayload::ClientHello(ClientHelloPayload {
                    client_version: ProtocolVersion::TLSv1_3,
                    random,
                    session_id: SessionID::random().unwrap(),
                    cipher_suites: vec![CipherSuite::TLS13_AES_128_GCM_SHA256],
                    compression_methods: vec![Compression::Null],
                    extensions: vec![
                        ClientExtension::SupportedVersions(vec![ProtocolVersion::TLSv1_3]),
                        ClientExtension::NamedGroups(vec![NamedGroup::X25519]),
                        ClientExtension::SignatureAlgorithms(vec![SignatureScheme::ED25519]),
                        ClientExtension::KeyShare(vec![KeyShareEntry {
                            group: NamedGroup::X25519,
                            payload: PayloadU16::new(kx.as_ref().to_vec()),
                        }]),
                    ],
                }),
            }),
        };

        let buf = PlainMessage::from(client_hello)
            .into_unencrypted_opaque()
            .encode();
        server.read_tls(&mut buf.as_slice()).unwrap();
        assert_eq!(
            server.process_new_packets().err(),
            Some(Error::PeerMisbehavedError(
                "QUIC transport parameters not found".into(),
            )),
        );
    }

    #[tokio::test]
    fn test_quic_server_no_tls12() {
        let mut server_config =
            make_server_config_with_versions(KeyType::Ed25519, &[&rustls::version::TLS13]);
        server_config.alpn_protocols = vec!["foo".into()];
        let server_config = Arc::new(server_config);

        use ring::rand::SecureRandom;
        use rustls::internal::msgs::base::PayloadU16;
        use rustls::internal::msgs::enums::{
            CipherSuite, Compression, HandshakeType, NamedGroup, SignatureScheme,
        };
        use rustls::internal::msgs::handshake::{
            ClientHelloPayload, HandshakeMessagePayload, KeyShareEntry, Random, SessionID,
        };
        use rustls::internal::msgs::message::PlainMessage;

        let rng = ring::rand::SystemRandom::new();
        let mut random = [0; 32];
        rng.fill(&mut random).unwrap();
        let random = Random::from(random);

        let kx = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::X25519, &rng)
            .unwrap()
            .compute_public_key()
            .unwrap();

        let mut server =
            ServerConnection::new_quic(server_config, quic::Version::V1, b"server params".to_vec())
                .unwrap();

        let client_hello = Message {
            version: ProtocolVersion::TLSv1_2,
            payload: MessagePayload::Handshake(HandshakeMessagePayload {
                typ: HandshakeType::ClientHello,
                payload: HandshakePayload::ClientHello(ClientHelloPayload {
                    client_version: ProtocolVersion::TLSv1_2,
                    random: random.clone(),
                    session_id: SessionID::random().unwrap(),
                    cipher_suites: vec![CipherSuite::TLS13_AES_128_GCM_SHA256],
                    compression_methods: vec![Compression::Null],
                    extensions: vec![
                        ClientExtension::NamedGroups(vec![NamedGroup::X25519]),
                        ClientExtension::SignatureAlgorithms(vec![SignatureScheme::ED25519]),
                        ClientExtension::KeyShare(vec![KeyShareEntry {
                            group: NamedGroup::X25519,
                            payload: PayloadU16::new(kx.as_ref().to_vec()),
                        }]),
                    ],
                }),
            }),
        };

        let buf = PlainMessage::from(client_hello)
            .into_unencrypted_opaque()
            .encode();
        server.read_tls(&mut buf.as_slice()).unwrap();
        assert_eq!(
            server.process_new_packets().err(),
            Some(Error::PeerIncompatibleError(
                "Server requires TLS1.3, but client omitted versions ext".into(),
            )),
        );
    }

    #[tokio::test]
    fn packet_key_api() {
        use rustls::quic::{Keys, Version};

        // Test vectors: https://www.rfc-editor.org/rfc/rfc9001.html#name-client-initial
        const CONNECTION_ID: &[u8] = &[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];
        const PACKET_NUMBER: u64 = 2;
        const PLAIN_HEADER: &[u8] = &[
            0xc3, 0x00, 0x00, 0x00, 0x01, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08,
            0x00, 0x00, 0x44, 0x9e, 0x00, 0x00, 0x00, 0x02,
        ];

        const PAYLOAD: &[u8] = &[
            0x06, 0x00, 0x40, 0xf1, 0x01, 0x00, 0x00, 0xed, 0x03, 0x03, 0xeb, 0xf8, 0xfa, 0x56,
            0xf1, 0x29, 0x39, 0xb9, 0x58, 0x4a, 0x38, 0x96, 0x47, 0x2e, 0xc4, 0x0b, 0xb8, 0x63,
            0xcf, 0xd3, 0xe8, 0x68, 0x04, 0xfe, 0x3a, 0x47, 0xf0, 0x6a, 0x2b, 0x69, 0x48, 0x4c,
            0x00, 0x00, 0x04, 0x13, 0x01, 0x13, 0x02, 0x01, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x0e, 0x00, 0x00, 0x0b, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e,
            0x63, 0x6f, 0x6d, 0xff, 0x01, 0x00, 0x01, 0x00, 0x00, 0x0a, 0x00, 0x08, 0x00, 0x06,
            0x00, 0x1d, 0x00, 0x17, 0x00, 0x18, 0x00, 0x10, 0x00, 0x07, 0x00, 0x05, 0x04, 0x61,
            0x6c, 0x70, 0x6e, 0x00, 0x05, 0x00, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33,
            0x00, 0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20, 0x93, 0x70, 0xb2, 0xc9, 0xca, 0xa4,
            0x7f, 0xba, 0xba, 0xf4, 0x55, 0x9f, 0xed, 0xba, 0x75, 0x3d, 0xe1, 0x71, 0xfa, 0x71,
            0xf5, 0x0f, 0x1c, 0xe1, 0x5d, 0x43, 0xe9, 0x94, 0xec, 0x74, 0xd7, 0x48, 0x00, 0x2b,
            0x00, 0x03, 0x02, 0x03, 0x04, 0x00, 0x0d, 0x00, 0x10, 0x00, 0x0e, 0x04, 0x03, 0x05,
            0x03, 0x06, 0x03, 0x02, 0x03, 0x08, 0x04, 0x08, 0x05, 0x08, 0x06, 0x00, 0x2d, 0x00,
            0x02, 0x01, 0x01, 0x00, 0x1c, 0x00, 0x02, 0x40, 0x01, 0x00, 0x39, 0x00, 0x32, 0x04,
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x05, 0x04, 0x80, 0x00, 0xff,
            0xff, 0x07, 0x04, 0x80, 0x00, 0xff, 0xff, 0x08, 0x01, 0x10, 0x01, 0x04, 0x80, 0x00,
            0x75, 0x30, 0x09, 0x01, 0x10, 0x0f, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57,
            0x08, 0x06, 0x04, 0x80, 0x00, 0xff, 0xff,
        ];

        let client_keys = Keys::initial(Version::V1, &CONNECTION_ID, true);
        assert_eq!(
            client_keys.local.packet.confidentiality_limit(),
            2u64.pow(23)
        );
        assert_eq!(client_keys.local.packet.integrity_limit(), 2u64.pow(52));
        assert_eq!(client_keys.local.packet.tag_len(), 16);

        let mut buf = Vec::new();
        buf.extend(PLAIN_HEADER);
        buf.extend(PAYLOAD);
        let header_len = PLAIN_HEADER.len();
        let tag_len = client_keys.local.packet.tag_len();
        let padding_len = 1200 - header_len - PAYLOAD.len() - tag_len;
        buf.extend(std::iter::repeat(0).take(padding_len));
        let (header, payload) = buf.split_at_mut(header_len);
        let tag = client_keys
            .local
            .packet
            .encrypt_in_place(PACKET_NUMBER, &*header, payload)
            .unwrap();

        let sample_len = client_keys.local.header.sample_len();
        let sample = &payload[..sample_len];
        let (first, rest) = header.split_at_mut(1);
        client_keys
            .local
            .header
            .encrypt_in_place(sample, &mut first[0], &mut rest[17..21])
            .unwrap();
        buf.extend_from_slice(tag.as_ref());

        const PROTECTED: &[u8] = &[
            0xc0, 0x00, 0x00, 0x00, 0x01, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08,
            0x00, 0x00, 0x44, 0x9e, 0x7b, 0x9a, 0xec, 0x34, 0xd1, 0xb1, 0xc9, 0x8d, 0xd7, 0x68,
            0x9f, 0xb8, 0xec, 0x11, 0xd2, 0x42, 0xb1, 0x23, 0xdc, 0x9b, 0xd8, 0xba, 0xb9, 0x36,
            0xb4, 0x7d, 0x92, 0xec, 0x35, 0x6c, 0x0b, 0xab, 0x7d, 0xf5, 0x97, 0x6d, 0x27, 0xcd,
            0x44, 0x9f, 0x63, 0x30, 0x00, 0x99, 0xf3, 0x99, 0x1c, 0x26, 0x0e, 0xc4, 0xc6, 0x0d,
            0x17, 0xb3, 0x1f, 0x84, 0x29, 0x15, 0x7b, 0xb3, 0x5a, 0x12, 0x82, 0xa6, 0x43, 0xa8,
            0xd2, 0x26, 0x2c, 0xad, 0x67, 0x50, 0x0c, 0xad, 0xb8, 0xe7, 0x37, 0x8c, 0x8e, 0xb7,
            0x53, 0x9e, 0xc4, 0xd4, 0x90, 0x5f, 0xed, 0x1b, 0xee, 0x1f, 0xc8, 0xaa, 0xfb, 0xa1,
            0x7c, 0x75, 0x0e, 0x2c, 0x7a, 0xce, 0x01, 0xe6, 0x00, 0x5f, 0x80, 0xfc, 0xb7, 0xdf,
            0x62, 0x12, 0x30, 0xc8, 0x37, 0x11, 0xb3, 0x93, 0x43, 0xfa, 0x02, 0x8c, 0xea, 0x7f,
            0x7f, 0xb5, 0xff, 0x89, 0xea, 0xc2, 0x30, 0x82, 0x49, 0xa0, 0x22, 0x52, 0x15, 0x5e,
            0x23, 0x47, 0xb6, 0x3d, 0x58, 0xc5, 0x45, 0x7a, 0xfd, 0x84, 0xd0, 0x5d, 0xff, 0xfd,
            0xb2, 0x03, 0x92, 0x84, 0x4a, 0xe8, 0x12, 0x15, 0x46, 0x82, 0xe9, 0xcf, 0x01, 0x2f,
            0x90, 0x21, 0xa6, 0xf0, 0xbe, 0x17, 0xdd, 0xd0, 0xc2, 0x08, 0x4d, 0xce, 0x25, 0xff,
            0x9b, 0x06, 0xcd, 0xe5, 0x35, 0xd0, 0xf9, 0x20, 0xa2, 0xdb, 0x1b, 0xf3, 0x62, 0xc2,
            0x3e, 0x59, 0x6d, 0x11, 0xa4, 0xf5, 0xa6, 0xcf, 0x39, 0x48, 0x83, 0x8a, 0x3a, 0xec,
            0x4e, 0x15, 0xda, 0xf8, 0x50, 0x0a, 0x6e, 0xf6, 0x9e, 0xc4, 0xe3, 0xfe, 0xb6, 0xb1,
            0xd9, 0x8e, 0x61, 0x0a, 0xc8, 0xb7, 0xec, 0x3f, 0xaf, 0x6a, 0xd7, 0x60, 0xb7, 0xba,
            0xd1, 0xdb, 0x4b, 0xa3, 0x48, 0x5e, 0x8a, 0x94, 0xdc, 0x25, 0x0a, 0xe3, 0xfd, 0xb4,
            0x1e, 0xd1, 0x5f, 0xb6, 0xa8, 0xe5, 0xeb, 0xa0, 0xfc, 0x3d, 0xd6, 0x0b, 0xc8, 0xe3,
            0x0c, 0x5c, 0x42, 0x87, 0xe5, 0x38, 0x05, 0xdb, 0x05, 0x9a, 0xe0, 0x64, 0x8d, 0xb2,
            0xf6, 0x42, 0x64, 0xed, 0x5e, 0x39, 0xbe, 0x2e, 0x20, 0xd8, 0x2d, 0xf5, 0x66, 0xda,
            0x8d, 0xd5, 0x99, 0x8c, 0xca, 0xbd, 0xae, 0x05, 0x30, 0x60, 0xae, 0x6c, 0x7b, 0x43,
            0x78, 0xe8, 0x46, 0xd2, 0x9f, 0x37, 0xed, 0x7b, 0x4e, 0xa9, 0xec, 0x5d, 0x82, 0xe7,
            0x96, 0x1b, 0x7f, 0x25, 0xa9, 0x32, 0x38, 0x51, 0xf6, 0x81, 0xd5, 0x82, 0x36, 0x3a,
            0xa5, 0xf8, 0x99, 0x37, 0xf5, 0xa6, 0x72, 0x58, 0xbf, 0x63, 0xad, 0x6f, 0x1a, 0x0b,
            0x1d, 0x96, 0xdb, 0xd4, 0xfa, 0xdd, 0xfc, 0xef, 0xc5, 0x26, 0x6b, 0xa6, 0x61, 0x17,
            0x22, 0x39, 0x5c, 0x90, 0x65, 0x56, 0xbe, 0x52, 0xaf, 0xe3, 0xf5, 0x65, 0x63, 0x6a,
            0xd1, 0xb1, 0x7d, 0x50, 0x8b, 0x73, 0xd8, 0x74, 0x3e, 0xeb, 0x52, 0x4b, 0xe2, 0x2b,
            0x3d, 0xcb, 0xc2, 0xc7, 0x46, 0x8d, 0x54, 0x11, 0x9c, 0x74, 0x68, 0x44, 0x9a, 0x13,
            0xd8, 0xe3, 0xb9, 0x58, 0x11, 0xa1, 0x98, 0xf3, 0x49, 0x1d, 0xe3, 0xe7, 0xfe, 0x94,
            0x2b, 0x33, 0x04, 0x07, 0xab, 0xf8, 0x2a, 0x4e, 0xd7, 0xc1, 0xb3, 0x11, 0x66, 0x3a,
            0xc6, 0x98, 0x90, 0xf4, 0x15, 0x70, 0x15, 0x85, 0x3d, 0x91, 0xe9, 0x23, 0x03, 0x7c,
            0x22, 0x7a, 0x33, 0xcd, 0xd5, 0xec, 0x28, 0x1c, 0xa3, 0xf7, 0x9c, 0x44, 0x54, 0x6b,
            0x9d, 0x90, 0xca, 0x00, 0xf0, 0x64, 0xc9, 0x9e, 0x3d, 0xd9, 0x79, 0x11, 0xd3, 0x9f,
            0xe9, 0xc5, 0xd0, 0xb2, 0x3a, 0x22, 0x9a, 0x23, 0x4c, 0xb3, 0x61, 0x86, 0xc4, 0x81,
            0x9e, 0x8b, 0x9c, 0x59, 0x27, 0x72, 0x66, 0x32, 0x29, 0x1d, 0x6a, 0x41, 0x82, 0x11,
            0xcc, 0x29, 0x62, 0xe2, 0x0f, 0xe4, 0x7f, 0xeb, 0x3e, 0xdf, 0x33, 0x0f, 0x2c, 0x60,
            0x3a, 0x9d, 0x48, 0xc0, 0xfc, 0xb5, 0x69, 0x9d, 0xbf, 0xe5, 0x89, 0x64, 0x25, 0xc5,
            0xba, 0xc4, 0xae, 0xe8, 0x2e, 0x57, 0xa8, 0x5a, 0xaf, 0x4e, 0x25, 0x13, 0xe4, 0xf0,
            0x57, 0x96, 0xb0, 0x7b, 0xa2, 0xee, 0x47, 0xd8, 0x05, 0x06, 0xf8, 0xd2, 0xc2, 0x5e,
            0x50, 0xfd, 0x14, 0xde, 0x71, 0xe6, 0xc4, 0x18, 0x55, 0x93, 0x02, 0xf9, 0x39, 0xb0,
            0xe1, 0xab, 0xd5, 0x76, 0xf2, 0x79, 0xc4, 0xb2, 0xe0, 0xfe, 0xb8, 0x5c, 0x1f, 0x28,
            0xff, 0x18, 0xf5, 0x88, 0x91, 0xff, 0xef, 0x13, 0x2e, 0xef, 0x2f, 0xa0, 0x93, 0x46,
            0xae, 0xe3, 0x3c, 0x28, 0xeb, 0x13, 0x0f, 0xf2, 0x8f, 0x5b, 0x76, 0x69, 0x53, 0x33,
            0x41, 0x13, 0x21, 0x19, 0x96, 0xd2, 0x00, 0x11, 0xa1, 0x98, 0xe3, 0xfc, 0x43, 0x3f,
            0x9f, 0x25, 0x41, 0x01, 0x0a, 0xe1, 0x7c, 0x1b, 0xf2, 0x02, 0x58, 0x0f, 0x60, 0x47,
            0x47, 0x2f, 0xb3, 0x68, 0x57, 0xfe, 0x84, 0x3b, 0x19, 0xf5, 0x98, 0x40, 0x09, 0xdd,
            0xc3, 0x24, 0x04, 0x4e, 0x84, 0x7a, 0x4f, 0x4a, 0x0a, 0xb3, 0x4f, 0x71, 0x95, 0x95,
            0xde, 0x37, 0x25, 0x2d, 0x62, 0x35, 0x36, 0x5e, 0x9b, 0x84, 0x39, 0x2b, 0x06, 0x10,
            0x85, 0x34, 0x9d, 0x73, 0x20, 0x3a, 0x4a, 0x13, 0xe9, 0x6f, 0x54, 0x32, 0xec, 0x0f,
            0xd4, 0xa1, 0xee, 0x65, 0xac, 0xcd, 0xd5, 0xe3, 0x90, 0x4d, 0xf5, 0x4c, 0x1d, 0xa5,
            0x10, 0xb0, 0xff, 0x20, 0xdc, 0xc0, 0xc7, 0x7f, 0xcb, 0x2c, 0x0e, 0x0e, 0xb6, 0x05,
            0xcb, 0x05, 0x04, 0xdb, 0x87, 0x63, 0x2c, 0xf3, 0xd8, 0xb4, 0xda, 0xe6, 0xe7, 0x05,
            0x76, 0x9d, 0x1d, 0xe3, 0x54, 0x27, 0x01, 0x23, 0xcb, 0x11, 0x45, 0x0e, 0xfc, 0x60,
            0xac, 0x47, 0x68, 0x3d, 0x7b, 0x8d, 0x0f, 0x81, 0x13, 0x65, 0x56, 0x5f, 0xd9, 0x8c,
            0x4c, 0x8e, 0xb9, 0x36, 0xbc, 0xab, 0x8d, 0x06, 0x9f, 0xc3, 0x3b, 0xd8, 0x01, 0xb0,
            0x3a, 0xde, 0xa2, 0xe1, 0xfb, 0xc5, 0xaa, 0x46, 0x3d, 0x08, 0xca, 0x19, 0x89, 0x6d,
            0x2b, 0xf5, 0x9a, 0x07, 0x1b, 0x85, 0x1e, 0x6c, 0x23, 0x90, 0x52, 0x17, 0x2f, 0x29,
            0x6b, 0xfb, 0x5e, 0x72, 0x40, 0x47, 0x90, 0xa2, 0x18, 0x10, 0x14, 0xf3, 0xb9, 0x4a,
            0x4e, 0x97, 0xd1, 0x17, 0xb4, 0x38, 0x13, 0x03, 0x68, 0xcc, 0x39, 0xdb, 0xb2, 0xd1,
            0x98, 0x06, 0x5a, 0xe3, 0x98, 0x65, 0x47, 0x92, 0x6c, 0xd2, 0x16, 0x2f, 0x40, 0xa2,
            0x9f, 0x0c, 0x3c, 0x87, 0x45, 0xc0, 0xf5, 0x0f, 0xba, 0x38, 0x52, 0xe5, 0x66, 0xd4,
            0x45, 0x75, 0xc2, 0x9d, 0x39, 0xa0, 0x3f, 0x0c, 0xda, 0x72, 0x19, 0x84, 0xb6, 0xf4,
            0x40, 0x59, 0x1f, 0x35, 0x5e, 0x12, 0xd4, 0x39, 0xff, 0x15, 0x0a, 0xab, 0x76, 0x13,
            0x49, 0x9d, 0xbd, 0x49, 0xad, 0xab, 0xc8, 0x67, 0x6e, 0xef, 0x02, 0x3b, 0x15, 0xb6,
            0x5b, 0xfc, 0x5c, 0xa0, 0x69, 0x48, 0x10, 0x9f, 0x23, 0xf3, 0x50, 0xdb, 0x82, 0x12,
            0x35, 0x35, 0xeb, 0x8a, 0x74, 0x33, 0xbd, 0xab, 0xcb, 0x90, 0x92, 0x71, 0xa6, 0xec,
            0xbc, 0xb5, 0x8b, 0x93, 0x6a, 0x88, 0xcd, 0x4e, 0x8f, 0x2e, 0x6f, 0xf5, 0x80, 0x01,
            0x75, 0xf1, 0x13, 0x25, 0x3d, 0x8f, 0xa9, 0xca, 0x88, 0x85, 0xc2, 0xf5, 0x52, 0xe6,
            0x57, 0xdc, 0x60, 0x3f, 0x25, 0x2e, 0x1a, 0x8e, 0x30, 0x8f, 0x76, 0xf0, 0xbe, 0x79,
            0xe2, 0xfb, 0x8f, 0x5d, 0x5f, 0xbb, 0xe2, 0xe3, 0x0e, 0xca, 0xdd, 0x22, 0x07, 0x23,
            0xc8, 0xc0, 0xae, 0xa8, 0x07, 0x8c, 0xdf, 0xcb, 0x38, 0x68, 0x26, 0x3f, 0xf8, 0xf0,
            0x94, 0x00, 0x54, 0xda, 0x48, 0x78, 0x18, 0x93, 0xa7, 0xe4, 0x9a, 0xd5, 0xaf, 0xf4,
            0xaf, 0x30, 0x0c, 0xd8, 0x04, 0xa6, 0xb6, 0x27, 0x9a, 0xb3, 0xff, 0x3a, 0xfb, 0x64,
            0x49, 0x1c, 0x85, 0x19, 0x4a, 0xab, 0x76, 0x0d, 0x58, 0xa6, 0x06, 0x65, 0x4f, 0x9f,
            0x44, 0x00, 0xe8, 0xb3, 0x85, 0x91, 0x35, 0x6f, 0xbf, 0x64, 0x25, 0xac, 0xa2, 0x6d,
            0xc8, 0x52, 0x44, 0x25, 0x9f, 0xf2, 0xb1, 0x9c, 0x41, 0xb9, 0xf9, 0x6f, 0x3c, 0xa9,
            0xec, 0x1d, 0xde, 0x43, 0x4d, 0xa7, 0xd2, 0xd3, 0x92, 0xb9, 0x05, 0xdd, 0xf3, 0xd1,
            0xf9, 0xaf, 0x93, 0xd1, 0xaf, 0x59, 0x50, 0xbd, 0x49, 0x3f, 0x5a, 0xa7, 0x31, 0xb4,
            0x05, 0x6d, 0xf3, 0x1b, 0xd2, 0x67, 0xb6, 0xb9, 0x0a, 0x07, 0x98, 0x31, 0xaa, 0xf5,
            0x79, 0xbe, 0x0a, 0x39, 0x01, 0x31, 0x37, 0xaa, 0xc6, 0xd4, 0x04, 0xf5, 0x18, 0xcf,
            0xd4, 0x68, 0x40, 0x64, 0x7e, 0x78, 0xbf, 0xe7, 0x06, 0xca, 0x4c, 0xf5, 0xe9, 0xc5,
            0x45, 0x3e, 0x9f, 0x7c, 0xfd, 0x2b, 0x8b, 0x4c, 0x8d, 0x16, 0x9a, 0x44, 0xe5, 0x5c,
            0x88, 0xd4, 0xa9, 0xa7, 0xf9, 0x47, 0x42, 0x41, 0xe2, 0x21, 0xaf, 0x44, 0x86, 0x00,
            0x18, 0xab, 0x08, 0x56, 0x97, 0x2e, 0x19, 0x4c, 0xd9, 0x34,
        ];

        assert_eq!(&buf, PROTECTED);

        let (header, payload) = buf.split_at_mut(header_len);
        let (first, rest) = header.split_at_mut(1);
        let sample = &payload[..sample_len];

        let server_keys = Keys::initial(Version::V1, &CONNECTION_ID, false);
        server_keys
            .remote
            .header
            .decrypt_in_place(sample, &mut first[0], &mut rest[17..21])
            .unwrap();
        let payload = server_keys
            .remote
            .packet
            .decrypt_in_place(PACKET_NUMBER, &*header, payload)
            .unwrap();

        assert_eq!(&payload[..PAYLOAD.len()], PAYLOAD);
        assert_eq!(payload.len(), buf.len() - header_len - tag_len);
    }

    #[tokio::test]
    fn test_quic_exporter() {
        for &kt in ALL_KEY_TYPES.iter() {
            let client_config = make_client_config_with_versions(kt, &[&rustls::version::TLS13]);
            let server_config = make_server_config_with_versions(kt, &[&rustls::version::TLS13]);

            do_exporter_test(client_config, server_config);
        }
    }
} // mod test_quic

#[tokio::test]
async fn test_client_does_not_offer_sha1() {
    use tls_aio::internal::msgs::{
        codec::Reader, enums::HandshakeType, handshake::HandshakePayload, message::MessagePayload,
        message::OpaqueMessage,
    };

    for kt in ALL_KEY_TYPES.iter() {
        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version]);
            let (mut client, _) = make_pair_for_configs(client_config, make_server_config(*kt)).await;

            assert!(client.wants_write());
            let mut buf = [0u8; 262144];
            let sz = client.write_tls(&mut buf.as_mut()).unwrap();
            let msg = OpaqueMessage::read(&mut Reader::init(&buf[..sz])).unwrap();
            let msg = Message::try_from(msg.into_plain_message()).unwrap();
            assert!(msg.is_handshake_type(HandshakeType::ClientHello));

            let client_hello = match msg.payload {
                MessagePayload::Handshake(hs) => match hs.payload {
                    HandshakePayload::ClientHello(ch) => ch,
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };

            let sigalgs = client_hello.get_sigalgs_extension().unwrap();
            assert!(
                !sigalgs.contains(&SignatureScheme::RSA_PKCS1_SHA1),
                "sha1 unexpectedly offered"
            );
        }
    }
}

#[tokio::test]
async fn test_client_config_keyshare() {
    let client_config =
        make_client_config_with_kx_groups(KeyType::Rsa, &[&tls_aio::kx_group::SECP384R1]);
    let server_config =
        make_server_config_with_kx_groups(KeyType::Rsa, &[&rustls::kx_group::SECP384R1]);
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;
    do_handshake_until_error(&mut client, &mut server).await.unwrap();
}

#[tokio::test]
async fn test_client_config_keyshare_mismatch() {
    let client_config =
        make_client_config_with_kx_groups(KeyType::Rsa, &[&tls_aio::kx_group::SECP384R1]);
    let server_config =
        make_server_config_with_kx_groups(KeyType::Rsa, &[&rustls::kx_group::X25519]);
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;
    assert!(do_handshake_until_error(&mut client, &mut server).await.is_err());
}

#[tokio::test]
async fn test_client_sends_helloretryrequest() {
    // client sends a secp384r1 key share
    let mut client_config = make_client_config_with_kx_groups(
        KeyType::Rsa,
        &[&tls_aio::kx_group::SECP384R1, &tls_aio::kx_group::X25519],
    );

    let storage = Arc::new(ClientStorage::new());
    client_config.session_storage = storage.clone();

    // but server only accepts x25519, so a HRR is required
    let server_config =
        make_server_config_with_kx_groups(KeyType::Rsa, &[&rustls::kx_group::X25519]);

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config).await;

    // client sends hello
    {
        let mut pipe = ServerSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert!(wrlen > 200);
        assert_eq!(pipe.writevs.len(), 1);
        assert!(pipe.writevs[0].len() == 1);
    }

    // server sends HRR
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert!(wrlen < 100); // just the hello retry request
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 2); // hello retry request and CCS
    }

    // client sends fixed hello
    {
        let mut pipe = ServerSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert!(wrlen > 200); // just the client hello retry
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 2); // only a CCS & client hello retry
    }

    // server completes handshake
    {
        let mut pipe = ClientSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert!(wrlen > 200);
        assert_eq!(pipe.writevs.len(), 1);
        assert!(pipe.writevs[0].len() == 5); // server hello / encrypted exts / cert / cert-verify / finished
    }

    do_handshake_until_error(&mut client, &mut server).await.unwrap();

    // client only did two storage queries: one for a session, another for a kx type
    assert_eq!(storage.gets(), 2);
    assert_eq!(storage.puts(), 2);
}

#[tokio::test]
async fn test_client_attempts_to_use_unsupported_kx_group() {
    // common to both client configs
    let shared_storage = Arc::new(ClientStorage::new());

    // first, client sends a x25519 and server agrees. x25519 is inserted
    //   into kx group cache.
    let mut client_config_1 =
        make_client_config_with_kx_groups(KeyType::Rsa, &[&tls_aio::kx_group::X25519]);
    client_config_1.session_storage = shared_storage.clone();

    // second, client only supports secp-384 and so kx group cache
    //   contains an unusable value.
    let mut client_config_2 =
        make_client_config_with_kx_groups(KeyType::Rsa, &[&tls_aio::kx_group::SECP384R1]);
    client_config_2.session_storage = shared_storage;

    let server_config = make_server_config(KeyType::Rsa);

    // first handshake
    let (mut client_1, mut server) = make_pair_for_configs(client_config_1, server_config.clone()).await;
    do_handshake_until_error(&mut client_1, &mut server).await.unwrap();

    // second handshake
    let (mut client_2, mut server) = make_pair_for_configs(client_config_2, server_config).await;
    do_handshake_until_error(&mut client_2, &mut server).await.unwrap();
}

#[tokio::test]
async fn test_client_mtu_reduction() {
    struct CollectWrites {
        writevs: Vec<Vec<usize>>,
    }

    impl io::Write for CollectWrites {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!()
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!()
        }
        fn write_vectored<'b>(&mut self, b: &[io::IoSlice<'b>]) -> io::Result<usize> {
            let writes = b.iter().map(|slice| slice.len()).collect::<Vec<usize>>();
            let len = writes.iter().sum();
            self.writevs.push(writes);
            Ok(len)
        }
    }

    fn collect_write_lengths(client: &mut ClientConnection) -> Vec<usize> {
        let mut collector = CollectWrites { writevs: vec![] };

        client.write_tls(&mut collector).unwrap();
        assert_eq!(collector.writevs.len(), 1);
        collector.writevs[0].clone()
    }

    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        client_config.max_fragment_size = Some(64);
        let mut client =
            ClientConnection::new(Arc::new(client_config), dns_name("localhost")).unwrap();
        client.start().await.unwrap();
        let writes = collect_write_lengths(&mut client);
        println!("writes at mtu=64: {:?}", writes);
        assert!(writes.iter().all(|x| *x <= 64));
        assert!(writes.len() > 1);
    }
}

#[tokio::test]
async fn test_server_mtu_reduction() {
    let mut server_config = make_server_config(KeyType::Rsa);
    server_config.max_fragment_size = Some(64);
    server_config.send_half_rtt_data = true;
    let (mut client, mut server) =
        make_pair_for_configs(make_client_config(KeyType::Rsa), server_config).await;

    let big_data = [0u8; 2048];
    server.writer().write_all(&big_data).unwrap();

    let encryption_overhead = 20; // FIXME: see issue #991

    send(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        server.write_tls(&mut pipe).unwrap();

        assert_eq!(pipe.writevs.len(), 1);
        assert!(pipe.writevs[0]
            .iter()
            .all(|x| *x <= 64 + encryption_overhead));
    }

    client.process_new_packets().await.unwrap();
    send(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = ClientSession::new(&mut client);
        server.write_tls(&mut pipe).unwrap();
        assert_eq!(pipe.writevs.len(), 1);
        assert!(pipe.writevs[0]
            .iter()
            .all(|x| *x <= 64 + encryption_overhead));
    }

    client.process_new_packets().await.unwrap();
    check_read(&mut client.reader(), &big_data);
}

async fn check_client_max_fragment_size(size: usize) -> Option<Error> {
    let mut client_config = make_client_config(KeyType::Ed25519);
    client_config.max_fragment_size = Some(size);
    ClientConnection::new(Arc::new(client_config), dns_name("localhost")).err()
}

#[tokio::test]
async fn bad_client_max_fragment_sizes() {
    assert_eq!(
        check_client_max_fragment_size(31).await,
        Some(Error::BadMaxFragmentSize)
    );
    assert_eq!(check_client_max_fragment_size(32).await, None);
    assert_eq!(check_client_max_fragment_size(64).await, None);
    assert_eq!(check_client_max_fragment_size(1460).await, None);
    assert_eq!(check_client_max_fragment_size(0x4000).await, None);
    assert_eq!(check_client_max_fragment_size(0x4005).await, None);
    assert_eq!(
        check_client_max_fragment_size(0x4006).await,
        Some(Error::BadMaxFragmentSize)
    );
    assert_eq!(
        check_client_max_fragment_size(0xffff).await,
        Some(Error::BadMaxFragmentSize)
    );
}

fn assert_lt(left: usize, right: usize) {
    if left >= right {
        panic!("expected {} < {}", left, right);
    }
}

#[test]
fn connection_types_are_not_huge() {
    // Arbitrary sizes
    assert_lt(mem::size_of::<ClientConnection>(), 1600);
}

use tls_aio::internal::msgs::{message::Message, message::MessagePayload};

#[tokio::test]
 async fn test_client_rejects_illegal_tls13_ccs() {
    fn corrupt_ccs(msg: &mut Message) -> Altered {
        if let MessagePayload::ChangeCipherSpec(_) = &mut msg.payload {
            println!("seen CCS {:?}", msg);
            return Altered::Raw(vec![0x14, 0x03, 0x03, 0x00, 0x02, 0x01, 0x02]);
        }
        Altered::InPlace
    }

    let (mut client, mut server) = make_pair(KeyType::Rsa).await;
    send(&mut client, &mut server);
    server.process_new_packets().unwrap();

    let (mut server, mut client) = (server.into(), client.into());

    receive_altered(&mut server, corrupt_ccs, &mut client);
    assert_eq!(
        client.process_new_packets().await,
        Err(Error::PeerMisbehavedError(
            "illegal middlebox CCS received".into()
        ))
    );
}

/// https://github.com/rustls/rustls/issues/797
#[cfg(feature = "tls12")]
#[tokio::test]
async fn test_client_tls12_no_resume_after_server_downgrade() {
    let mut client_config = common::make_client_config(KeyType::Ed25519);
    let client_storage = Arc::new(ClientStorage::new());
    client_config.session_storage = client_storage.clone();
    let client_config = Arc::new(client_config);

    let server_config_1 = Arc::new(common::finish_server_config(
        KeyType::Ed25519,
        ServerConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap(),
    ));

    let mut server_config_2 = common::finish_server_config(
        KeyType::Ed25519,
        ServerConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap(),
    );
    server_config_2.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});

    dbg!("handshake 1");
    let mut client_1 =
        ClientConnection::new(client_config.clone(), "localhost".try_into().unwrap()).unwrap();
    client_1.start().await.unwrap();
    let mut server_1 = ServerConnection::new(server_config_1).unwrap();
    common::do_handshake(&mut client_1, &mut server_1).await;
    assert_eq!(client_storage.puts(), 2);

    dbg!("handshake 2");
    let mut client_2 =
        ClientConnection::new(client_config, "localhost".try_into().unwrap()).unwrap();
    client_2.start().await.unwrap();
    let mut server_2 = ServerConnection::new(Arc::new(server_config_2)).unwrap();
    common::do_handshake(&mut client_2, &mut server_2).await;
    assert_eq!(client_storage.puts(), 2);
}

#[derive(Default, Debug)]
struct LogCounts {
    trace: usize,
    debug: usize,
    info: usize,
    warn: usize,
    error: usize,
}

impl LogCounts {
    fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn add(&mut self, level: log::Level) {
        match level {
            log::Level::Trace => self.trace += 1,
            log::Level::Debug => self.debug += 1,
            log::Level::Info => self.info += 1,
            log::Level::Warn => self.warn += 1,
            log::Level::Error => self.error += 1,
        }
    }
}

thread_local!(static COUNTS: RefCell<LogCounts> = RefCell::new(LogCounts::new()));

struct CountingLogger;

static LOGGER: CountingLogger = CountingLogger;

impl CountingLogger {
    fn install() {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    }

    fn reset() {
        COUNTS.with(|c| {
            c.borrow_mut().reset();
        });
    }
}

impl log::Log for CountingLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        println!("logging at {:?}: {:?}", record.level(), record.args());

        COUNTS.with(|c| {
            c.borrow_mut().add(record.level());
        });
    }

    fn flush(&self) {}
}

#[tokio::test]
async fn test_no_warning_logging_during_successful_sessions() {
    CountingLogger::install();
    CountingLogger::reset();

    for kt in ALL_KEY_TYPES.iter() {
        for version in tls_aio::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version]);
            let (mut client, mut server) =
                make_pair_for_configs(client_config, make_server_config(*kt)).await;
            do_handshake(&mut client, &mut server).await;
        }
    }

    if cfg!(feature = "logging") {
        COUNTS.with(|c| {
            println!("After tests: {:?}", c.borrow());
            assert_eq!(c.borrow().warn, 0);
            assert_eq!(c.borrow().error, 0);
            assert_eq!(c.borrow().info, 0);
            assert!(c.borrow().trace > 0);
            assert!(c.borrow().debug > 0);
        });
    } else {
        COUNTS.with(|c| {
            println!("After tests: {:?}", c.borrow());
            assert_eq!(c.borrow().warn, 0);
            assert_eq!(c.borrow().error, 0);
            assert_eq!(c.borrow().info, 0);
            assert_eq!(c.borrow().trace, 0);
            assert_eq!(c.borrow().debug, 0);
        });
    }
}
