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
  "token",
  "secret_access_key",
  "secret_id",
  "secret_key",
  "security_token",
  "credential",
  "account_key",
  "access_token",
  "client_secret",
  "refresh_token",
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
// generic rclone backend reusing the common fields, with its remaining
// options accepted through the JSON extras field.
const ENDPOINT_PROTOCOLS = [
  "s3", "gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp",
  "sftp-native", "smb", "koofr", "pcloud", "seafile",
  // generic backends whose service address has a dedicated field
  "azurefiles", "filefabric", "hdfs", "http", "imagekit", "netstorage",
  "qingstor", "quatrix", "sia",
];
const ENDPOINT_REQUIRED = [
  "gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp", "smb",
  "sftp-native", "koofr", "pcloud", "seafile",
  "filefabric", "hdfs", "http", "imagekit", "netstorage", "quatrix",
];
const USERNAME_VISIBLE = [
  "webdav", "smb", "pcloud", "seafile",
  "azurefiles", "cloudinary", "filen", "filescom", "hdfs", "iclouddrive",
  "internxt", "linkbox", "mega", "mailru", "netstorage", "opendrive",
  "pikpak", "protondrive", "swift", "ulozto",
];
const USERNAME_REQUIRED = [
  "pcloud", "seafile",
  "cloudinary", "filen", "iclouddrive", "internxt", "linkbox", "mega",
  "mailru", "netstorage", "opendrive", "pikpak", "protondrive",
];
const PASSWORD_VISIBLE = [
  "webdav", "ftp", "smb", "sftp-native", "koofr", "pcloud", "seafile",
  "filen", "filescom", "iclouddrive", "internxt", "linkbox", "mega",
  "mailru", "netstorage", "opendrive", "pikpak", "protondrive", "sia",
  "swift", "ulozto",
];
const PASSWORD_REQUIRED = [
  "koofr", "pcloud", "seafile",
  "iclouddrive", "internxt", "linkbox", "mega", "mailru", "netstorage",
  "opendrive", "pikpak", "protondrive",
];
const KEY_VISIBLE_PROTOCOLS = ["s3", "obs", "oss", "azurefiles", "b2", "cloudinary", "imagekit", "internetarchive", "qingstor", "sugarsync"];
const KEY_REQUIRED_PROTOCOLS = ["s3", "obs", "oss", "b2", "cloudinary", "imagekit"];
const OAUTH_CLIENT_VISIBLE = [
  "aliyun-drive", "dropbox", "gdrive", "onedrive",
  "box", "hidrive", "huaweidrive", "jottacloud", "mailru", "premiumizeme",
  "putio", "sharefile", "zoho",
];
const ACCESS_TOKEN_VISIBLE = ["dropbox", "gdrive", "onedrive", "yandex-disk", "box", "drime", "gofile"];
const TOKEN_VISIBLE = ["fichier", "filefabric", "filelu", "filen", "linkbox", "pixeldrain", "quatrix", "shade", "storj", "ulozto"];
const TOKEN_REQUIRED = ["filelu", "linkbox", "quatrix", "shade"];
const QUICK_PROTOCOLS = new Set(["fs", "aliyun-drive", "dropbox", "gdrive", "onedrive", "yandex-disk", "s3", "gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp", "sftp-native", "smb", "koofr", "pcloud", "seafile"]);
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
    current.required("endpoint", ENDPOINT_REQUIRED.includes(protocol));
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
    current.visible("access_key_id", KEY_VISIBLE_PROTOCOLS.includes(protocol));
    current.required("access_key_id", KEY_REQUIRED_PROTOCOLS.includes(protocol));
    current.visible("secret_access_key", KEY_VISIBLE_PROTOCOLS.includes(protocol));
    current.required("secret_access_key", KEY_REQUIRED_PROTOCOLS.includes(protocol));
    current.visible("secret_id", protocol === "cos");
    current.required("secret_id", protocol === "cos");
    current.visible("secret_key", protocol === "cos");
    current.required("secret_key", protocol === "cos");
    current.visible("security_token", protocol === "cos");
    current.required("security_token", false);
    // Generic rclone backends (no quick form) enter their extra options as
    // one JSON object; every other protocol hides it.
    current.visible("config", GENERIC_PROTOCOLS.includes(protocol));
    // Remote service accounts, per protocol family.
    current.visible("username", USERNAME_VISIBLE.includes(protocol));
    current.required("username", USERNAME_REQUIRED.includes(protocol));
    current.visible("user", ["ftp", "sftp", "sftp-native"].includes(protocol));
    current.visible("share", ["smb", "azurefiles"].includes(protocol));
    current.visible("domain", protocol === "smb");
    current.visible("password", PASSWORD_VISIBLE.includes(protocol));
    current.required("password", PASSWORD_REQUIRED.includes(protocol));
    current.visible("key", ["sftp", "sftp-native"].includes(protocol));
    current.visible("known_hosts_strategy", ["sftp", "sftp-native"].includes(protocol));
    // Egress proxy: deliberately absent from the form — the proxy comes from
    // the DBX host environment (HTTP_PROXY/HTTPS_PROXY inherited by the rcd
    // "direct" group). The legacy via-DBX-SSH fields must not reappear either.
    current.visible("proxy_type", false);
    current.visible("proxy_host", false);
    current.visible("proxy_port", false);
    current.visible("proxy_username", false);
    current.visible("proxy_password", false);
    current.visible("connection_mode", false);
    current.visible("dbx_ssh_connection", false);
    // SSH tunnel (rclone engine): likewise absent from the form — egress
    // networking is host-owned, and tunnels remain an external_config-only
    // feature (MCP/API), never a per-connection form field.
    current.visible("tunnel_jump_hosts", false);
    current.visible("tunnel_identity_file", false);
    current.visible("access_token", ACCESS_TOKEN_VISIBLE.includes(protocol));
    current.required("access_token", protocol === "yandex-disk");
    current.visible("client_id", OAUTH_CLIENT_VISIBLE.includes(protocol));
    current.visible("client_secret", OAUTH_CLIENT_VISIBLE.includes(protocol));
    current.visible("refresh_token", ["aliyun-drive", "dropbox", "gdrive", "onedrive"].includes(protocol));
    current.visible("token", TOKEN_VISIBLE.includes(protocol));
    current.required("token", TOKEN_REQUIRED.includes(protocol));
    current.visible("drive_type", protocol === "aliyun-drive");
    current.visible("email", protocol === "koofr");
    current.required("email", protocol === "koofr");
    current.visible("repo_name", protocol === "seafile");
    current.required("repo_name", protocol === "seafile");
    // Read-only hides (never removes) the delete toggle.
    current.visible("allow_delete", !read_only);
  }
}

// Proxy and SSH tunnel fields are banned from the form, not merely hidden:
// the DBX host environment owns egress networking (HTTP_PROXY/HTTPS_PROXY),
// so the per-connection proxy fields were dropped first and the tunnel pair
// followed. Tunnels stay configurable via external_config (MCP/API) only —
// a field reappearing here means the host-owned networking story regressed.
for (const key of [
  "proxy_type",
  "proxy_host",
  "proxy_port",
  "proxy_username",
  "proxy_password",
  "connection_mode",
  "dbx_ssh_connection",
  "tunnel_jump_hosts",
  "tunnel_identity_file",
]) {
  assert(!(key in byKey), `${key}: must stay out of the form (egress networking is host-owned)`);
}

assert.equal(byKey.key.binding, "secret");
assert.equal(byKey.key.type, "textarea");
console.log(`PASS Files connection form: ${scenarios} combinations; ordering, secret bindings, seven-language labels/descriptions/placeholders/options`);
