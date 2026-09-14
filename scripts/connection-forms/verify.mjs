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
  if (field.type === "select" && field.default !== undefined) {
    assert(field.options.some((option) => option.value === field.default), `${field.key}: invalid default`);
  }
  for (const locale of locales) {
    const localized = manifest.localizations[locale]?.contributions?.[provider.id]?.fields?.[field.key]
      ?? (locale === "en" ? field : undefined);
    assert(localized?.label?.trim(), `${locale}/${field.key}: missing label`);
    for (const option of field.options ?? []) {
      const label = Array.isArray(localized.options)
        ? localized.options.find((item) => item.value === option.value)?.label
        : localized.options?.[option.value];
      assert(label?.trim(), `${locale}/${field.key}/${option.value}: missing option label`);
    }
  }
}

const options = (key) => byKey[key].options.map((option) => option.value);
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
    // Object storage (S3 / OSS): endpoint+bucket+keys required.
    current.visible("endpoint", !["fs", "opendal-custom"].includes(protocol));
    current.required("endpoint", ["webdav", "ftp", "sftp", "smb", "sftp-native"].includes(protocol));
    current.visible("bucket", ["s3", "oss"].includes(protocol));
    current.required("bucket", ["s3", "oss"].includes(protocol));
    current.visible("region", protocol === "s3");
    current.visible("enable_virtual_host_style", protocol === "s3");
    current.visible("access_key_id", ["s3", "oss"].includes(protocol));
    current.required("access_key_id", ["s3", "oss"].includes(protocol));
    current.visible("secret_access_key", ["s3", "oss"].includes(protocol));
    current.required("secret_access_key", ["s3", "oss"].includes(protocol));
    // Custom OpenDAL service descriptor.
    current.visible("service", protocol === "opendal-custom");
    current.required("service", protocol === "opendal-custom");
    current.visible("config", protocol === "opendal-custom");
    // Remote service accounts, per protocol family.
    current.visible("username", ["webdav", "smb"].includes(protocol));
    current.visible("user", ["ftp", "sftp", "sftp-native"].includes(protocol));
    current.visible("share", protocol === "smb");
    current.visible("domain", protocol === "smb");
    current.visible("password", ["webdav", "ftp", "smb", "sftp-native"].includes(protocol));
    current.visible("key", ["sftp", "sftp-native"].includes(protocol));
    current.visible("known_hosts_strategy", ["sftp", "sftp-native"].includes(protocol));
    // Read-only hides (never removes) the delete toggle.
    current.visible("allow_delete", !read_only);
    // Legacy via-DBX-SSH fields must not reappear in the form.
    current.visible("connection_mode", false);
    current.visible("dbx_ssh_connection", false);
  }
}

assert.equal(byKey.key.binding, "secret");
assert.equal(byKey.key.type, "textarea");
console.log(`PASS Files connection form: ${scenarios} combinations; field ordering and seven-language labels/options`);
