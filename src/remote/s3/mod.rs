//! S3, as a panel.
//!
//! A [`RemoteTransport`] like every other backend here, so the quick-connect
//! line, `hosts.toml`, the keyring, the job engine and the archive machinery
//! all work against it unchanged.
//!
//! # S3 has no directories
//!
//! A key is a string, and `photos/2024/a.jpg` contains slashes the way any
//! other string might. What makes a listing look like a tree is
//! `ListObjectsV2` with `delimiter=/`, which answers with the keys directly
//! under a prefix and a separate list of the prefixes below it. Those prefixes
//! are the directories a panel draws, and they exist only as an artefact of
//! asking that way.
//!
//! Two consequences worth knowing rather than discovering:
//!
//! * **An empty directory usually is not there.** Most tools make one by
//!   putting a zero-byte object at `prefix/`, and this does the same, but a
//!   prefix with nothing under it and no marker object cannot be listed
//!   because there is nothing to list.
//! * **Renaming copies.** S3 has no rename: it is `PUT` with
//!   `x-amz-copy-source` and then `DELETE`, server side, which is what every
//!   other client does and is why renaming a large object is not instant.
//!
//! # Paging
//!
//! A listing arrives a page at a time with a continuation token, and a bucket
//! is allowed to hold more objects than anybody wants to hold in memory. The
//! walk stops at [`MAX_KEYS`], which is the same bound every other backend
//! here puts on a listing.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};
use crate::remote::transport::RemoteTransport;
use crate::remote::{Protocol, Target};
use crate::vfs::{Capabilities, Entry, LatencyClass, ReadSeek};

mod list;
pub mod sign;

#[cfg(test)]
#[path = "s3_tests.rs"]
mod tests;

/// The most keys one listing will hold.
pub const MAX_KEYS: usize = 100_000;

/// The most bytes a listing reply may be.
const MAX_REPLY: u64 = 64 * 1024 * 1024;

/// The most a single upload may carry.
///
/// A `PUT` of one object needs its length up front, and the copy engine hands
/// bytes over as it reads them, so the body is gathered first. Multipart
/// upload is the answer for anything larger and is a bigger feature than this
/// one: it needs an upload id, per-part retries and an abort path for when the
/// program is killed half way, or the parts are billed for ever.
pub const MAX_UPLOAD: usize = 512 * 1024 * 1024;

/// One S3 connection.
#[derive(Debug)]
pub struct S3Fs {
    /// `https://s3.eu-west-1.amazonaws.com`, no trailing slash.
    origin: String,
    /// The `Host` header, which is what gets signed.
    host: String,
    /// The access key id.
    access_key: String,
    /// The secret. Never logged, never written to `hosts.toml`.
    secret: Vec<u8>,
    /// Which region to sign for.
    region: String,
    /// `AWS_SESSION_TOKEN`, for temporary credentials. Signed as a header, so
    /// it has to be present at signing time rather than added afterwards.
    session_token: Option<String>,
    /// Where the panel opens: `/bucket` or `/bucket/prefix`.
    root: String,
    /// Cleared by [`RemoteTransport::close`].
    live: AtomicBool,
}

impl S3Fs {
    /// Open a connection and prove it works.
    ///
    /// Lists the starting directory, because a connection that cannot do that
    /// is not connected and finding out here means the panel never opens onto
    /// an error.
    pub fn connect(
        target: &Target,
        access_key: &str,
        secret: Option<&[u8]>,
        from_env: bool,
    ) -> Result<Self> {
        let port = target.port;
        let host = if port == Protocol::S3.default_port() {
            target.host.clone()
        } else {
            format!("{}:{port}", target.host)
        };
        // What was typed wins; the environment fills in what was not. That
        // order matters: somebody who typed a key meant that key, and an
        // AWS_ACCESS_KEY_ID left over in the shell silently overriding it
        // would be a connection to the wrong account with no way to see why.
        let env = if from_env {
            Credentials::from_env()
        } else {
            Credentials::default()
        };
        let access_key = if access_key.is_empty() {
            env.access_key.clone().unwrap_or_default()
        } else {
            access_key.to_string()
        };
        let secret = match secret {
            Some(bytes) if !bytes.is_empty() => bytes.to_vec(),
            _ => env.secret.clone().unwrap_or_default().into_bytes(),
        };
        // A region from the environment beats one guessed from the hostname,
        // because the guess is only ever a guess and `AWS_REGION` is a
        // statement.
        let region = env
            .region
            .clone()
            .unwrap_or_else(|| region_of(&target.host));
        let fs = Self {
            origin: format!("https://{host}"),
            region,
            host,
            access_key,
            secret,
            session_token: env.session_token.clone(),
            root: normalise(target.dir.as_deref().unwrap_or("/")),
            live: AtomicBool::new(true),
        };
        fs.list(&fs.root.clone())?;
        Ok(fs)
    }

