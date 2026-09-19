// Standalone Files connection-form contract verifier.
//
// The monorepo version imports the DBX host's TypeScript condition evaluator.
// This copy intentionally keeps only the small, host-compatible evaluator and
// Files assertions needed by this repository, so clean clones do not need ../host
// or ../shared. Replace this file with the future public form-contract package
// when that package is available; keep the scenarios below as the Files
// regression contract.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const root = new URL("../../", import.meta.url);
const manifest = JSON.parse(readFileSync(new URL("manifest.json", root), "utf8"));
const provider = manifest.contributions.find((item) => item.type === "connection-provider");
assert(provider, "Files manifest must define a connection provider");
const fields = provider.fields;
const byKey = Object.fromEntries(fields.map((field) => [field.key, field]));
const defaults = Object.fromEntries(fields.map((field) => [field.key, field.default]));
const locales = ["en", "zh-CN", "zh-TW", "es", "it", "ja", "pt-BR"];

assert.equal(new Set(fields.map((field) => field.key)).size, fields.length, "duplicate field keys");

function conditionMatches(condition, value) {
  if (!condition) return true;
  if (value === undefined || value === null || (typeof value === "string" && value.trim() === "")) return false;
  return condition.one_of.includes(String(value));
}

function isVisible(field, values, seen = new Set([field.key])) {
  const condition = field.visible_when;
  if (!condition || !conditionMatches(condition, values[condition.field])) return !condition;
  const target = byKey[condition.field];
  if (!target || seen.has(target.key)) return true;
  seen.add(target.key);
  return isVisible(target, values, seen);
}

function isRequired(field, values) {
  return Boolean(field.required)
    || Boolean(field.required_when && conditionMatches(field.required_when, values[field.required_when.field]));
}

for (const [index, field] of fields.entries()) {
  for (const condition of [field.visible_when, field.required_when].filter(Boolean)) {
    const target = byKey[condition.field];
    assert(target, `${field.key}: unknown condition field ${condition.field}`);
    assert(fields.indexOf(target) < index, `${field.key}: condition target must precede dependent field`);
    const values = target.type === "boolean" ? ["true", "false"] : target.options?.map((option) => option.value);
    if (values) assert(condition.one_of.every((value) => values.includes(value)), `${field.key}: invalid condition value`);
  }
  // Conflict guard: a conditionally required field must stay visible in
  // every state where it is required, otherwise the host demands a value
  // the user cannot type ("required but hidden" combination).
  if (field.required_when) {
    assert(field.visible_when, `${field.key}: required_when without visible_when`);
    assert(
      field.required_when.one_of.every((value) => field.visible_when.one_of.includes(value)),
      `${field.key}: required_when values outside visible_when`,
    );
  }
  // A statically required field must never be conditionally hidden: once the
  // select/boolean state hides it, the host still refuses an empty submit.
  if (field.required && field.visible_when) {
    assert.fail(`${field.key}: statically required field must not carry visible_when`);
  }
  if (field.type === "select") {
    // A duplicated option value renders as one indistinguishable entry and
    // silently drops the other label from the dropdown.
    const values = field.options.map((option) => option.value);
    assert.equal(new Set(values).size, values.length, `${field.key}: duplicate option values`);
    if (field.default !== undefined) {
      assert(field.options.some((option) => option.value === field.default), `${field.key}: invalid default`);
    }
  }
  for (const locale of locales) {
    const localized = manifest.localizations[locale]?.contributions?.[provider.id]?.fields?.[field.key]
      ?? (locale === "en" ? field : undefined);
    assert(localized?.label?.trim(), `${locale}/${field.key}: missing label`);
    // Descriptions and placeholders carry the combination semantics (which
    // protocol shows what, defaults, units); a missing translation degrades
    // the form to English mid-sentence in that locale.
    if (field.description) {
      assert(localized?.description?.trim(), `${locale}/${field.key}: missing description`);
    }
    if (field.placeholder) {
      assert(localized?.placeholder?.trim(), `${locale}/${field.key}: missing placeholder`);
    }
    for (const option of field.options ?? []) {
      const label = Array.isArray(localized.options)
        ? localized.options.find((item) => item.value === option.value)?.label
        : localized.options?.[option.value];
      assert(label?.trim(), `${locale}/${field.key}/${option.value}: missing option label`);
    }
  }
}

