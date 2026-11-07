//! Bounded TPM2 wire subset used only with a fresh disposable fixture TPM.
//! Command layouts/attributes follow TCG TPM2 Part2/Part3 and tpm2-tss SAPI.
use crate::{fail, hash, require, Cursor, Failure, Result};

pub const MAX_PACKET: usize = 4096;
pub const MAX_BUNDLE: usize = 1536;
pub const NV_INDEX: u32 = 0x0150_0016;
const NV_ATTRS: u32 = 0x0202_0002; // NO_DA | OWNERREAD | OWNERWRITE, ordinary index
const SHA256: u16 = 0x000b;
const ALG_NULL: u16 = 0x0010;
const OWNER: u32 = 0x4000_0001;
const NULL: u32 = 0x4000_0007;
const PASSWORD: u32 = 0x4000_0009;
const PARENT_ATTRS: u32 = 0x0003_0072;
// FIXEDTPM | FIXEDPARENT | ADMINWITHPOLICY | NODA. In particular USERWITHAUTH
// and SENSITIVEDATAORIGIN are clear: only policy may authorize the sealed value.
pub const SEALED_ATTRS: u32 = 0x0000_0492;
const PCR7: [u8; 10] = [0, 0, 0, 1, 0, 0x0b, 3, 0x80, 0, 0];

fn u16b(v: &mut Vec<u8>, n: u16) {
    v.extend(n.to_be_bytes());
}
fn u32b(v: &mut Vec<u8>, n: u32) {
    v.extend(n.to_be_bytes());
}
fn blob(v: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    require(data.len() <= u16::MAX as usize, "blob_length")?;
    u16b(v, data.len() as u16);
    v.extend(data);
    Ok(())
}

pub trait Transport {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>>;
}
pub struct Tpm<T>(pub T);
pub struct Response {
    pub handles: Vec<u32>,
    pub params: Vec<u8>,
}

fn command(cc: u32, handles: &[u32], auth: Option<u32>, params: &[u8]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for h in handles {
        u32b(&mut body, *h);
    }
    if let Some(handle) = auth {
        u32b(&mut body, 9); // one empty nonce/auth; no parameter encryption
        u32b(&mut body, handle);
        u16b(&mut body, 0);
        body.push(0); // successful policy authorization consumes the session
        u16b(&mut body, 0);
    }
    body.extend(params);
    require(body.len() + 10 <= MAX_PACKET, "oversized_command")?;
    let mut out = Vec::new();
    u16b(&mut out, if auth.is_some() { 0x8002 } else { 0x8001 });
    u32b(&mut out, (body.len() + 10) as u32);
    u32b(&mut out, cc);
    out.extend(body);
    Ok(out)
}

fn response(
    raw: &[u8],
    handles: usize,
    auth: Option<u32>,
    stage: &'static str,
) -> Result<Response> {
    require(raw.len() <= MAX_PACKET, "oversized_response")?;
    let mut c = Cursor::new(raw);
    let tag = c.be16()?;
    require(
        matches!(tag, 0x8001 | 0x8002) && c.be32()? as usize == raw.len(),
        "response_header",
    )?;
    let rc = c.be32()?;
    if rc != 0 {
        require(tag == 0x8001 && c.left() == 0, "malformed_error_response")?;
        return Err(Failure {
            stage,
            rc: Some(rc),
        });
    }
    require(
        tag == if auth.is_some() { 0x8002 } else { 0x8001 },
        "response_session_tag",
    )?;
    let mut hs = Vec::new();
    for _ in 0..handles {
        hs.push(c.be32()?);
    }
    let params = if let Some(handle) = auth {
        let n = c.be32()? as usize;
        let p = c.take(n)?.to_vec();
        let nonce = c.blob(64)?;
        require(
            nonce.len() == if handle == PASSWORD { 0 } else { 32 },
            "response_nonce_size",
        )?;
        require(
            c.byte()? == if handle == PASSWORD { 1 } else { 0 },
            "response_session_attributes",
        )?;
        require(c.blob(64)?.is_empty(), "unexpected_response_hmac")?;
        p
    } else {
        c.take(c.left())?.to_vec()
    };
    c.finish()?;
    Ok(Response {
        handles: hs,
        params,
    })
}

