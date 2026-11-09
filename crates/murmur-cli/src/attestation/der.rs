//! The DER reading the attestation check needs and nothing more: walking the TLVs of an X.509
//! certificate to its subject public key, its extensions and its URI subject alternative names.
//!
//! Chain and signature validation are not done here; `rustls-webpki` does them. This reader only
//! pulls out the fields webpki does not expose: the Fulcio identity extensions and the SAN URI.

pub(crate) const TAG_BOOLEAN: u8 = 0x01;
pub(crate) const TAG_BIT_STRING: u8 = 0x03;
pub(crate) const TAG_OCTET_STRING: u8 = 0x04;
pub(crate) const TAG_OID: u8 = 0x06;
pub(crate) const TAG_UTF8_STRING: u8 = 0x0c;
pub(crate) const TAG_SEQUENCE: u8 = 0x30;
/// `[0] EXPLICIT`: the certificate version.
const TAG_VERSION: u8 = 0xa0;
/// `[3] EXPLICIT`: the certificate extensions.
const TAG_EXTENSIONS: u8 = 0xa3;
/// `[6] IMPLICIT IA5String`: a URI general name.
const TAG_GENERAL_NAME_URI: u8 = 0x86;

/// `1.2.840.10045.2.1`, id-ecPublicKey.
pub(crate) const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// `1.2.840.10045.3.1.7`, the P-256 curve.
pub(crate) const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// `2.5.29.17`, subjectAltName.
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
/// `1.3.6.1.4.1.57264.1`, the arc Fulcio's certificate extensions live under.
const OID_FULCIO_ARC: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01];

/// One TLV read from DER: its tag and its content bytes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tlv<'a> {
    pub(crate) tag: u8,
    pub(crate) content: &'a [u8],
}

/// A cursor over a run of DER TLVs.
pub(crate) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    /// The tag of the next TLV, without consuming it.
    pub(crate) fn peek_tag(&self) -> Option<u8> {
        self.rest.first().copied()
    }

    /// Reads the next TLV. Only single-byte tags and definite lengths of up to four bytes are
    /// read, which covers every certificate field this module looks at.
    pub(crate) fn read(&mut self) -> Result<Tlv<'a>, String> {
        let (&tag, rest) = self.rest.split_first().ok_or("truncated DER")?;
        if tag & 0x1f == 0x1f {
            return Err("multi-byte DER tag".to_string());
        }
        let (&first, mut rest) = rest.split_first().ok_or("truncated DER length")?;
        let length = if first < 0x80 {
            usize::from(first)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 || rest.len() < count {
                return Err("unsupported DER length".to_string());
            }
            let (digits, after) = rest.split_at(count);
            rest = after;
            digits
                .iter()
                .fold(0usize, |acc, &digit| (acc << 8) | usize::from(digit))
        };
        if rest.len() < length {
            return Err("DER value runs past its container".to_string());
        }
        let (content, after) = rest.split_at(length);
        self.rest = after;
        Ok(Tlv { tag, content })
    }

    /// Reads the next TLV and requires its tag to be `tag`.
    pub(crate) fn expect(&mut self, tag: u8) -> Result<&'a [u8], String> {
        let tlv = self.read()?;
        if tlv.tag != tag {
            return Err(format!(
                "expected DER tag {tag:#04x}, found {:#04x}",
                tlv.tag
            ));
        }
        Ok(tlv.content)
    }
}

/// The fields of a certificate the attestation check reads.
#[derive(Debug)]
pub(crate) struct Certificate<'a> {
    /// The subject public key algorithm OID and its parameter (the curve OID for an EC key).
    pub(crate) key_algorithm: &'a [u8],
    pub(crate) key_parameter: Option<&'a [u8]>,
    /// The subject public key bit string's bytes, the unused-bits byte removed.
    pub(crate) public_key: &'a [u8],
    /// Every extension as (OID, value), the value the extnValue OCTET STRING's content.
    pub(crate) extensions: Vec<(&'a [u8], &'a [u8])>,
}

