use super::*;
use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::Engine;
use libadb::pairing::{Role, Spake2};
use rsa::{pkcs8::DecodePrivateKey, traits::PublicKeyParts};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "adb-pairing-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        )))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn send(stream: &mut impl Write, kind: u8, payload: &[u8]) {
    stream.write_all(&[1, kind]).unwrap();
    stream
        .write_all(&u32::try_from(payload.len()).unwrap().to_be_bytes())
        .unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();
}
fn receive(stream: &mut impl Read, kind: u8) -> Vec<u8> {
    let mut header = [0; 6];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[..2], &[1, kind]);
    let length = u32::from_be_bytes(header[2..].try_into().unwrap()) as usize;
    assert!(length <= 8192 + 16);
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).unwrap();
    payload
}

fn device(code: &'static str) -> (SocketAddr, thread::JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
    let peer = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(config)).unwrap(),
            socket,
        );
        let theirs = receive(&mut stream, 0);
        assert_eq!(theirs.len(), 32);
        assert_eq!(
            stream.conn.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        let mut exported = [0; 64];
        stream
            .conn
            .export_keying_material(&mut exported, b"adb-label\0", None)
            .unwrap();
        let mut password = code.as_bytes().to_vec();
        password.extend_from_slice(&exported);
        let spake = Spake2::new(
            Role::Bob,
            b"adb pair server\0",
            b"adb pair client\0",
            &password,
            &mut OsRng,
        );
        send(&mut stream, 0, spake.message());
        let shared = spake.finish(&theirs).unwrap();
        let mut key = [0; 16];
        hkdf::Hkdf::<sha2::Sha256>::new(None, shared.as_ref())
            .expand(b"adb pairing_auth aes-128-gcm key", &mut key)
            .unwrap();
        let cipher = Aes128Gcm::new((&key).into());
        let mut info = vec![0; 8192];
        info[0] = 1;
        info[1..=b"adb-test-guid".len()].copy_from_slice(b"adb-test-guid");
        let nonce = Nonce::from_slice(&[0; 12]);
        send(
            &mut stream,
            1,
            &cipher.encrypt(nonce, info.as_slice()).unwrap(),
        );
        let sealed = receive(&mut stream, 1);
        let Ok(info) = cipher.decrypt(nonce, sealed.as_slice()) else {
            return None;
        };
        assert_eq!(info.len(), 8192);
        assert_eq!(info[0], 0);
        let end = info[1..].iter().position(|&byte| byte == 0).unwrap();
        Some(String::from_utf8(info[1..=end].to_vec()).unwrap())
    });
    (address, peer)
}

#[test]
fn direct_pairing_persists_the_identity_learned_by_a_device() {
    let directory = Directory::new();
    let (address, peer) = device("123456");
    let path = directory.0.clone();
    let paired = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            tokio::spawn(async move {
                pair_with_key_directory(address, "123456", path, Duration::from_secs(5)).await
            })
            .await
            .unwrap()
        })
        .unwrap();
    assert_eq!(paired.guid, "adb-test-guid");
    assert_eq!(paired.private_key_path, directory.0.join("adbkey"));
    let learned = peer.join().unwrap().unwrap();
    let private =
        rsa::RsaPrivateKey::from_pkcs8_pem(&fs::read_to_string(&paired.private_key_path).unwrap())
            .unwrap();
    let wire = base64::engine::general_purpose::STANDARD
        .decode(learned.split_ascii_whitespace().next().unwrap())
        .unwrap();
    assert_eq!(wire.len(), 524);
    let mut modulus = private.n().to_bytes_le();
    modulus.resize(256, 0);
    assert_eq!(&wire[8..264], modulus);
    assert_eq!(&wire[520..524], &65537_u32.to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&paired.private_key_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
}

#[test]
fn wrong_pairing_code_never_authenticates_the_hosts_key() {
    let directory = Directory::new();
    let (address, peer) = device("123456");
    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(pair_with_key_directory(
            address,
            "000000",
            &directory.0,
            Duration::from_secs(5),
        ))
        .unwrap_err();
    assert!(error.to_string().contains("code did not match"), "{error}");
    assert_eq!(peer.join().unwrap(), None);
}
