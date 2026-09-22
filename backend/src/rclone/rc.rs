//! Thin rclone rc HTTP API client.
//!
//! Endpoint set follows the patterns proven by yet-another-rclone-dashboard:
//! POST JSON to `{base}/{method}` with Basic Auth, treat HTTP 200 + a JSON
//! `error` field as the rclone business-error channel, and use
//! `_async: true` + `job/status` for long operations (progress/cancel live in
//! `transfers.rs`, phase C).

use std::time::Duration;

use serde_json::Value;

#[derive(Debug)]
pub enum RcError {
    /// Transport-level failure (connection refused, timeout, body read).
    Transport(reqwest::Error),
    /// Non-200 from rcd (auth problems surface here as 401).
    Http { status: u16, body: String },
    /// rcd answered 200 but the payload carries an rclone error.
    Rclone { message: String },
    /// 200 with an unparsable body.
    Malformed(String),
}

impl std::fmt::Display for RcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RcError::Transport(error) => write!(f, "rc transport error: {error}"),
            RcError::Http { status, body } => write!(f, "rc http {status}: {}", truncate(body)),
            RcError::Rclone { message } => write!(f, "{message}"),
            RcError::Malformed(body) => write!(f, "rc returned malformed JSON: {}", truncate(body)),
        }
    }
}

impl std::error::Error for RcError {}

fn truncate(text: &str) -> &str {
    match text.char_indices().nth(240) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Minimal query-component percent-encoding (no urlencoding dependency):
/// the rc `fs`/`remote` strings carry `:` and `/` which are legal in query
/// strings; everything an endpoint could misread gets escaped.
fn encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'/' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// No total timeout; connect stays bounded so a dead rcd fails fast instead
/// of hanging the caller on the OS connect stack.
fn build_unbounded_http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client with static options always builds")
}

/// Client bound to one rcd instance (base URL + session credential).
#[derive(Debug, Clone)]
pub struct RcClient {
    http: reqwest::Client,
    /// Same envelope, no total wall-clock timeout: listings assemble their
    /// body after server-side work rclone controls internally (a huge S3
    /// prefix pages ListObjectsV2 1000 keys at a time before rc answers),
    /// so the 30s client kills them mid-body (issue #49).
    unbounded_http: reqwest::Client,
    base_url: String,
    user: String,
    pass: String,
}

impl RcClient {
    pub fn new(base_url: String, user: String, pass: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client with static options always builds");
        Self {
            http,
            unbounded_http: build_unbounded_http(),
            base_url: base_url.trim_end_matches('/').to_string(),
            user,
            pass,
        }
    }

    /// Client variant for long-running transfers: no wall-clock timeout —
    /// a reqwest timeout spans the whole body, so staged uploads/pumped
    /// downloads of large files would die at 30s. Control-plane calls keep
    /// the default client.
    pub fn transfer_client(&self) -> RcClient {
        let http = reqwest::Client::builder()
            .build()
            .expect("reqwest client with static options always builds");
        RcClient {
            http,
            unbounded_http: build_unbounded_http(),
            base_url: self.base_url.clone(),
            user: self.user.clone(),
            pass: self.pass.clone(),
        }
    }

    /// One rc call: POST JSON params, unwrap rclone's result envelope.
    pub async fn call(&self, method: &str, params: &Value) -> Result<Value, RcError> {
        Self::send(&self.http, self, method, params).await
    }

    /// [`Self::call`] over the unbounded client: for calls whose body rclone
    /// only starts writing after unbounded internal work (operations/list on
    /// huge object-store prefixes). Connect stays bounded so a dead rcd
    /// still fails fast.
    pub async fn call_unbounded(&self, method: &str, params: &Value) -> Result<Value, RcError> {
        Self::send(&self.unbounded_http, self, method, params).await
    }

