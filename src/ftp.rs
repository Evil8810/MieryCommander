//! FTP / FTPS connections.

use crate::fsutil::Entry;
use crate::remote::{self, ConnectError, Pump, Remote, Security, Site};
use std::fs::File;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpResult, RustlsConnector, RustlsFtpStream};

const TIMEOUT: Duration = Duration::from_secs(20);

// ----------------------------------------------------------------------------
// TLS
// ----------------------------------------------------------------------------

/// Accepts any certificate. Only used when the user explicitly allows
/// self-signed / invalid certificates for a site.
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_connector(accept_invalid: bool) -> Result<RustlsConnector, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = if accept_invalid {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth()
    } else {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(RustlsConnector::from(Arc::new(config)))
}

// ----------------------------------------------------------------------------
// Connection
// ----------------------------------------------------------------------------

pub struct FtpConn {
    id: usize,
    site: Site,
    password: String,
    home: String,
    stream: Mutex<Option<RustlsFtpStream>>,
}

fn open_stream(site: &Site, password: &str) -> Result<RustlsFtpStream, String> {
    let host = site.host.trim();
    if host.is_empty() {
        return Err("Kein Server angegeben".into());
    }
    let addr = (host, site.port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}: Adresse nicht gefunden"))?;
    let mut stream = match site.security {
        Security::None => RustlsFtpStream::connect_timeout(addr, TIMEOUT).map_err(|e| describe(&e))?,
        Security::Explicit => RustlsFtpStream::connect_timeout(addr, TIMEOUT)
            .map_err(|e| describe(&e))?
            .into_secure(tls_connector(site.accept_invalid_certs)?, host)
            .map_err(|e| describe(&e))?,
        #[allow(deprecated)]
        Security::Implicit => {
            RustlsFtpStream::connect_secure_implicit(addr, tls_connector(site.accept_invalid_certs)?, host)
                .map_err(|e| describe(&e))?
        }
    };
    let _ = stream.get_ref().set_read_timeout(Some(TIMEOUT));
    let _ = stream.get_ref().set_write_timeout(Some(TIMEOUT));
    let user = if site.user.trim().is_empty() { "anonymous" } else { site.user.trim() };
    stream
        .login(user, password)
        .map_err(|e| format!("Anmeldung fehlgeschlagen: {}", describe(&e)))?;
    stream.transfer_type(FileType::Binary).map_err(|e| describe(&e))?;
    // Ask for UTF-8 file names; old servers may refuse, that's fine.
    let _ = stream.opts("UTF8", Some("ON"));
    stream.set_passive_nat_workaround(true);
    if !site.passive {
        stream = stream.active_mode(TIMEOUT);
    }
    Ok(stream)
}

pub fn describe(e: &FtpError) -> String {
    match e {
        FtpError::UnexpectedResponse(r) => {
            let body = String::from_utf8_lossy(&r.body).trim().to_string();
            if body.is_empty() { format!("Server: {:?}", r.status) } else { body }
        }
        other => other.to_string(),
    }
}

fn is_connection_error(e: &FtpError) -> bool {
    match e {
        FtpError::ConnectionError(io) => io.kind() != std::io::ErrorKind::Interrupted,
        FtpError::UnexpectedResponse(r) => r.status.code() == 421,
        _ => false,
    }
}

impl FtpConn {
    pub fn connect(site: Site, password: String) -> Result<FtpConn, ConnectError> {
        let mut stream = open_stream(&site, &password)?;
        let mut home = stream.pwd().unwrap_or_else(|_| "/".into());
        let dir = site.remote_dir.trim().to_string();
        if !dir.is_empty() {
            stream.cwd(&dir).map_err(|e| format!("Startordner {dir}: {}", describe(&e)))?;
            home = stream.pwd().unwrap_or(dir);
        }
        Ok(FtpConn {
            id: remote::next_id(),
            site,
            password,
            home,
            stream: Mutex::new(Some(stream)),
        })
    }