// Secret-binding contract: credential fields must bind to the host's secret
// store (lifecycle `connection_secrets`, masked input) — never to the config
// binding, which is persisted in plaintext alongside the connection record.
// `password` inputs must always be secret-bound so the host masks them.
const SECRET_FIELDS = new Set([
  "key",
  "password",
  "secret_access_key",
  "secret_id",
  "secret_key",
  "security_token",
  "credential",
  "account_key",
  "access_token",
  "client_secret",
  "refresh_token",
  "proxy_password",
]);
for (const field of fields) {
  if (field.type === "password") {
    assert.equal(field.binding, "secret", `${field.key}: password input must bind to secret`);
  }
  if (field.binding === "secret") {
    assert(
      field.type === "password" || field.type === "textarea",
      `${field.key}: secret binding expects masked/multiline input, got ${field.type}`,
    );
    assert(
      SECRET_FIELDS.has(field.key),
      `${field.key}: new secret-bound field — update the SECRET_FIELDS contract deliberately`,
    );
  }
}
for (const key of SECRET_FIELDS) {
  assert.equal(byKey[key].binding, "secret", `${key}: must stay secret-bound`);
}

const options = (key) => byKey[key].options.map((option) => option.value);
// Protocols with a dedicated quick form; every other protocol value is a
// generic rclone backend entering its parameters through the JSON field.
const ENDPOINT_PROTOCOLS = ["s3", "gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp", "sftp-native", "smb", "koofr", "pcloud", "seafile"];
const QUICK_PROTOCOLS = new Set(["fs", "aliyun-drive", "dropbox", "gdrive", "onedrive", "yandex-disk", ...ENDPOINT_PROTOCOLS]);
const GENERIC_PROTOCOLS = options("protocol").filter((value) => !QUICK_PROTOCOLS.has(value));
assert(GENERIC_PROTOCOLS.length > 0, "generic rclone protocol options must exist");
let scenarios = 0;
function state(overrides) {
  scenarios++;
  const values = { ...defaults, ...overrides };
  const visible = new Set(fields.filter((field) => isVisible(field, values)).map((field) => field.key));
  const required = new Set(fields.filter((field) => visible.has(field.key) && isRequired(field, values)).map((field) => field.key));
  return {
    visible(key, expected) { assert.equal(visible.has(key), expected, `${key} visibility: ${JSON.stringify(overrides)}`); },
    required(key, expected) { assert.equal(required.has(key), expected, `${key} required: ${JSON.stringify(overrides)}`); },
  };
}