    async fn send(
        http: &reqwest::Client,
        ctx: &Self,
        method: &str,
        params: &Value,
    ) -> Result<Value, RcError> {
        let response = http
            .post(format!("{}/{method}", ctx.base_url))
            .basic_auth(&ctx.user, Some(&ctx.pass))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(params.to_string())
            .send()
            .await
            .map_err(RcError::Transport)?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(RcError::Transport)?;
        if !status.is_success() {
            return Err(RcError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let value: Value = serde_json::from_str(&body).map_err(|_| RcError::Malformed(body))?;
        // `error` must be non-empty to count: `job/status` carries
        // `"error": ""` on every running/successful job (live-verified
        // v1.75.1), and sync.rs polls it through this envelope.
        if let Some(error) = value
            .get("error")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
        {
            return Err(RcError::Rclone {
                message: error.to_string(),
            });
        }
        Ok(value)
    }

    /// Cheap liveness/auth probe (also used as spawn health check).
    pub async fn noop(&self) -> Result<(), RcError> {
        self.call("rc/noopauth", &serde_json::json!({})).await?;
        Ok(())
    }

    pub async fn version(&self) -> Result<Value, RcError> {
        self.call("core/version", &serde_json::json!({})).await
    }

    /// Registers `name` as a remote. Sensitive parameters are obscured by
    /// rcd itself when `obscure` is set — they still transit this loopback
    /// call, but are stored scrambled in the temp config file.
    pub async fn config_create(
        &self,
        name: &str,
        backend_type: &str,
        parameters: Value,
        obscure: bool,
    ) -> Result<Value, RcError> {
        self.call(
            "config/create",
            &serde_json::json!({
                "name": name,
                "type": backend_type,
                "parameters": parameters,
                "opt": { "obscure": obscure, "nonInteractive": true },
            }),
        )
        .await
    }

    pub async fn config_delete(&self, name: &str) -> Result<Value, RcError> {
        self.call("config/delete", &serde_json::json!({ "name": name }))
            .await
    }

    pub async fn config_dump(&self) -> Result<Value, RcError> {
        self.call("config/dump", &serde_json::json!({})).await
    }

    /// Lists `remote:path`. Non-recursive by default; `opt` carries extras
    /// (`recurse`, `maxDepth`, `filesOnly`, `dirsOnly`) verbatim.
    ///
    /// Unbounded total timeout (issue #49): rclone answers only after its own
    /// internal S3 pagination finishes, so a huge prefix routinely outlives
    /// the 30s control-plane client.
    pub async fn operations_list(
        &self,
        fs: &str,
        remote: &str,
        opt: Value,
    ) -> Result<Value, RcError> {
        let mut payload = serde_json::json!({ "fs": fs, "remote": remote });
        if !opt.is_null() {
            payload["opt"] = opt;
        }
        self.call_unbounded("operations/list", &payload).await
    }

    pub async fn operations_stat(&self, fs: &str, remote: &str) -> Result<Value, RcError> {
        self.call(
            "operations/stat",
            &serde_json::json!({ "fs": fs, "remote": remote }),
        )
        .await
    }

    /// Recursive file count + byte total for a path.
    pub async fn operations_size(&self, fs: &str, remote: &str) -> Result<Value, RcError> {
        self.call(
            "operations/size",
            &serde_json::json!({ "fs": fs, "remote": remote }),
        )
        .await
    }

    /// Remote space usage (`free`/`total`/`used`, cloud backends add
    /// `trash`). Local fs reports the underlying volume.
    pub async fn operations_about(&self, fs: &str) -> Result<Value, RcError> {
        self.call("operations/about", &serde_json::json!({ "fs": fs })).await
    }

    /// Starts an async src↔dst comparison (`operations/check`). Poll via
    /// `job/status`; the finished record carries `output` with
    /// missingOnSrc/missingOnDst/differ/error lists (live-verified v1.75.1).
    pub async fn operations_check_async(
        &self,
        src_fs: &str,
        dst_fs: &str,
        one_way: bool,
        download: bool,
        group: &str,
    ) -> Result<Value, RcError> {
        let mut body = serde_json::json!({
            "srcFs": src_fs,
            "dstFs": dst_fs,
            "_async": true,
            "_group": group,
        });
        if one_way {
            body["oneWay"] = Value::Bool(true);
        }
        if download {
            body["download"] = Value::Bool(true);
        }
        self.call("operations/check", &body).await
    }

    /// SUM lines for every object under `fs`/`remote`
    /// (`{hashType, hashsum: ["<hash>  <rel path>", ...]}`).
    pub async fn operations_hashsum(
        &self,
        fs: &str,
        remote: &str,
        hash_type: &str,
        download: bool,
    ) -> Result<Value, RcError> {
        self.call(
            "operations/hashsum",
            &serde_json::json!({
                "fs": fs,
                "remote": remote,
                "hashType": hash_type,
                "download": download,
            }),
        )
        .await
    }

    /// Recursive filename search (`operations/list` + rc-level filter
    /// params, live-verified v1.75.1): `include` glob and `ignore_case` ride
    /// at the TOP level of the body (not inside `opt`), `opt` carries
    /// `recurse`/`filesOnly`. Non-matching directories are pruned by rclone.
    pub async fn operations_list_filtered(
        &self,
        fs: &str,
        remote: &str,
        include_glob: &str,
        files_only: bool,
    ) -> Result<Value, RcError> {
        let mut opt = serde_json::json!({ "recurse": true });
        if files_only {
            opt["filesOnly"] = Value::Bool(true);
        }
        self.call_unbounded(
            "operations/list",
            &serde_json::json!({
                "fs": fs,
                "remote": remote,
                "include": [include_glob],
                "ignore_case": true,
                "opt": opt,
            }),
        )
        .await
    }

    /// Downloads `url` and uploads it to `fs`/`remote` server-side (the rcd
    /// host fetches it). With `auto_filename` the name comes from the URL;
    /// the answer is `{}` — callers derive the final path themselves.
    pub async fn operations_copyurl(
        &self,
        fs: &str,
        remote: &str,
        url: &str,
        auto_filename: bool,
    ) -> Result<Value, RcError> {
        self.call(
            "operations/copyurl",
            &serde_json::json!({
                "fs": fs,
                "remote": remote,
                "url": url,
                "autoFilename": auto_filename,
                "no_check": false,
            }),
        )
        .await
    }

    /// Starts an rclone serve instance over `fs` (serve/start, live-verified
    /// v1.75.1): `addr: "127.0.0.1:0"` makes rcd pick a free loopback port
    /// and report it back — the answer is `{"addr": "127.0.0.1:<port>",
    /// "id": "http-<suffix>"}`. `serve_type` is a bare serve family name
    /// ("http" / "webdav"); the full fs path (named remote included) goes
    /// into `fs` verbatim.
    pub async fn serve_start(
        &self,
        fs: &str,
        serve_type: &str,
        addr: &str,
    ) -> Result<Value, RcError> {
        self.call(
            "serve/start",
            &serde_json::json!({
                "type": serve_type,
                "fs": fs,
                "addr": addr,
            }),
        )
        .await
    }

    /// Stops one serve instance by id (serve/stop). rclone answers `{}`;
    /// an unknown id answers an rclone error — the caller decides whether
    /// that means idempotent success.
    pub async fn serve_stop(&self, id: &str) -> Result<Value, RcError> {
        self.call("serve/stop", &serde_json::json!({ "id": id })).await
    }

    /// Lists the rcd process's active serve instances (serve/list):
    /// `{"list": [{"id", "addr", "params": {"addr", "fs", "type"}}]}`.
    /// Bookkeeping only — serves die with the rcd process (respawn clears
    /// the list), so callers must tolerate stale ids.
    pub async fn serve_list(&self) -> Result<Value, RcError> {
        self.call("serve/list", &serde_json::json!({})).await
    }

    /// Empties the remote's trash (fs-level; local fs rejects with
    /// "doesn't support cleanup", which surfaces to the caller).
    pub async fn operations_cleanup(&self, fs: &str) -> Result<Value, RcError> {
        self.call("operations/cleanup", &serde_json::json!({ "fs": fs })).await
    }

    /// Removes every empty directory under `fs`/`remote` (empty `remote` =
    /// connection root). Answer is `{}` regardless of how many went away.
    pub async fn operations_rmdirs(&self, fs: &str, remote: &str) -> Result<Value, RcError> {
        self.call(
            "operations/rmdirs",
            &serde_json::json!({ "fs": fs, "remote": remote }),
        )
        .await
    }

    /// Queries (rate `None`) or sets the rcd process's bandwidth limit.
    /// `rate` takes rclone bwlimit spellings — `"10M"`, `"1M:100k"`, `"off"`;
    /// an unparsable value answers an RcError (HTTP 500 `bad bwlimit: ...`,
    /// live-verified v1.75.1). The setting is per-rcd-process: every proxy
    /// group's rcd needs its own call, and a respawned rcd needs a replay.
    pub async fn core_bwlimit(&self, rate: Option<&str>) -> Result<Value, RcError> {
        let mut payload = serde_json::Map::new();
        if let Some(rate) = rate {
            payload.insert("rate".into(), Value::String(rate.to_string()));
        }
        self.call("core/bwlimit", &Value::Object(payload)).await
    }

    /// Backend feature/capability report — input to the `files/capabilities`
    /// projection (phase A wires a conservative per-protocol matrix on top).
    pub async fn backend_features(&self, fs: &str, remote: &str) -> Result<Value, RcError> {
        self.call(
            "backend/features",
            &serde_json::json!({ "fs": fs, "remote": remote }),
        )
        .await
    }

    /// Refreshes the directory cache of an ACTIVE VFS (`vfs/refresh`).
    /// `fs` selects the mount (the same fs string `mount/mount` was given —
    /// rclone canonicalizes both sides, so `None` picks the only VFS when
    /// exactly one is active). `dir` is the mount-root-relative directory
    /// to re-read (`None` = the mount root); `recursive` walks the whole
    /// subtree. Live-verified v1.75.1 against rcd + source-pinned
    /// (`vfs/rc.go`): `recursive` is parsed with `strconv.ParseBool` from a
    /// STRING — a JSON boolean answers `value must be string
    /// "recursive"=true`; and there is no `remote` param on this endpoint —
    /// leftover request keys must carry a `dir` prefix or rclone answers
    /// `unknown key`.
    pub async fn vfs_refresh(
        &self,
        fs: &str,
        dir: Option<&str>,
        recursive: bool,
    ) -> Result<Value, RcError> {
        let mut payload = serde_json::json!({
            "fs": fs,
            "recursive": if recursive { "true" } else { "false" },
        });
        if let Some(dir) = dir
            .map(|dir| dir.trim_matches('/'))
            .filter(|dir| !dir.is_empty())
        {
            payload["dir"] = Value::String(dir.to_string());
        }
        self.call("vfs/refresh", &payload).await
    }

    /// Stats of an ACTIVE VFS (`vfs/stats`): `{fs, inUse, metadataCache,
    /// opt}` plus `diskCache` when the mount runs a VFS cache mode > off
    /// (byte usage lives in `diskCache.bytesUsed`). Only `fs` is consumed;
    /// this endpoint ignores rather than validates extra keys.
    pub async fn vfs_stats(&self, fs: &str) -> Result<Value, RcError> {
        self.call("vfs/stats", &serde_json::json!({ "fs": fs })).await
    }

    /// rc-serve URL for byte reads. rcserver routes GET paths with the
    /// `^\[(.*?)\](.*)$` regex, so the fs component MUST be bracket-wrapped —
    /// plain forms 404 for every path (live-verified v1.68–1.75). Idempotent:
    /// callers may pass a raw fs (bracketed here) or their own already
    /// bracketed one (ops/bytes_channel conventions).
    pub fn serve_url(&self, fs: &str, remote: &str) -> String {
        let bracketed;
        let fs_component = if fs.starts_with('[') {
            fs
        } else {
            bracketed = format!("[{fs}]");
            &bracketed
        };
        let remote_trim = remote.trim_matches('/');
        if remote_trim.is_empty() {
            format!("{}/{}", self.base_url, fs_component)
        } else {
            format!("{}/{}/{}", self.base_url, fs_component, remote_trim)
        }
    }

    /// Byte read via `--rc-serve` with optional inclusive Range. Returns the
    /// raw response (streaming body); a 416 for out-of-range starts maps to
    /// `RcError::Http`. Caller inspects status (200 vs 206) for truncation.
    pub async fn serve_get(
        &self,
        fs: &str,
        remote: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<reqwest::Response, RcError> {
        let url = self.serve_url(fs, remote);
        let mut request = self
            .http
            .get(&url)
            .basic_auth(&self.user, Some(&self.pass));
        if let Some((start, end)) = range {
            let value = match end {
                Some(end) => format!("bytes={start}-{end}"),
                None => format!("bytes={start}-"),
            };
            request = request.header(reqwest::header::RANGE, value);
        }
        let response = request.send().await.map_err(RcError::Transport)?;
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            Err(RcError::Http {
                status: status.as_u16(),
                body: response.text().await.unwrap_or_default(),
            })
        }
    }

    /// Multipart streaming upload via `operations/uploadfile` (rclone
    /// >= 1.68). `file` is streamed from disk, not buffered whole.
    pub async fn operations_uploadfile(
        &self,
        fs: &str,
        remote: &str,
        file: &std::path::Path,
        mime: Option<&str>,
    ) -> Result<Value, RcError> {
        let mut part = reqwest::multipart::Part::file(file)
            .await
            .map_err(|error| RcError::Malformed(format!("staging file unreadable: {error}")))?;
        part = part.file_name("payload");
        if let Some(mime) = mime {
            // Mime strings come from our own extension mapping; an invalid
            // one is a caller bug, not a user-facing condition.
            part = part
                .mime_str(mime)
                .map_err(|error| RcError::Malformed(format!("invalid mime {mime}: {error}")))?;
        }
        let form = reqwest::multipart::Form::new().part("file", part);
        let url = format!(
            "{}/operations/uploadfile?fs={}&remote={}",
            self.base_url,
            encode_query_component(fs),
            encode_query_component(remote)
        );
        let response = self
            .http
            .post(url)
            .basic_auth(&self.user, Some(&self.pass))
            .multipart(form)
            .send()
            .await
            .map_err(RcError::Transport)?;
        let status = response.status();
        let body = response.text().await.map_err(RcError::Transport)?;
        if !status.is_success() {
            return Err(RcError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let value: Value =
            serde_json::from_str(&body).map_err(|_| RcError::Malformed(body))?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return Err(RcError::Rclone {
                message: error.to_string(),
            });
        }
        Ok(value)
    }
}