fn name(public: &[u8]) -> Vec<u8> {
    let mut out = SHA256.to_be_bytes().to_vec();
    out.extend(hash(public));
    out
}
fn parent_template() -> Result<Vec<u8>> {
    let mut p = Vec::new();
    u16b(&mut p, 0x0023);
    u16b(&mut p, SHA256);
    u32b(&mut p, PARENT_ATTRS);
    blob(&mut p, &[])?;
    u16b(&mut p, 0x0006);
    u16b(&mut p, 128);
    u16b(&mut p, 0x0043); // AES128 CFB
    u16b(&mut p, ALG_NULL);
    u16b(&mut p, 0x0003);
    u16b(&mut p, ALG_NULL); // scheme, P256, KDF
    blob(&mut p, &[])?;
    blob(&mut p, &[])?;
    Ok(p)
}
pub fn sealed_template(policy: &[u8; 32]) -> Result<Vec<u8>> {
    let mut p = Vec::new();
    u16b(&mut p, 0x0008);
    u16b(&mut p, SHA256);
    u32b(&mut p, SEALED_ATTRS);
    blob(&mut p, policy)?;
    u16b(&mut p, ALG_NULL);
    blob(&mut p, &[])?;
    Ok(p)
}
fn validate_sealed(public: &[u8], policy: &[u8; 32]) -> Result<()> {
    let mut c = Cursor::new(public);
    require(
        c.be16()? == 0x0008 && c.be16()? == SHA256 && c.be32()? == SEALED_ATTRS,
        "sealed_object_attributes",
    )?;
    require(
        c.blob(32)? == policy && c.be16()? == ALG_NULL,
        "sealed_object_policy",
    )?;
    require(c.blob(64)?.len() == 32, "sealed_object_unique")?;
    c.finish()
}
fn skip_creation(c: &mut Cursor<'_>) -> Result<()> {
    c.blob(1024)?; // creationData
    require(c.blob(64)?.len() == 32, "creation_hash")?;
    require(c.be16()? == 0x8021, "creation_ticket")?;
    c.be32()?;
    c.blob(64)?;
    Ok(())
}

pub struct Bundle {
    pub token: [u8; 32],
    pub pcr: [u8; 32],
    pub policy: [u8; 32],
    pub secret_hash: [u8; 32],
    pub parent_name: Vec<u8>,
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}
impl Bundle {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut v = b"VRTPM001".to_vec();
        v.extend(self.token);
        v.extend(self.pcr);
        v.extend(self.policy);
        v.extend(self.secret_hash);
        blob(&mut v, &self.parent_name)?;
        blob(&mut v, &self.private)?;
        blob(&mut v, &self.public)?;
        require(v.len() <= MAX_BUNDLE, "bundle_too_large")?;
        Ok(v)
    }
    pub fn decode(bytes: &[u8], token: &[u8; 32]) -> Result<Self> {
        require(bytes.len() <= MAX_BUNDLE, "bundle_too_large")?;
        let mut c = Cursor::new(bytes);
        require(c.take(8)? == b"VRTPM001", "bundle_version")?;
        let t: [u8; 32] = c.take(32)?.try_into().unwrap();
        require(&t == token, "bundle_token_mismatch")?;
        let pcr = c.take(32)?.try_into().unwrap();
        let policy = c.take(32)?.try_into().unwrap();
        let secret_hash = c.take(32)?.try_into().unwrap();
        let parent_name = c.blob(34)?.to_vec();
        require(
            parent_name.len() == 34 && parent_name[..2] == SHA256.to_be_bytes(),
            "parent_name",
        )?;
        let private = c.blob(1024)?.to_vec();
        require(!private.is_empty(), "missing_private_blob")?;
        let public = c.blob(512)?.to_vec();
        validate_sealed(&public, &policy)?;
        c.finish()?;
        Ok(Self {
            token: t,
            pcr,
            policy,
            secret_hash,
            parent_name,
            private,
            public,
        })
    }
    pub fn blob_hash(&self) -> Result<[u8; 32]> {
        let mut v = Vec::new();
        blob(&mut v, &self.private)?;
        blob(&mut v, &self.public)?;
        Ok(hash(&v))
    }
}

