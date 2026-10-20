//! The few DER structures ACME needs: a PKCS#10 certificate signing request
//! for one DNS name, the `notAfter` of a certificate, and the PKCS#8 wrapper
//! of a SEC1 EC key (what Forge-Go's autocert cache stores).
//!
//! This is encoding only. Keys, signatures and digests come from `ring`;
//! no cryptographic primitive is implemented here.

use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const UTF8_STRING: u8 = 0x0c;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const CONTEXT_0_CONSTRUCTED: u8 = 0xa0;
const DNS_NAME: u8 = 0x82;

const OID_COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_EXTENSION_REQUEST: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x0e,
];
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x1d, 0x11];
const OID_ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];

/// One TLV.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 4);
    out.push(tag);
    let length = content.len();
    if length < 0x80 {
        out.push(u8::try_from(length).unwrap_or(0));
    } else {
        let bytes = length.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | u8::try_from(bytes.len() - skip).unwrap_or(0));
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// A PKCS#10 request for `host` (common name and the only subject
/// alternative name), signed with `key`.
///
/// # Errors
///
/// Signing failed.
pub fn certificate_request(host: &str, key: &EcdsaKeyPair) -> Result<Vec<u8>, String> {
    let version = tlv(INTEGER, &[0]);
    let common_name = tlv(
        SEQUENCE,
        &concat(&[OID_COMMON_NAME, &tlv(UTF8_STRING, host.as_bytes())]),
    );
    let subject = tlv(SEQUENCE, &tlv(SET, &common_name));
    let algorithm = tlv(SEQUENCE, &concat(&[OID_EC_PUBLIC_KEY, OID_P256]));
    let mut public_key = vec![0_u8];
    public_key.extend_from_slice(key.public_key().as_ref());
    let subject_public_key_info = tlv(
        SEQUENCE,
        &concat(&[&algorithm, &tlv(BIT_STRING, &public_key)]),
    );
    let names = tlv(SEQUENCE, &tlv(DNS_NAME, host.as_bytes()));
    let extension = tlv(
        SEQUENCE,
        &concat(&[OID_SUBJECT_ALT_NAME, &tlv(OCTET_STRING, &names)]),
    );
    let extensions = tlv(SEQUENCE, &extension);
    let attribute = tlv(
        SEQUENCE,
        &concat(&[OID_EXTENSION_REQUEST, &tlv(SET, &extensions)]),
    );
    let attributes = tlv(CONTEXT_0_CONSTRUCTED, &attribute);
    let info = tlv(
        SEQUENCE,
        &concat(&[&version, &subject, &subject_public_key_info, &attributes]),
    );
    let signature = key
        .sign(&SystemRandom::new(), &info)
        .map_err(|_| "sign certificate request".to_owned())?;
    let mut signature_bits = vec![0_u8];
    signature_bits.extend_from_slice(signature.as_ref());
    Ok(tlv(
        SEQUENCE,
        &concat(&[
            &info,
            &tlv(SEQUENCE, OID_ECDSA_SHA256),
            &tlv(BIT_STRING, &signature_bits),
        ]),
    ))
}

/// A new P-256 key in PKCS#8 form.
///
/// # Errors
///
/// The system random source failed.
pub fn generate_p256() -> Result<Vec<u8>, String> {
    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
        .map(|document| document.as_ref().to_vec())
        .map_err(|_| "generate key".to_owned())
}

/// Wraps a SEC1 `ECPrivateKey` (PEM `EC PRIVATE KEY`) of a P-256 key in
/// PKCS#8.
#[must_use]
pub fn sec1_to_pkcs8(sec1: &[u8]) -> Vec<u8> {
    let algorithm = tlv(SEQUENCE, &concat(&[OID_EC_PUBLIC_KEY, OID_P256]));
    tlv(
        SEQUENCE,
        &concat(&[&tlv(INTEGER, &[0]), &algorithm, &tlv(OCTET_STRING, sec1)]),
    )
}