/// Reads `der` as an X.509 certificate.
pub(crate) fn parse_certificate(der: &[u8]) -> Result<Certificate<'_>, String> {
    let mut outer = Reader::new(der);
    let certificate = outer.expect(TAG_SEQUENCE)?;
    let mut certificate = Reader::new(certificate);
    let mut tbs = Reader::new(certificate.expect(TAG_SEQUENCE)?);
    if tbs.peek_tag() == Some(TAG_VERSION) {
        tbs.read()?;
    }
    tbs.read()?; // serialNumber
    tbs.expect(TAG_SEQUENCE)?; // signature
    tbs.expect(TAG_SEQUENCE)?; // issuer
    tbs.expect(TAG_SEQUENCE)?; // validity
    tbs.expect(TAG_SEQUENCE)?; // subject
    let mut spki = Reader::new(tbs.expect(TAG_SEQUENCE)?);
    let mut algorithm = Reader::new(spki.expect(TAG_SEQUENCE)?);
    let key_algorithm = algorithm.expect(TAG_OID)?;
    let key_parameter = match algorithm.read() {
        Ok(tlv) if tlv.tag == TAG_OID => Some(tlv.content),
        _ => None,
    };
    let bits = spki.expect(TAG_BIT_STRING)?;
    let public_key = match bits.split_first() {
        Some((0, key)) => key,
        _ => return Err("subject public key has unused bits".to_string()),
    };

    let mut extensions = Vec::new();
    while !tbs.is_empty() {
        let tlv = tbs.read()?;
        if tlv.tag != TAG_EXTENSIONS {
            continue;
        }
        let mut list = Reader::new(Reader::new(tlv.content).expect(TAG_SEQUENCE)?);
        while !list.is_empty() {
            let mut extension = Reader::new(list.expect(TAG_SEQUENCE)?);
            let oid = extension.expect(TAG_OID)?;
            if extension.peek_tag() == Some(TAG_BOOLEAN) {
                extension.read()?;
            }
            let value = extension.expect(TAG_OCTET_STRING)?;
            extensions.push((oid, value));
        }
    }
    Ok(Certificate {
        key_algorithm,
        key_parameter,
        public_key,
        extensions,
    })
}

impl Certificate<'_> {
    /// The value of Fulcio extension `1.3.6.1.4.1.57264.1.<arc>`, read as the DER UTF8String the
    /// arcs from 8 onward carry. `None` when the extension is absent or not a UTF8String.
    pub(crate) fn fulcio_extension(&self, arc: u8) -> Option<String> {
        let value = self.extensions.iter().find_map(|(oid, value)| {
            let tail = oid.strip_prefix(OID_FULCIO_ARC)?;
            (tail == [arc]).then_some(*value)
        })?;
        let content = Reader::new(value).expect(TAG_UTF8_STRING).ok()?;
        String::from_utf8(content.to_vec()).ok()
    }

    /// The URI entries of the subjectAltName extension, in order.
    pub(crate) fn san_uris(&self) -> Vec<String> {
        let Some(value) = self
            .extensions
            .iter()
            .find_map(|(oid, value)| (*oid == OID_SUBJECT_ALT_NAME).then_some(*value))
        else {
            return Vec::new();
        };
        let Ok(names) = Reader::new(value).expect(TAG_SEQUENCE) else {
            return Vec::new();
        };
        let mut names = Reader::new(names);
        let mut uris = Vec::new();
        while let Ok(name) = names.read() {
            if name.tag == TAG_GENERAL_NAME_URI {
                if let Ok(uri) = std::str::from_utf8(name.content) {
                    uris.push(uri.to_string());
                }
            }
        }
        uris
    }
}

/// The public key bytes of a DER SubjectPublicKeyInfo, the unused-bits byte removed. The
/// algorithm is not read: a trusted root names it in `keyDetails`.
pub(crate) fn parse_spki(der: &[u8]) -> Result<&[u8], String> {
    let mut spki = Reader::new(Reader::new(der).expect(TAG_SEQUENCE)?);
    spki.expect(TAG_SEQUENCE)?;
    let bits = spki.expect(TAG_BIT_STRING)?;
    match bits.split_first() {
        Some((0, key)) => Ok(key),
        _ => Err("public key has unused bits".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_form_lengths_are_read_and_overruns_refused() {
        let mut long = vec![0x04, 0x81, 0x80];
        long.extend([7u8; 0x80]);
        let tlv = Reader::new(&long).read().unwrap();
        assert_eq!((tlv.tag, tlv.content.len()), (0x04, 0x80));

        assert!(Reader::new(&[0x04, 0x05, 1, 2]).read().is_err());
        assert!(Reader::new(&[0x1f, 0x01, 0x00]).read().is_err());
        assert!(Reader::new(&[0x04, 0x85, 0, 0, 0, 0, 1]).read().is_err());
    }
}