for (const protocol of options("protocol")) {
  for (const read_only of [true, false]) {
    const current = state({ protocol, read_only });
    // Object storage (S3 / OSS): bucket+keys required; endpoint required for
    // OSS (no default endpoint) but optional for S3 (AWS default endpoint).
    current.visible("endpoint", ENDPOINT_PROTOCOLS.includes(protocol));
    current.required("endpoint", ["gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp", "smb", "sftp-native", "koofr", "pcloud", "seafile"].includes(protocol));
    current.visible("bucket", ["s3", "gcs", "obs", "oss", "cos"].includes(protocol));
    // Bucket namespace (2026-09-17): s3/oss/cos/obs accept an empty bucket —
    // the connection root then lists all buckets and the first path segment
    // selects one. gcs stays required (its ListBuckets needs an OAuth token
    // exchange; phase 2).
    current.required("bucket", protocol === "gcs");
    current.visible("container", protocol === "azblob");
    current.required("container", false);
    current.visible("account_name", protocol === "azblob");
    current.required("account_name", protocol === "azblob");
    current.visible("account_key", protocol === "azblob");
    current.required("account_key", protocol === "azblob");
    current.visible("credential", protocol === "gcs");
    current.required("credential", protocol === "gcs");
    current.visible("scope", protocol === "gcs");
    current.visible("region", protocol === "s3");
    current.visible("enable_virtual_host_style", protocol === "s3");
    current.visible("access_key_id", ["s3", "obs", "oss"].includes(protocol));
    current.required("access_key_id", ["s3", "obs", "oss"].includes(protocol));
    current.visible("secret_access_key", ["s3", "obs", "oss"].includes(protocol));
    current.required("secret_access_key", ["s3", "obs", "oss"].includes(protocol));
    current.visible("secret_id", protocol === "cos");
    current.required("secret_id", protocol === "cos");
    current.visible("secret_key", protocol === "cos");
    current.required("secret_key", protocol === "cos");
    current.visible("security_token", protocol === "cos");
    current.required("security_token", false);
    // Generic rclone backends (no quick form) enter their parameters as one
    // JSON object; every other protocol hides it.
    current.visible("config", GENERIC_PROTOCOLS.includes(protocol));
    // Remote service accounts, per protocol family.
    current.visible("username", ["webdav", "smb", "pcloud", "seafile"].includes(protocol));
    current.required("username", ["pcloud", "seafile"].includes(protocol));
    current.visible("user", ["ftp", "sftp", "sftp-native"].includes(protocol));
    current.visible("share", protocol === "smb");
    current.visible("domain", protocol === "smb");
    current.visible("password", ["webdav", "ftp", "smb", "sftp-native", "koofr", "pcloud", "seafile"].includes(protocol));
    current.required("password", ["koofr", "pcloud", "seafile"].includes(protocol));
    current.visible("key", ["sftp", "sftp-native"].includes(protocol));
    current.visible("known_hosts_strategy", ["sftp", "sftp-native"].includes(protocol));
    // Egress proxy (rclone engine, 2026-09-19): the selector shows for the
    // FTP/SFTP family only. The dependent host/port/user/password fields stay
    // hidden at the default proxy_type=off — dedicated scenarios below cover
    // the visible/required states once a proxy type is picked.
    current.visible("proxy_type", ["ftp", "sftp", "sftp-native"].includes(protocol));
    current.required("proxy_type", false);
    current.visible("proxy_host", false);
    current.required("proxy_host", false);
    current.visible("proxy_port", false);
    current.required("proxy_port", false);
    current.visible("proxy_username", false);
    current.visible("proxy_password", false);
    // SSH tunnel (rclone engine, 2026-09-19): the flat ssh -J pair is shown
    // for every protocol except fs and is never required. The dedicated
    // tunnel scenarios below re-check the boundary states.
    current.visible("tunnel_jump_hosts", protocol !== "fs");
    current.required("tunnel_jump_hosts", false);
    current.visible("tunnel_identity_file", protocol !== "fs");
    current.required("tunnel_identity_file", false);
    current.visible("access_token", ["dropbox", "gdrive", "onedrive", "yandex-disk"].includes(protocol));
    current.required("access_token", protocol === "yandex-disk");
    current.visible("client_id", ["aliyun-drive", "dropbox", "gdrive", "onedrive"].includes(protocol));
    current.visible("client_secret", ["aliyun-drive", "dropbox", "gdrive", "onedrive"].includes(protocol));
    current.visible("refresh_token", ["aliyun-drive", "dropbox", "gdrive", "onedrive"].includes(protocol));
    current.visible("drive_type", protocol === "aliyun-drive");
    current.visible("email", protocol === "koofr");
    current.required("email", protocol === "koofr");
    current.visible("repo_name", protocol === "seafile");
    current.required("repo_name", protocol === "seafile");
    // Read-only hides (never removes) the delete toggle.
    current.visible("allow_delete", !read_only);
    // Legacy via-DBX-SSH fields must not reappear in the form.
    current.visible("connection_mode", false);
    current.visible("dbx_ssh_connection", false);
  }
}