    /// Run `f` on the (re)connected control stream. A dropped connection is
    /// re-established once and the call retried.
    fn with<R>(&self, mut f: impl FnMut(&mut RustlsFtpStream) -> FtpResult<R>) -> Result<R, String> {
        let mut guard = self.stream.lock().unwrap();
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(open_stream(&self.site, &self.password)?);
            }
            let s = guard.as_mut().unwrap();
            match f(s) {
                Ok(r) => return Ok(r),
                Err(e) if attempt == 0 && is_connection_error(&e) => *guard = None,
                Err(e) => return Err(describe(&e)),
            }
        }
        Err("Verbindung verloren".into())
    }
}

impl Remote for FtpConn {
    fn id(&self) -> usize {
        self.id
    }

    fn site(&self) -> &Site {
        &self.site
    }

    fn home(&self) -> &str {
        &self.home
    }

    fn url(&self) -> String {
        let scheme = if self.site.security == Security::None { "ftp" } else { "ftps" };
        format!("{scheme}://{}@{}", self.site.user, self.site.host)
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>, String> {
        let (mlsd, lines) = self.with(|s| match s.mlsd(Some(dir)) {
            Ok(l) => Ok((true, l)),
            Err(FtpError::UnexpectedResponse(r)) if r.status.code() >= 500 => s.list(Some(dir)).map(|l| (false, l)),
            Err(e) => Err(e),
        })?;
        let mut out = Vec::new();
        for line in lines {
            let parsed = if mlsd {
                suppaftp::list::ListParser::parse_mlsd(&line)
            } else {
                suppaftp::list::ListParser::parse_posix(&line).or_else(|_| suppaftp::list::ListParser::parse_dos(&line))
            };
            let Ok(f) = parsed else { continue };
            let name = f.name().to_string();
            if name == "." || name == ".." || name.is_empty() {
                continue;
            }
            out.push(Entry {
                path: PathBuf::from(remote::join(dir, &name)),
                mode: posix_mode(&f),
                name,
                is_dir: f.is_directory(),
                is_link: f.is_symlink(),
                is_parent: false,
                size: if f.is_directory() { 0 } else { f.size() as u64 },
                modified: Some(f.modified()).filter(|t| *t > SystemTime::UNIX_EPOCH),
            });
        }
        Ok(out)
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.with(|s| s.mkdir(path))
    }

    fn is_dir(&self, path: &str) -> bool {
        self.with(|s| {
            let back = s.pwd()?;
            s.cwd(path)?;
            s.cwd(back)
        })
        .is_ok()
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.with(|s| s.rename(from, to))
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.with(|s| s.rm(path))
    }

    fn remove_dir(&self, path: &str) -> Result<(), String> {
        self.with(|s| s.rmdir(path))
    }

    fn download(&self, remote: &str, local: &Path, pump: &Pump) -> Result<(), String> {
        self.with(|s| {
            let mut out = File::create(local).map_err(FtpError::ConnectionError)?;
            let mut ts = s.retr_as_stream(remote)?;
            pump(&mut ts, &mut out).map_err(FtpError::ConnectionError)?;
            ts.finish()
        })
    }

    fn upload(&self, local: &Path, remote: &str, pump: &Pump) -> Result<(), String> {
        self.with(|s| {
            let mut input = File::open(local).map_err(FtpError::ConnectionError)?;
            let mut ts = s.put_with_stream(remote)?;
            pump(&mut input, &mut ts).map_err(FtpError::ConnectionError)?;
            ts.finish()
        })
    }

    fn quit(&self) {
        if let Ok(mut guard) = self.stream.try_lock()
            && let Some(mut s) = guard.take()
        {
            let _ = s.quit();
        }
    }
}

fn posix_mode(f: &suppaftp::list::File) -> u32 {
    use suppaftp::list::PosixPexQuery::*;
    let mut m = 0;
    for (shift, who) in [(6, Owner), (3, Group), (0, Others)] {
        if f.can_read(who) {
            m |= 4 << shift;
        }
        if f.can_write(who) {
            m |= 2 << shift;
        }
        if f.can_execute(who) {
            m |= 1 << shift;
        }
    }
    m
}