    /// Where the panel opens.
    #[must_use]
    pub fn start_dir(&self) -> &str {
        &self.root
    }

    /// Split a path into its bucket and the key below it.
    ///
    /// `/bucket/a/b.txt` is the bucket `bucket` and the key `a/b.txt`; the
    /// root is no bucket at all, which is the one listing that names buckets
    /// rather than objects.
    fn split(path: &str) -> (Option<String>, String) {
        let trimmed = path.trim_start_matches('/');
        match trimmed.split_once('/') {
            Some((bucket, key)) if !bucket.is_empty() => {
                (Some(bucket.to_string()), key.to_string())
            }
            _ if trimmed.is_empty() => (None, String::new()),
            _ => (Some(trimmed.to_string()), String::new()),
        }
    }

    /// Send one signed request.
    fn send(
        &self,
        method: &str,
        bucket: Option<&str>,
        key: &str,
        query: &[(&str, String)],
        body: &[u8],
        extra: &[(String, String)],
    ) -> Result<ureq::http::Response<ureq::Body>> {
        // Path style rather than virtual-hosted: it works on every endpoint
        // including the ones that are not AWS, and a bucket name that is not a
        // legal DNS label has nowhere to go in a hostname.
        let uri = match bucket {
            Some(bucket) => format!(
                "/{}/{}",
                sign::encode_segment(bucket),
                sign::encode_key(key)
            ),
            None => "/".to_string(),
        };
        let uri = uri.trim_end_matches('/').to_string();
        let uri = if uri.is_empty() { "/".to_string() } else { uri };
        let canonical_query = sign::canonical_query(query);
        let payload_hash = if body.is_empty() {
            sign::EMPTY_SHA256.to_string()
        } else {
            sign::sha256_hex(body)
        };
        let timestamp = timestamp();
        // The session token is **signed**, not merely sent: a temporary
        // credential's token is part of what the endpoint verifies, and
        // adding it to the request afterwards produces a 403 that says
        // nothing about which header was missing.
        let mut extra = extra.to_vec();
        if let Some(token) = self.session_token.as_ref() {
            extra.push(("x-amz-security-token".to_string(), token.clone()));
        }
        let extra = extra.as_slice();
        let signed = sign::sign(
            &sign::Request {
                method,
                uri: &uri,
                query: &canonical_query,
                host: &self.host,
                payload_hash: &payload_hash,
                timestamp: &timestamp,
                extra,
            },
            &self.access_key,
            &self.secret,
            &self.region,
        );

        let url = if canonical_query.is_empty() {
            format!("{}{uri}", self.origin)
        } else {
            format!("{}{uri}?{canonical_query}", self.origin)
        };
        let mut builder = ureq::http::Request::builder()
            .method(method)
            .uri(&url)
            .header("Authorization", signed.authorization)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &timestamp);
        for (name, value) in extra {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder
            .body(body.to_vec())
            .map_err(|err| Error::msg(format!("{method} {uri}: {err}")))?;
        agent()
            .run(request)
            .map_err(|err| translate(method, &uri, &err))
    }
}

/// The HTTP agent, configured as the update check's is.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// `20130524T000000Z`, now.
fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// The region an endpoint's hostname implies.
///
/// `s3.eu-west-1.amazonaws.com` is `eu-west-1`. Anything that is not an AWS
/// hostname gets `us-east-1`, which is what MinIO, Ceph and every other
/// S3-compatible endpoint accept and ignore: they verify the signature against
/// whatever region it claims, so the value only has to match itself.
#[must_use]
pub fn region_of(host: &str) -> String {
    if !host.ends_with(".amazonaws.com") {
        return "us-east-1".to_string();
    }
    for part in host.split('.') {
        // `eu-west-1`, `us-gov-east-1`: at least two dashes and a trailing
        // digit is what tells a region from `s3` or `amazonaws`.
        if part.matches('-').count() >= 2 && part.ends_with(|c: char| c.is_ascii_digit()) {
            return part.to_string();
        }
    }
    "us-east-1".to_string()
}

/// A directory path with exactly one trailing slash, or `/`.
fn normalise(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    format!("{trimmed}/")
}

