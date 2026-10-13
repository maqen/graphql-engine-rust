//! Just enough PEM and DER to read public keys: SubjectPublicKeyInfo, PKCS#1 RSA public
//! keys and the key inside an X.509 certificate. Signature checks are done by `ring`.

use base64::Engine as _;

/// A public key, in the form `ring` verifies with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicKey {
    /// Modulus and exponent, big-endian without leading zeros.
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// Uncompressed SEC1 point (`04 || x || y`).
    EcP256(Vec<u8>),
    EcP384(Vec<u8>),
    EcP521(Vec<u8>),
    Ed25519(Vec<u8>),
}

const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
const OID_P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OID: u8 = 0x06;

/// Parse the first PEM block in `text` into a public key.
pub(crate) fn parse_pem(text: &str) -> Result<PublicKey, String> {
    let begin = text.find("-----BEGIN ").ok_or("no PEM header found")?;
    let rest = &text[begin + 11..];
    let label_end = rest.find("-----").ok_or("malformed PEM header")?;
    let label = &rest[..label_end];
    let body = &rest[label_end + 5..];
    let end_marker = format!("-----END {label}-----");
    let end = body.find(&end_marker).ok_or("no PEM footer found")?;
    let b64: String = body[..end]
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("invalid base64 in PEM: {e}"))?;
    match label {
        "PUBLIC KEY" => parse_spki(&der),
        "RSA PUBLIC KEY" => parse_pkcs1(&der),
        "CERTIFICATE" => parse_certificate(&der),
        other => Err(format!("unsupported PEM type: {other}")),
    }
}

/// One DER element: tag, contents and the whole encoding.
struct Tlv<'a> {
    tag: u8,
    contents: &'a [u8],
    raw: &'a [u8],
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn next(&mut self) -> Result<Tlv<'a>, String> {
        let input = self.0;
        let (&tag, rest) = input.split_first().ok_or("truncated DER")?;
        let (&first, mut rest) = rest.split_first().ok_or("truncated DER")?;
        let len = if first < 0x80 {
            usize::from(first)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return Err("unsupported DER length".into());
            }
            let len = rest[..n]
                .iter()
                .fold(0usize, |l, &b| (l << 8) | usize::from(b));
            rest = &rest[n..];
            len
        };
        if rest.len() < len {
            return Err("truncated DER".into());
        }
        let header = input.len() - rest.len();
        self.0 = &rest[len..];
        Ok(Tlv {
            tag,
            contents: &rest[..len],
            raw: &input[..header + len],
        })
    }

    fn expect(&mut self, tag: u8) -> Result<&'a [u8], String> {
        let tlv = self.next()?;
        if tlv.tag != tag {
            return Err(format!("unexpected DER tag {:#04x}", tlv.tag));
        }
        Ok(tlv.contents)
    }
}

/// `SubjectPublicKeyInfo ::= SEQUENCE { algorithm AlgorithmIdentifier, subjectPublicKey BIT STRING }`
fn parse_spki(der: &[u8]) -> Result<PublicKey, String> {
    let mut outer = Reader(Reader(der).expect(SEQUENCE)?);
    let mut alg = Reader(outer.expect(SEQUENCE)?);
    let oid = alg.expect(OID)?;
    let bits = outer.expect(BIT_STRING)?;
    let key = match bits.split_first() {
        Some((0, key)) => key,
        _ => return Err("malformed public key bit string".into()),
    };
    match oid {
        OID_RSA => parse_pkcs1(key),
        OID_EC => {
            let curve = alg.expect(OID)?;
            match curve {
                OID_P256 => Ok(PublicKey::EcP256(key.to_vec())),
                OID_P384 => Ok(PublicKey::EcP384(key.to_vec())),
                OID_P521 => Ok(PublicKey::EcP521(key.to_vec())),
                _ => Err("unsupported elliptic curve".into()),
            }
        }
        OID_ED25519 => Ok(PublicKey::Ed25519(key.to_vec())),
        _ => Err("unsupported public key algorithm".into()),
    }
}

/// `RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }`
fn parse_pkcs1(der: &[u8]) -> Result<PublicKey, String> {
    let mut seq = Reader(Reader(der).expect(SEQUENCE)?);
    let n = strip_zeros(seq.expect(INTEGER)?);
    let e = strip_zeros(seq.expect(INTEGER)?);
    Ok(PublicKey::Rsa {
        n: n.to_vec(),
        e: e.to_vec(),
    })
}

/// The SubjectPublicKeyInfo inside `Certificate ::= SEQUENCE { tbsCertificate, ... }`.
fn parse_certificate(der: &[u8]) -> Result<PublicKey, String> {
    let mut cert = Reader(Reader(der).expect(SEQUENCE)?);
    let mut tbs = Reader(cert.expect(SEQUENCE)?);
    let mut field = tbs.next()?;
    if field.tag == 0xa0 {
        // [0] EXPLICIT version
        field = tbs.next()?;
    }
    // serialNumber (already read), signature, issuer, validity, subject, subjectPublicKeyInfo
    for _ in 0..4 {
        field = tbs.next()?;
    }
    let _ = field;
    let spki = tbs.next()?;
    if spki.tag != SEQUENCE {
        return Err("malformed certificate".into());
    }
    parse_spki(spki.raw)
}

pub(crate) fn strip_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSA_PUB: &str = include_str!("../tests/fixtures/rsa_public.pem");
    const RSA_PKCS1: &str = include_str!("../tests/fixtures/rsa_public_pkcs1.pem");
    const RSA_CERT: &str = include_str!("../tests/fixtures/rsa_cert.pem");
    const EC_PUB: &str = include_str!("../tests/fixtures/ec_public.pem");
    const ED_PUB: &str = include_str!("../tests/fixtures/ed_public.pem");

    #[test]
    fn rsa_spki() {
        let PublicKey::Rsa { n, e } = parse_pem(RSA_PUB).unwrap() else {
            panic!("not rsa")
        };
        assert_eq!(n.len(), 256);
        assert_eq!(e, vec![1, 0, 1]);
        let from_pkcs1 = parse_pem(RSA_PKCS1).unwrap();
        let from_cert = parse_pem(RSA_CERT).unwrap();
        assert_eq!(
            from_pkcs1,
            PublicKey::Rsa {
                n: n.clone(),
                e: e.clone()
            }
        );
        assert_eq!(from_cert, PublicKey::Rsa { n, e });
    }

    #[test]
    fn ec_and_ed25519() {
        assert!(matches!(parse_pem(EC_PUB).unwrap(), PublicKey::EcP256(p) if p.len() == 65));
        assert!(matches!(parse_pem(ED_PUB).unwrap(), PublicKey::Ed25519(p) if p.len() == 32));
    }

    #[test]
    fn bad_pem() {
        assert!(parse_pem("nope").is_err());
        assert!(parse_pem("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----").is_err());
    }
}
