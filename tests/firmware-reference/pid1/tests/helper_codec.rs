//! Explicitly supervised helper-only integration; never run by default tests.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use vroom_tpm_reference_init::tpm::{Bundle, Tpm, Transport, MAX_PACKET};
use vroom_tpm_reference_init::{fail, hash, require, Result};

struct Socket(UnixStream);
impl Transport for Socket {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        self.0
            .write_all(request)
            .map_err(|_| fail("test_socket_write"))?;
        let mut hdr = [0; 10];
        self.0
            .read_exact(&mut hdr)
            .map_err(|_| fail("test_socket_header"))?;
        let length = u32::from_be_bytes(hdr[2..6].try_into().unwrap()) as usize;
        require((10..=MAX_PACKET).contains(&length), "test_socket_length")?;
        let mut out = hdr.to_vec();
        out.resize(length, 0);
        self.0
            .read_exact(&mut out[10..])
            .map_err(|_| fail("test_socket_body"))?;
        Ok(out)
    }
}

#[test]
#[ignore = "requires an explicitly owned fresh swtpm and external 30-second supervisor"]
fn fresh_helper_real_create_load_policy_unseal_and_opaque_nv_roundtrip() {
    let socket =
        std::env::var_os("VROOM_SWTPM_TEST_SOCKET").expect("supervisor must provide owned socket");
    let socket = UnixStream::connect(socket).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut tpm = Tpm(Socket(socket));
    tpm.family().unwrap();
    assert!(tpm.nv_size().unwrap().is_none());
    let pcr = tpm.pcr7().unwrap();
    let (parent, parent_name) = tpm.primary().unwrap();
    let policy = tpm.trial_policy(&pcr).unwrap();
    let mut secret = tpm.random().unwrap();
    let secret_hash = hash(&secret);
    let created = tpm.create(parent, &secret, &policy);
    secret.fill(0);
    let (private, public) = created.unwrap();
    let bundle = Bundle {
        token: *b"0123456789abcdef0123456789abcdef",
        pcr,
        policy,
        secret_hash,
        parent_name,
        private,
        public,
    };
    let object = tpm.load(parent, &bundle).unwrap();
    tpm.verify_policy_unseal(object, &bundle).unwrap();
    tpm.flush(object).unwrap();
    let bytes = bundle.encode().unwrap();
    tpm.nv_store(&bytes).unwrap();
    let size = tpm.nv_size().unwrap().unwrap();
    let restored = Bundle::decode(&tpm.nv_read(size).unwrap(), &bundle.token).unwrap();
    assert!(restored.blob_hash().unwrap() == bundle.blob_hash().unwrap());
    let object = tpm.load(parent, &restored).unwrap();
    tpm.verify_policy_unseal(object, &restored).unwrap();
    tpm.flush(object).unwrap();
    tpm.flush(parent).unwrap();
    tpm.shutdown().unwrap();
}