/// An HTTP failure, as a sentence.
fn translate(method: &str, path: &str, err: &ureq::Error) -> Error {
    let text = err.to_string();
    if text.contains("403") {
        return Error::msg(format!(
            "{path}: refused ({method}); check the access key, the secret and the region"
        ));
    }
    if text.contains("404") {
        return Error::msg(format!("{path}: no such bucket or key"));
    }
    Error::msg(format!("{path}: {method} failed: {text}"))
}

impl S3Fs {
    /// The body of a reply, bounded.
    fn body_of(response: ureq::http::Response<ureq::Body>) -> Result<String> {
        let mut text = String::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_REPLY)
            .read_to_string(&mut text)
            .map_err(Error::Bare)?;
        Ok(text)
    }
}

impl RemoteTransport for S3Fs {
    fn protocol(&self) -> Protocol {
        Protocol::S3
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            writable: true,
            // A `GET` is a stream. Ranged reads exist and the viewer could use
            // them, but every seek would be a request and a round trip, which
            // is worse than the forward-only mode it already has for this.
            seekable: false,
            random_access: false,
            has_directories: true,
            // Renaming is a copy and a delete, server side but not atomic:
            // there is a moment when both exist, and if the delete fails there
            // is a moment when both stay.
            atomic_rename: false,
            // Said honestly, so the panel knows a listing arrives in pages and
            // a large bucket is not a stall.
            paged_listing: true,
            can_execute: false,
            links: false,
            settable_mode: false,
            latency: LatencyClass::Network,
        }
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = normalise(dir);
        let (bucket, prefix) = Self::split(&dir);
        // The root of a connection names buckets. It is the one listing that
        // is not about objects at all.
        let Some(bucket) = bucket else {
            let response = self.send("GET", None, "", &[], &[], &[])?;
            return Ok(list::buckets(&Self::body_of(response)?, MAX_KEYS));
        };

        let mut out: Vec<Entry> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = vec![
                ("list-type", "2".to_string()),
                ("delimiter", "/".to_string()),
                ("prefix", prefix.clone()),
            ];
            if let Some(next) = token.as_ref() {
                query.push(("continuation-token", next.clone()));
            }
            let response = self.send("GET", Some(&bucket), "", &query, &[], &[])?;
            let page = list::objects(&Self::body_of(response)?, &prefix, MAX_KEYS);
            out.extend(page.entries);
            // Bounded whatever the bucket holds: the reply's length is the
            // server's to choose and this is the memory it is allocated into.
            if out.len() >= MAX_KEYS {
                break;
            }
            match page.next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Entry> {
        let (bucket, key) = Self::split(path);
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some(bucket) = bucket else {
            return Ok(Entry::dir(name));
        };
        if key.is_empty() {
            // A bucket, which is a directory.
            return Ok(Entry::dir(name));
        }
        let response = self.send("HEAD", Some(&bucket), &key, &[], &[], &[])?;
        let mut entry = Entry::file(name);
        entry.size = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(entry)
    }

    fn read_link(&self, path: &str) -> Result<String> {
        Err(Error::Unsupported(Box::leak(
            format!("{path}: S3 has no symbolic links").into_boxed_str(),
        )))
    }

    fn open_read(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        let (bucket, key) = Self::split(path);
        let bucket = bucket.ok_or_else(|| Error::msg(format!("{path}: no bucket in that path")))?;
        let response = self.send("GET", Some(&bucket), &key, &[], &[], &[])?;
        Ok(Box::new(response.into_body().into_reader()))
    }

    fn open_seek(&self, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        Err(Error::Unsupported(Box::leak(
            format!("{path}: an S3 read is a stream, not a seekable file").into_boxed_str(),
        )))
    }

