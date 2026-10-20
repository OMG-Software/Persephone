//! TLS 1.3-only policy: the owner-approved requirement is checked on the wire.
//! A loopback listener captures the real ClientHello, so the assertion covers
//! what rustls offers, not just how the builder was configured.

use persephone_test_matrix::policy_http_client_builder;
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    thread,
    time::Duration,
};

const TLS_1_2: u16 = 0x0303;
const TLS_1_3: u16 = 0x0304;
const EXT_SUPPORTED_VERSIONS: u16 = 0x002b;
// TLS_AES_128_GCM_SHA256, TLS_AES_256_GCM_SHA384, TLS_CHACHA20_POLY1305_SHA256.
const TLS_1_3_SUITES: [u16; 3] = [0x1301, 0x1302, 0x1303];

#[derive(Debug)]
struct ClientHello {
    supported_versions: Vec<u16>,
    cipher_suites: Vec<u16>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        if self.bytes.len() < count {
            return Err(format!(
                "truncated: need {count}, have {}",
                self.bytes.len()
            ));
        }
        let (head, tail) = self.bytes.split_at(count);
        self.bytes = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<usize, String> {
        Ok(self.take(1)?[0].into())
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn prefixed16(&mut self) -> Result<&'a [u8], String> {
        let length = usize::from(self.u16()?);
        self.take(length)
    }

    fn prefixed24(&mut self) -> Result<&'a [u8], String> {
        let length = self.u24()?;
        self.take(length)
    }

    fn u24(&mut self) -> Result<usize, String> {
        let bytes = self.take(3)?;
        Ok(usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2]))
    }
}

fn u16_list(bytes: &[u8]) -> Result<Vec<u16>, String> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return Err("odd-length u16 list".into());
    }
    Ok(pairs.iter().map(|pair| u16::from_be_bytes(*pair)).collect())
}

/// Parses one TLS record holding a complete ClientHello (RFC 8446 §4.1.2).
fn parse_client_hello(record: &[u8]) -> Result<ClientHello, String> {
    let mut record = Cursor { bytes: record };
    if record.u8()? != 0x16 {
        return Err("not a handshake record".into());
    }
    record.u16()?; // legacy record version
    let record_length = usize::from(record.u16()?);
    let mut handshake = Cursor {
        bytes: record.take(record_length)?,
    };
    if handshake.u8()? != 0x01 {
        return Err("not a ClientHello".into());
    }
    let mut hello = Cursor {
        bytes: handshake.prefixed24()?,
    };
    hello.u16()?; // legacy_version, fixed at TLS 1.2 by RFC 8446
    hello.take(32)?; // random
    let session_id_length = hello.u8()?;
    hello.take(session_id_length)?;
    let suites_length = usize::from(hello.u16()?);
    let cipher_suites = u16_list(hello.take(suites_length)?)?;
    let compression_length = hello.u8()?;
    hello.take(compression_length)?;
    let mut extensions = Cursor {
        bytes: hello.prefixed16()?,
    };
    let mut supported_versions = None;
    while !extensions.bytes.is_empty() {
        let kind = extensions.u16()?;
        let data = extensions.prefixed16()?;
        if kind == EXT_SUPPORTED_VERSIONS {
            let mut data = Cursor { bytes: data };
            let list_length = data.u8()?;
            supported_versions = Some(u16_list(data.take(list_length)?)?);
        }
    }
    Ok(ClientHello {
        supported_versions: supported_versions.ok_or("no supported_versions extension")?,
        cipher_suites,
    })
}

/// Sends one HTTPS request from `builder` to a loopback listener that records
/// the first TLS record and then closes, failing the handshake by design.
async fn capture_client_hello(builder: reqwest::ClientBuilder) -> ClientHello {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let capture = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut record = vec![0; 5];
        stream.read_exact(&mut record).unwrap();
        let body_length = usize::from(u16::from_be_bytes([record[3], record[4]]));
        record.resize(5 + body_length, 0);
        stream.read_exact(&mut record[5..]).unwrap();
        let _ = stream.shutdown(Shutdown::Both);
        record
    });
    let client = builder.timeout(Duration::from_secs(5)).build().unwrap();
    let result = client
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .await;
    assert!(result.is_err(), "the capture listener never completes TLS");
    parse_client_hello(&capture.join().unwrap()).unwrap()
}