// Egress proxy regression scenarios (rclone engine connection proxy).
//
// (a) FTP + HTTP proxy: the dependent proxy fields become visible once a
// proxy type is picked; host and port are conditionally required while the
// credentials stay optional (anonymous proxies are legitimate).
const ftpProxy = state({ protocol: "ftp", read_only: false, proxy_type: "http" });
ftpProxy.visible("proxy_type", true);
ftpProxy.visible("proxy_host", true);
ftpProxy.required("proxy_host", true);
ftpProxy.visible("proxy_port", true);
ftpProxy.required("proxy_port", true);
ftpProxy.visible("proxy_username", true);
ftpProxy.required("proxy_username", false);
ftpProxy.visible("proxy_password", true);
ftpProxy.required("proxy_password", false);

// (b) S3 never surfaces proxy fields: proxy_type itself is hidden by the
// protocol gate, and the visible_when chain (proxy_host -> proxy_type ->
// protocol) hides every dependent field even when a stale proxy_type leaks
// in from a saved state.
const s3Proxy = state({ protocol: "s3", read_only: false, proxy_type: "http" });
s3Proxy.visible("proxy_type", false);
s3Proxy.visible("proxy_host", false);
s3Proxy.required("proxy_host", false);
s3Proxy.visible("proxy_port", false);
s3Proxy.required("proxy_port", false);
s3Proxy.visible("proxy_username", false);
s3Proxy.required("proxy_username", false);
s3Proxy.visible("proxy_password", false);
s3Proxy.required("proxy_password", false);

// (c) SFTP with proxy off: host/port are neither visible nor required, and
// no dependent proxy field leaks into the form.
const sftpOff = state({ protocol: "sftp", read_only: false, proxy_type: "off" });
sftpOff.visible("proxy_type", true);
sftpOff.visible("proxy_host", false);
sftpOff.required("proxy_host", false);
sftpOff.visible("proxy_port", false);
sftpOff.required("proxy_port", false);
sftpOff.visible("proxy_username", false);
sftpOff.required("proxy_username", false);
sftpOff.visible("proxy_password", false);
sftpOff.required("proxy_password", false);

// SSH tunnel contract (rclone engine, flat ssh -J fields, 2026-09-19): the
// visible_when gate must target protocol and cover exactly the protocol
// option set minus fs (the generic loop above already asserts target
// precedence and that every gate value is a real protocol option).
for (const key of ["tunnel_jump_hosts", "tunnel_identity_file"]) {
  const gate = byKey[key].visible_when;
  assert.equal(gate?.field, "protocol", `${key}: tunnel gate must target protocol`);
  assert.deepEqual(
    [...gate.one_of].sort(),
    options("protocol").filter((value) => value !== "fs").sort(),
    `${key}: tunnel gate must cover every protocol except fs`,
  );
  assert.equal(byKey[key].required_when, undefined, `${key}: tunnel fields are always optional`);
  assert.equal(byKey[key].binding, "config", `${key}: tunnel fields bind to config`);
}

// (d) S3 surfaces the tunnel pair, both optional: object storage without any
// FTP/SFTP baggage can still reach a private network through the jump chain.
const s3Tunnel = state({ protocol: "s3", read_only: false });
s3Tunnel.visible("tunnel_jump_hosts", true);
s3Tunnel.required("tunnel_jump_hosts", false);
s3Tunnel.visible("tunnel_identity_file", true);
s3Tunnel.required("tunnel_identity_file", false);

// (e) fs hides the tunnel pair entirely: the gate excludes fs, so neither
// field (nor any requirement) can leak into a local-filesystem form.
const fsTunnel = state({ protocol: "fs", read_only: false });
fsTunnel.visible("tunnel_jump_hosts", false);
fsTunnel.required("tunnel_jump_hosts", false);
fsTunnel.visible("tunnel_identity_file", false);
fsTunnel.required("tunnel_identity_file", false);

assert.equal(byKey.key.binding, "secret");
assert.equal(byKey.key.type, "textarea");
console.log(`PASS Files connection form: ${scenarios} combinations; ordering, secret bindings, seven-language labels/descriptions/placeholders/options`);