impl<T: Transport> Tpm<T> {
    fn call(
        &mut self,
        cc: u32,
        hs: &[u32],
        auth: Option<u32>,
        p: &[u8],
        out_hs: usize,
        stage: &'static str,
    ) -> Result<Response> {
        let mut request = command(cc, hs, auth, p)?;
        let result = self.0.exchange(&request);
        request.fill(0);
        let raw = result?;
        response(&raw, out_hs, auth, stage)
    }
    pub fn family(&mut self) -> Result<()> {
        let mut p = Vec::new();
        u32b(&mut p, 6);
        u32b(&mut p, 0x100);
        u32b(&mut p, 1);
        let r = self.call(0x17a, &[], None, &p, 0, "get_family")?;
        let mut c = Cursor::new(&r.params);
        require(
            c.byte()? <= 1
                && c.be32()? == 6
                && c.be32()? == 1
                && c.be32()? == 0x100
                && c.be32()? == 0x322e_3000,
            "not_tpm2",
        )?;
        c.finish()
    }
    pub fn pcr7(&mut self) -> Result<[u8; 32]> {
        let r = self.call(0x17e, &[], None, &PCR7, 0, "pcr_read")?;
        let mut c = Cursor::new(&r.params);
        c.be32()?;
        require(
            c.take(PCR7.len())? == PCR7 && c.be32()? == 1,
            "pcr_selection_mismatch",
        )?;
        let digest = c.blob(32)?;
        require(digest.len() == 32, "pcr_digest_size")?;
        let out = digest.try_into().unwrap();
        c.finish()?;
        Ok(out)
    }
    pub fn random(&mut self) -> Result<[u8; 32]> {
        let mut bytes = Vec::new();
        for _ in 0..8 {
            if bytes.len() == 32 {
                break;
            }
            let r = self.call(
                0x17b,
                &[],
                None,
                &((32 - bytes.len()) as u16).to_be_bytes(),
                0,
                "get_random",
            )?;
            let mut c = Cursor::new(&r.params);
            let got = c.blob(32 - bytes.len())?;
            require(!got.is_empty(), "empty_random_response")?;
            bytes.extend(got);
            c.finish()?;
        }
        require(bytes.len() == 32, "short_random_response")?;
        Ok(bytes.try_into().unwrap())
    }
    pub fn primary(&mut self) -> Result<(u32, Vec<u8>)> {
        let mut p = Vec::new();
        blob(&mut p, &[0, 0, 0, 0])?;
        blob(&mut p, &parent_template()?)?;
        blob(&mut p, &[])?;
        u32b(&mut p, 0);
        let r = self.call(0x131, &[OWNER], Some(PASSWORD), &p, 1, "create_primary")?;
        let mut c = Cursor::new(&r.params);
        let public = c.blob(512)?;
        let template = parent_template()?;
        require(
            public.len() >= template.len()
                && public[..template.len() - 4] == template[..template.len() - 4],
            "primary_template_mismatch",
        )?;
        let mut unique = Cursor::new(&public[template.len() - 4..]);
        require(
            unique.blob(32)?.len() == 32 && unique.blob(32)?.len() == 32,
            "primary_point_size",
        )?;
        unique.finish()?;
        skip_creation(&mut c)?;
        let nm = c.blob(34)?.to_vec();
        c.finish()?;
        require(nm == name(public), "primary_name_mismatch")?;
        require(r.handles[0] >> 24 == 0x80, "primary_handle")?;
        Ok((r.handles[0], nm))
    }
    fn start_policy(&mut self, trial: bool) -> Result<u32> {
        let nonce = self.random()?;
        let mut p = Vec::new();
        blob(&mut p, &nonce)?;
        blob(&mut p, &[])?;
        p.push(if trial { 3 } else { 1 });
        u16b(&mut p, ALG_NULL);
        u16b(&mut p, SHA256);
        let r = self.call(0x176, &[NULL, NULL], None, &p, 1, "start_policy")?;
        let mut c = Cursor::new(&r.params);
        require(c.blob(64)?.len() == 32, "policy_nonce_size")?;
        c.finish()?;
        require(r.handles[0] >> 24 == 3, "policy_session_handle")?;
        Ok(r.handles[0])
    }
    fn policy_pcr(&mut self, session: u32, pcr: &[u8; 32]) -> Result<()> {
        let mut p = Vec::new();
        blob(&mut p, &hash(pcr))?;
        p.extend(PCR7);
        let r = self.call(0x17f, &[session], None, &p, 0, "policy_pcr")?;
        require(r.params.is_empty(), "policy_pcr_response")
    }
    fn policy_digest(&mut self, session: u32) -> Result<[u8; 32]> {
        let r = self.call(0x189, &[session], None, &[], 0, "policy_digest")?;
        let mut c = Cursor::new(&r.params);
        let d = c.blob(32)?;
        require(d.len() == 32, "policy_digest_size")?;
        let out = d.try_into().unwrap();
        c.finish()?;
        Ok(out)
    }
    pub fn trial_policy(&mut self, pcr: &[u8; 32]) -> Result<[u8; 32]> {
        let session = self.start_policy(true)?;
        self.policy_pcr(session, pcr)?;
        let digest = self.policy_digest(session)?;
        self.flush(session)?;
        Ok(digest)
    }
    pub fn create(
        &mut self,
        parent: u32,
        secret: &[u8; 32],
        policy: &[u8; 32],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut sensitive = Vec::new();
        blob(&mut sensitive, &[])?;
        blob(&mut sensitive, secret)?;
        let mut p = Vec::new();
        blob(&mut p, &sensitive)?;
        blob(&mut p, &sealed_template(policy)?)?;
        blob(&mut p, &[])?;
        u32b(&mut p, 0);
        let result = self.call(0x153, &[parent], Some(PASSWORD), &p, 0, "create_sealed");
        sensitive.fill(0);
        p.fill(0);
        let r = result?;
        let mut c = Cursor::new(&r.params);
        let private = c.blob(1024)?.to_vec();
        let public = c.blob(512)?.to_vec();
        require(!private.is_empty(), "empty_private_blob")?;
        validate_sealed(&public, policy)?;
        skip_creation(&mut c)?;
        c.finish()?;
        Ok((private, public))
    }
    pub fn load(&mut self, parent: u32, b: &Bundle) -> Result<u32> {
        validate_sealed(&b.public, &b.policy)?;
        let mut p = Vec::new();
        blob(&mut p, &b.private)?;
        blob(&mut p, &b.public)?;
        let r = self.call(0x157, &[parent], Some(PASSWORD), &p, 1, "load_sealed")?;
        let mut c = Cursor::new(&r.params);
        require(c.blob(34)? == name(&b.public), "loaded_name_mismatch")?;
        c.finish()?;
        require(r.handles[0] >> 24 == 0x80, "loaded_handle")?;
        Ok(r.handles[0])
    }
    fn unseal(&mut self, object: u32, auth: u32) -> Result<Vec<u8>> {
        let r = self.call(0x15e, &[object], Some(auth), &[], 0, "unseal")?;
        let mut c = Cursor::new(&r.params);
        let data = c.blob(32)?.to_vec();
        require(data.len() == 32, "unsealed_value_size")?;
        c.finish()?;
        Ok(data)
    }
    pub fn verify_policy_unseal(&mut self, object: u32, b: &Bundle) -> Result<()> {
        match self.unseal(object, PASSWORD) {
            Err(Failure {
                rc: Some(0x12f | 0x124),
                ..
            }) => {}
            Err(e) => return Err(e),
            Ok(mut value) => {
                value.fill(0);
                return Err(fail("password_authorization_was_accepted"));
            }
        }
        let empty = self.start_policy(false)?;
        match self.unseal(object, empty) {
            Err(Failure {
                rc: Some(0x99d), ..
            }) => {}
            Err(e) => return Err(e),
            Ok(mut value) => {
                value.fill(0);
                return Err(fail("empty_policy_was_accepted"));
            }
        }
        self.flush(empty)?;
        let policy = self.start_policy(false)?;
        self.policy_pcr(policy, &b.pcr)?;
        require(
            self.policy_digest(policy)? == b.policy,
            "real_policy_digest_mismatch",
        )?;
        let mut value = self.unseal(object, policy)?;
        let matches = hash(&value) == b.secret_hash;
        value.fill(0);
        require(matches, "unsealed_value_hash_mismatch")
    }
    pub fn nv_size(&mut self) -> Result<Option<usize>> {
        let r = match self.call(0x169, &[NV_INDEX], None, &[], 0, "nv_read_public") {
            Err(Failure {
                rc: Some(0x18b), ..
            }) => return Ok(None),
            other => other?,
        };
        let mut c = Cursor::new(&r.params);
        let public = c.blob(128)?;
        let mut p = Cursor::new(public);
        require(
            p.be32()? == NV_INDEX && p.be16()? == SHA256 && p.be32()? == (NV_ATTRS | 0x2000_0000),
            "nv_public_mismatch",
        )?;
        require(p.blob(64)?.is_empty(), "unexpected_nv_policy")?;
        let size = p.be16()? as usize;
        p.finish()?;
        require(c.blob(34)? == name(public), "nv_name_mismatch")?;
        c.finish()?;
        require((1..=MAX_BUNDLE).contains(&size), "nv_size_limit")?;
        Ok(Some(size))
    }
    pub fn nv_read(&mut self, size: usize) -> Result<Vec<u8>> {
        require(size <= MAX_BUNDLE, "nv_size_limit")?;
        let mut out = Vec::new();
        while out.len() < size {
            let count = (size - out.len()).min(512);
            let mut p = Vec::new();
            u16b(&mut p, count as u16);
            u16b(&mut p, out.len() as u16);
            let r = self.call(0x14e, &[OWNER, NV_INDEX], Some(PASSWORD), &p, 0, "nv_read")?;
            let mut c = Cursor::new(&r.params);
            let data = c.blob(count)?;
            require(data.len() == count, "short_nv_read")?;
            out.extend(data);
            c.finish()?;
        }
        Ok(out)
    }
    pub fn nv_store(&mut self, bytes: &[u8]) -> Result<()> {
        require(
            !bytes.is_empty() && bytes.len() <= MAX_BUNDLE,
            "nv_size_limit",
        )?;
        let mut public = Vec::new();
        u32b(&mut public, NV_INDEX);
        u16b(&mut public, SHA256);
        u32b(&mut public, NV_ATTRS);
        blob(&mut public, &[])?;
        u16b(&mut public, bytes.len() as u16);
        let mut p = Vec::new();
        blob(&mut p, &[])?;
        blob(&mut p, &public)?;
        let r = self.call(0x12a, &[OWNER], Some(PASSWORD), &p, 0, "nv_define")?;
        require(r.params.is_empty(), "nv_define_response")?;
        for (i, chunk) in bytes.chunks(512).enumerate() {
            let mut p = Vec::new();
            blob(&mut p, chunk)?;
            u16b(&mut p, (i * 512) as u16);
            let r = self.call(0x137, &[OWNER, NV_INDEX], Some(PASSWORD), &p, 0, "nv_write")?;
            require(r.params.is_empty(), "nv_write_response")?;
        }
        require(
            self.nv_size()? == Some(bytes.len()) && self.nv_read(bytes.len())? == bytes,
            "nv_readback_mismatch",
        )
    }
    pub fn flush(&mut self, h: u32) -> Result<()> {
        let r = self.call(0x165, &[], None, &h.to_be_bytes(), 0, "flush_context")?;
        require(r.params.is_empty(), "flush_response")
    }
    pub fn shutdown(&mut self) -> Result<()> {
        let r = self.call(0x145, &[], None, &0u16.to_be_bytes(), 0, "shutdown_clear")?;
        require(r.params.is_empty(), "shutdown_response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Script(VecDeque<(u32, Vec<u8>)>);
    impl Transport for Script {
        fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>> {
            let (cc, response) = self.0.pop_front().expect("unexpected TPM command");
            assert_eq!(u32::from_be_bytes(request[6..10].try_into().unwrap()), cc);
            Ok(response)
        }
    }
    fn reply(params: &[u8], auth: Option<u32>) -> Vec<u8> {
        let mut out = Vec::new();
        u16b(&mut out, if auth.is_some() { 0x8002 } else { 0x8001 });
        u32b(&mut out, 0);
        u32b(&mut out, 0);
        if auth.is_some() {
            u32b(&mut out, params.len() as u32);
        }
        out.extend(params);
        if let Some(handle) = auth {
            blob(&mut out, if handle == PASSWORD { &[] } else { &[1; 32] }).unwrap();
            out.push(if handle == PASSWORD { 1 } else { 0 });
            blob(&mut out, &[]).unwrap();
        }
        let len = out.len() as u32;
        out[2..6].copy_from_slice(&len.to_be_bytes());
        out
    }
    fn error(rc: u32) -> Vec<u8> {
        let mut out = vec![0x80, 1, 0, 0, 0, 10];
        u32b(&mut out, rc);
        out
    }
    fn bundle() -> Bundle {
        let policy = [2; 32];
        let mut public = sealed_template(&policy).unwrap();
        public.truncate(public.len() - 2);
        blob(&mut public, &[3; 32]).unwrap();
        Bundle {
            token: *b"0123456789abcdef0123456789abcdef",
            pcr: [1; 32],
            policy,
            secret_hash: [4; 32],
            parent_name: name(b"parent"),
            private: vec![5; 128],
            public,
        }
    }
    #[test]
    fn sealed_public_requires_policy_and_disallows_password_attribute() {
        assert_eq!(SEALED_ATTRS & 0x40, 0);
        assert_eq!(SEALED_ATTRS & 0x20, 0);
        let b = bundle();
        assert!(validate_sealed(&b.public, &b.policy).is_ok());
        let mut invalid = b.public.clone();
        invalid[7] |= 0x40;
        assert!(validate_sealed(&invalid, &b.policy).is_err());
        assert!(validate_sealed(&b.public, &[0; 32]).is_err());
    }
    #[test]
    fn bundle_is_complete_token_bound_and_contains_only_wrapped_state() {
        let b = bundle();
        let encoded = b.encode().unwrap();
        let r = Bundle::decode(&encoded, &b.token).unwrap();
        assert_eq!(r.private, b.private);
        assert_eq!(r.blob_hash().unwrap(), b.blob_hash().unwrap());
        for size in 0..encoded.len() {
            assert!(Bundle::decode(&encoded[..size], &b.token).is_err());
        }
        assert!(Bundle::decode(&encoded, &[b'a'; 32]).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        assert!(Bundle::decode(&trailing, &b.token).is_err());
    }
    #[test]
    fn exact_pcr_read_wire_and_response_selection() {
        assert_eq!(
            crate::hex(&command(0x17e, &[], None, &PCR7).unwrap()),
            "8001000000140000017e00000001000b03800000"
        );
        let mut p = 7u32.to_be_bytes().to_vec();
        p.extend(PCR7);
        u32b(&mut p, 1);
        blob(&mut p, &[6; 32]).unwrap();
        let mut t = Tpm(Script([(0x17e, reply(&p, None))].into()));
        assert_eq!(t.pcr7().unwrap(), [6; 32]);
        p[11] = 1;
        let mut t = Tpm(Script([(0x17e, reply(&p, None))].into()));
        assert!(t.pcr7().is_err());
    }
    #[test]
    fn response_framing_auth_and_errors_are_strict() {
        let good = reply(b"data", Some(PASSWORD));
        assert_eq!(
            response(&good, 0, Some(PASSWORD), "test").unwrap().params,
            b"data"
        );
        for size in 0..good.len() {
            assert!(response(&good[..size], 0, Some(PASSWORD), "test").is_err());
        }
        let mut bad = good.clone();
        let n = bad.len();
        bad[n - 3] = 0;
        assert!(response(&bad, 0, Some(PASSWORD), "test").is_err());
        assert!(response(&reply(&[], None), 0, Some(PASSWORD), "test").is_err());
        assert_eq!(
            response(&error(0x12f), 0, Some(PASSWORD), "unseal")
                .err()
                .unwrap(),
            Failure {
                stage: "unseal",
                rc: Some(0x12f)
            }
        );
    }
    #[test]
    fn only_missing_handle_selects_first_boot_phase() {
        let mut t = Tpm(Script([(0x169, error(0x18b))].into()));
        assert_eq!(t.nv_size().unwrap(), None);
        for rc in [0x101, 0x99d, 0x18a] {
            let mut t = Tpm(Script([(0x169, error(rc))].into()));
            assert!(t.nv_size().is_err());
        }
    }
    #[test]
    fn empty_password_success_or_unrelated_error_never_counts_as_denial() {
        let b = bundle();
        let mut p = Vec::new();
        blob(&mut p, &[0; 32]).unwrap();
        let mut t = Tpm(Script([(0x15e, reply(&p, Some(PASSWORD)))].into()));
        assert_eq!(
            t.verify_policy_unseal(0x8000_0001, &b).unwrap_err().stage,
            "password_authorization_was_accepted"
        );
        let mut t = Tpm(Script([(0x15e, error(0x101))].into()));
        assert_eq!(
            t.verify_policy_unseal(0x8000_0001, &b).unwrap_err().rc,
            Some(0x101)
        );
    }

    fn sized(bytes: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        blob(&mut v, bytes).unwrap();
        v
    }
    fn handle_reply(handle: u32, params: &[u8]) -> Vec<u8> {
        let mut v = handle.to_be_bytes().to_vec();
        v.extend(params);
        reply(&v, None)
    }
    struct Recorded {
        script: Script,
        requests: Vec<Vec<u8>>,
    }
    impl Transport for Recorded {
        fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>> {
            self.requests.push(request.to_vec());
            self.script.exchange(request)
        }
    }
    #[test]
    fn trial_policy_uses_trial_session_and_explicit_sha256_pcr7_digest() {
        let policy = [7; 32];
        let pcr = [8; 32];
        let script = Script(
            [
                (0x17b, reply(&sized(&[6; 32]), None)),
                (0x176, handle_reply(0x0300_0001, &sized(&[1; 32]))),
                (0x17f, reply(&[], None)),
                (0x189, reply(&sized(&policy), None)),
                (0x165, reply(&[], None)),
            ]
            .into(),
        );
        let mut t = Tpm(Recorded {
            script,
            requests: Vec::new(),
        });
        assert_eq!(t.trial_policy(&pcr).unwrap(), policy);
        assert_eq!(t.0.requests[1][54], 3);
        let mut expected = 0x0300_0001u32.to_be_bytes().to_vec();
        blob(&mut expected, &hash(&pcr)).unwrap();
        expected.extend(PCR7);
        assert_eq!(&t.0.requests[2][10..], expected);
        assert!(t.0.script.0.is_empty());
    }
    #[test]
    fn unseal_requires_both_authorization_negatives_then_real_matching_policy() {
        let mut b = bundle();
        b.secret_hash = hash(&[9; 32]);
        let script = Script(
            [
                (0x15e, error(0x12f)),
                (0x17b, reply(&sized(&[6; 32]), None)),
                (0x176, handle_reply(0x0300_0001, &sized(&[1; 32]))),
                (0x15e, error(0x99d)),
                (0x165, reply(&[], None)),
                (0x17b, reply(&sized(&[6; 32]), None)),
                (0x176, handle_reply(0x0300_0002, &sized(&[1; 32]))),
                (0x17f, reply(&[], None)),
                (0x189, reply(&sized(&b.policy), None)),
                (0x15e, reply(&sized(&[9; 32]), Some(0x0300_0002))),
            ]
            .into(),
        );
        let mut t = Tpm(Recorded {
            script,
            requests: Vec::new(),
        });
        t.verify_policy_unseal(0x8000_0001, &b).unwrap();
        assert_eq!(t.0.requests[2][54], 1);
        assert_eq!(t.0.requests[6][54], 1);
        assert_eq!(&t.0.requests[9][18..22], &0x0300_0002u32.to_be_bytes());
        assert!(t.0.script.0.is_empty());
    }
    #[test]
    fn accepting_empty_policy_is_never_a_pass() {
        let b = bundle();
        let script = Script(
            [
                (0x15e, error(0x12f)),
                (0x17b, reply(&sized(&[6; 32]), None)),
                (0x176, handle_reply(0x0300_0001, &sized(&[1; 32]))),
                (0x15e, reply(&sized(&[9; 32]), Some(0x0300_0001))),
            ]
            .into(),
        );
        let mut t = Tpm(script);
        assert_eq!(
            t.verify_policy_unseal(0x8000_0001, &b).unwrap_err().stage,
            "empty_policy_was_accepted"
        );
    }
}