    fn open_write(&self, path: &str) -> Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(Upload {
            fs: self.clone_handle(),
            path: path.to_string(),
            buffer: Vec::new(),
        }))
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        // A zero-byte object whose key ends in `/`, which is the convention
        // every other tool uses and the only way to make an empty prefix
        // visible: a prefix with nothing under it cannot be listed, because
        // there is nothing to list.
        let dir = normalise(path);
        let (bucket, key) = Self::split(&dir);
        let bucket = bucket.ok_or_else(|| Error::msg("a bucket cannot be created from a panel"))?;
        self.send("PUT", Some(&bucket), &key, &[], &[], &[])
            .map(|_| ())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        let (bucket, key) = Self::split(path);
        let bucket = bucket.ok_or_else(|| Error::msg(format!("{path}: no bucket in that path")))?;
        self.send("DELETE", Some(&bucket), &key, &[], &[], &[])
            .map(|_| ())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        // Only the marker object. The copy engine walks a tree and deletes its
        // members itself, so by the time this is called the prefix is empty;
        // deleting a whole prefix here would be a second, quieter recursive
        // delete that the user was never asked about.
        let dir = normalise(path);
        let (bucket, key) = Self::split(&dir);
        let bucket = bucket.ok_or_else(|| Error::msg(format!("{path}: no bucket in that path")))?;
        match self.send("DELETE", Some(&bucket), &key, &[], &[], &[]) {
            Ok(_) => Ok(()),
            // There may have been no marker object at all, which is the usual
            // case for a prefix that only ever held files. Nothing to delete
            // is not a failure to delete.
            Err(_) => Ok(()),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from_bucket, from_key) = Self::split(from);
        let (to_bucket, to_key) = Self::split(to);
        let (Some(from_bucket), Some(to_bucket)) = (from_bucket, to_bucket) else {
            return Err(Error::msg(format!("{from}: no bucket in that path")));
        };
        // S3 has no rename. This is what every client does, and it is why
        // renaming a large object is not instant.
        let source = format!(
            "/{}/{}",
            sign::encode_segment(&from_bucket),
            sign::encode_key(&from_key)
        );
        self.send(
            "PUT",
            Some(&to_bucket),
            &to_key,
            &[],
            &[],
            &[("x-amz-copy-source".to_string(), source)],
        )?;
        self.send("DELETE", Some(&from_bucket), &from_key, &[], &[], &[])
            .map(|_| ())
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    fn close(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

impl S3Fs {
    /// A second handle onto the same connection, for an upload to hold.
    fn clone_handle(&self) -> Self {
        Self {
            origin: self.origin.clone(),
            host: self.host.clone(),
            access_key: self.access_key.clone(),
            secret: self.secret.clone(),
            region: self.region.clone(),
            root: self.root.clone(),
            session_token: self.session_token.clone(),
            live: AtomicBool::new(self.live.load(Ordering::Relaxed)),
        }
    }
}

/// What the environment says about AWS credentials.
///
/// The names every AWS tool uses, so a shell already set up for `aws` or
/// `rclone` needs nothing typed here: `Ctrl+F`, `s3://s3.amazonaws.com`, and
/// the keys come from where they already are.
///
/// **Read, never written.** Nothing here puts a credential into the
/// environment, into `hosts.toml`, or into a log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    /// `AWS_ACCESS_KEY_ID`.
    pub access_key: Option<String>,
    /// `AWS_SECRET_ACCESS_KEY`.
    pub secret: Option<String>,
    /// `AWS_SESSION_TOKEN`, set by SSO and by assumed roles.
    pub session_token: Option<String>,
    /// `AWS_REGION`, or `AWS_DEFAULT_REGION` where that is the one set.
    pub region: Option<String>,
}

impl Credentials {
    /// Read them.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from(&|name: &str| std::env::var(name).ok())
    }

    /// The same, from any source, so a test needs no environment of its own.
    #[must_use]
    pub fn from(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let nonempty = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        Self {
            access_key: nonempty("AWS_ACCESS_KEY_ID"),
            secret: nonempty("AWS_SECRET_ACCESS_KEY"),
            session_token: nonempty("AWS_SESSION_TOKEN"),
            // `AWS_REGION` is the newer name and wins where both are set,
            // which is what every AWS SDK does.
            region: nonempty("AWS_REGION").or_else(|| nonempty("AWS_DEFAULT_REGION")),
        }
    }
}

/// An object being written.
///
/// A `PUT` needs its length up front and the copy engine hands bytes over as
/// it reads them, so the body is gathered and sent on `flush`. Bounded by
/// [`MAX_UPLOAD`]; past that the write is refused rather than the process
/// growing to the size of whatever was dragged onto a panel.
struct Upload {
    fs: S3Fs,
    path: String,
    buffer: Vec<u8>,
}

impl std::io::Write for Upload {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.buffer.len().saturating_add(buf.len()) > MAX_UPLOAD {
            return Err(std::io::Error::other(format!(
                "{}: larger than the {} MB a single S3 upload holds; \
                 multipart upload is not built yet",
                self.path,
                MAX_UPLOAD / (1024 * 1024)
            )));
        }
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let (bucket, key) = S3Fs::split(&self.path);
        let Some(bucket) = bucket else {
            return Err(std::io::Error::other(format!(
                "{}: no bucket in that path",
                self.path
            )));
        };
        self.fs
            .send("PUT", Some(&bucket), &key, &[], &self.buffer, &[])
            .map(|_| ())
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        self.buffer.clear();
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let _ = std::io::Write::flush(self);
        }
    }
}