/// A DER reader over one buffer.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let (&tag, rest) = self.data.split_first()?;
        let (&first, mut rest) = rest.split_first()?;
        let length = if first < 0x80 {
            usize::from(first)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 || rest.len() < count {
                return None;
            }
            let mut length = 0_usize;
            for byte in &rest[..count] {
                length = (length << 8) | usize::from(*byte);
            }
            rest = &rest[count..];
            length
        };
        if rest.len() < length {
            return None;
        }
        let (content, after) = rest.split_at(length);
        self.data = after;
        Some((tag, content))
    }
}

/// The `notAfter` of a DER certificate, as seconds since the Unix epoch.
#[must_use]
pub fn not_after(certificate: &[u8]) -> Option<i64> {
    let (tag, certificate) = Reader { data: certificate }.next()?;
    if tag != SEQUENCE {
        return None;
    }
    let (tag, tbs) = Reader { data: certificate }.next()?;
    if tag != SEQUENCE {
        return None;
    }
    let mut fields = Reader { data: tbs };
    let mut field = fields.next()?;
    if field.0 == CONTEXT_0_CONSTRUCTED {
        // Explicit version.
        field = fields.next()?;
    }
    // field is the serial number.
    if field.0 != INTEGER {
        return None;
    }
    let _signature = fields.next()?;
    let _issuer = fields.next()?;
    let (tag, validity) = fields.next()?;
    if tag != SEQUENCE {
        return None;
    }
    let mut times = Reader { data: validity };
    let _not_before = times.next()?;
    let (tag, text) = times.next()?;
    parse_time(tag, std::str::from_utf8(text).ok()?)
}

fn parse_time(tag: u8, text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (year, rest) = match tag {
        UTC_TIME if text.len() == 12 => {
            let short: i64 = text[..2].parse().ok()?;
            (
                if short >= 50 {
                    1900 + short
                } else {
                    2000 + short
                },
                &text[2..],
            )
        }
        GENERALIZED_TIME if text.len() == 14 => (text[..4].parse().ok()?, &text[4..]),
        _ => return None,
    };
    if !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let stamp = crate::clock::Timestamp::parse_rfc3339(&format!(
        "{year:04}-{}-{}T{}:{}:{}Z",
        &rest[0..2],
        &rest[2..4],
        &rest[4..6],
        &rest[6..8],
        &rest[8..10]
    ))?;
    Some(stamp.unix_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_use_the_shortest_form() {
        assert_eq!(tlv(0x04, &[1, 2]), [0x04, 2, 1, 2]);
        let long = tlv(0x04, &[0; 200]);
        assert_eq!(&long[..3], &[0x04, 0x81, 200]);
        let longer = tlv(0x04, &[0; 300]);
        assert_eq!(&longer[..4], &[0x04, 0x82, 0x01, 0x2c]);
    }

    #[test]
    fn certificate_request_carries_the_name_and_key() {
        let pkcs8 = generate_p256().expect("key");
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .expect("parse key");
        let csr = certificate_request("app.example.com", &key).expect("csr");
        let (tag, body) = Reader { data: &csr }.next().expect("outer");
        assert_eq!(tag, SEQUENCE);
        let mut parts = Reader { data: body };
        let (_, info) = parts.next().expect("info");
        let (_, algorithm) = parts.next().expect("algorithm");
        assert_eq!(algorithm, OID_ECDSA_SHA256);
        let (tag, signature) = parts.next().expect("signature");
        assert_eq!(tag, BIT_STRING);
        // The signature verifies over the encoded request info.
        let info_der = tlv(SEQUENCE, info);
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_ASN1,
            key.public_key().as_ref(),
        )
        .verify(&info_der, &signature[1..])
        .expect("signature verifies");
        assert!(
            csr.windows(b"app.example.com".len())
                .filter(|window| *window == b"app.example.com")
                .count()
                == 2,
            "name in subject and SAN"
        );
    }

    #[test]
    fn utc_and_generalized_times() {
        assert_eq!(parse_time(UTC_TIME, "261019120000Z"), Some(1_792_411_200));
        assert_eq!(
            parse_time(GENERALIZED_TIME, "20261019120000Z"),
            Some(1_792_411_200)
        );
        assert_eq!(parse_time(UTC_TIME, "991231235959Z"), Some(946_684_799));
        assert_eq!(parse_time(UTC_TIME, "2610191200Z"), None);
    }
}