/// rustls 0.23.46 restricts `supported_versions` but still lists TLS 1.2 cipher
/// suites; that is a fingerprint detail, not a negotiable version. Refusal of
/// a TLS 1.2 server is checked separately below.
#[tokio::test]
async fn given_policy_client_when_connecting_then_client_hello_offers_only_tls_1_3() {
    let hello = capture_client_hello(policy_http_client_builder()).await;
    assert_eq!(hello.supported_versions, [TLS_1_3]);
    assert!(
        TLS_1_3_SUITES
            .iter()
            .all(|suite| hello.cipher_suites.contains(suite)),
        "TLS 1.3 cipher suites missing: {:04x?}",
        hello.cipher_suites
    );
}

const ALERT_RECORD: u8 = 0x15;
const ALERT_FATAL: u8 = 0x02;
const ALERT_PROTOCOL_VERSION: u8 = 70;

/// A minimal TLS 1.2 ServerHello: legacy version 1.2, no supported_versions
/// extension, ECDHE-RSA-AES128-GCM-SHA256 selected.
fn tls_1_2_server_hello() -> Vec<u8> {
    let mut body = vec![0x03, 0x03];
    body.extend([0x5a; 32]); // random, without a TLS 1.3 downgrade sentinel
    body.push(0x00); // empty session id
    body.extend([0xc0, 0x2f]);
    body.push(0x00); // null compression
    body.extend([0x00, 0x00]); // empty extensions
    let mut handshake = vec![0x02];
    handshake.extend(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend(body);
    let mut record = vec![0x16, 0x03, 0x03];
    record.extend((handshake.len() as u16).to_be_bytes());
    record.extend(handshake);
    record
}

/// Answers the ClientHello with a TLS 1.2 ServerHello and returns whatever the
/// client sends back before closing or going quiet.
async fn client_reaction_to_tls_1_2(builder: reqwest::ClientBuilder) -> Vec<u8> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = [0; 5];
        stream.read_exact(&mut header).unwrap();
        let mut hello = vec![0; usize::from(u16::from_be_bytes([header[3], header[4]]))];
        stream.read_exact(&mut hello).unwrap();
        stream.write_all(&tls_1_2_server_hello()).unwrap();
        let mut reaction = Vec::new();
        let mut buffer = [0; 256];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 || reaction.len() > 4096 {
                break;
            }
            reaction.extend(&buffer[..read]);
        }
        let _ = stream.shutdown(Shutdown::Both);
        reaction
    });
    let client = builder.timeout(Duration::from_secs(5)).build().unwrap();
    let result = client
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .await;
    assert!(result.is_err(), "the fake server never completes TLS");
    server.join().unwrap()
}

fn sent_protocol_version_alert(reaction: &[u8]) -> bool {
    reaction.windows(7).any(|record| {
        record[0] == ALERT_RECORD
            && record[3..5] == [0x00, 0x02]
            && record[5] == ALERT_FATAL
            && record[6] == ALERT_PROTOCOL_VERSION
    })
}

#[tokio::test]
async fn given_policy_client_when_server_selects_tls_1_2_then_client_aborts_with_protocol_version()
{
    let reaction = client_reaction_to_tls_1_2(policy_http_client_builder()).await;
    assert!(
        sent_protocol_version_alert(&reaction),
        "expected a fatal protocol_version alert, got {reaction:02x?}"
    );
}

/// Positive control: an unrestricted client accepts the TLS 1.2 ServerHello and
/// waits for the certificate instead of raising protocol_version.
#[tokio::test]
async fn given_unrestricted_client_when_server_selects_tls_1_2_then_no_version_alert_is_sent() {
    let reaction = client_reaction_to_tls_1_2(reqwest::Client::builder().no_proxy()).await;
    assert!(!sent_protocol_version_alert(&reaction));
}

/// Positive control: without the policy the same capture sees TLS 1.2 offered,
/// so the assertion above would detect a regression.
#[tokio::test]
async fn given_unrestricted_reqwest_client_when_connecting_then_tls_1_2_is_offered() {
    let hello = capture_client_hello(reqwest::Client::builder().no_proxy()).await;
    assert!(hello.supported_versions.contains(&TLS_1_3));
    assert!(hello.supported_versions.contains(&TLS_1_2));
    assert!(
        hello
            .cipher_suites
            .iter()
            .any(|suite| !TLS_1_3_SUITES.contains(suite))
    );
}

#[test]
fn given_truncated_or_foreign_records_then_parser_rejects_them() {
    for record in [
        &[][..],
        &[0x17, 0x03, 0x03, 0x00, 0x00][..],
        &[0x16, 0x03, 0x01, 0x00, 0x10, 0x01][..],
        &[0x16, 0x03, 0x01, 0x00, 0x04, 0x02, 0x00, 0x00, 0x00][..],
    ] {
        assert!(parse_client_hello(record).is_err());
    }
}
