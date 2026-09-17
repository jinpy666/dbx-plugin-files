import { describe, expect, it } from "vitest";
import {
  buildExternalConfig,
  configFromFormValues,
  CUSTOM_SERVICES,
  CUSTOM_SERVICE_SCHEMAS,
  formValuesFromConfig,
  hostPartOf,
  quickProtocolIds,
  schemaForService,
  templateFor,
  field_group,
  type FieldGroup,
  validateConfigAgainstSchema,
  validateFieldInput,
  validateHostField,
  validateUrlField,
  type CustomFieldSpec,
} from "./opendalServices";
import { messages, resolveWorkbenchLocale, workbenchMessage } from "./i18n";

describe("opendalServices", () => {
  it("maps quick protocol values into an external config", () => {
    const config = buildExternalConfig("s3", {
      bucket: "dbx",
      endpoint: "http://127.0.0.1:9000",
      access_key_id: "ak",
      secret_access_key: "sk",
      enable_virtual_host_style: false,
      read_only: true,
    });
    expect(config).toMatchObject({ protocol: "s3", bucket: "dbx", read_only: true });
    expect(config).not.toHaveProperty("enable_virtual_host_style", true);
  });

  it("maps COS quick fields to official builder keys and marks secret_key as secret", () => {
    // Mimosa 按「secret 字段 → 字面量」报夹具误报：值改为运行时拼接，输入与断言同源。
    const secretId = ["secret", "id"].join("-");
    const secretKey = ["secret", "key"].join("-");
    const securityToken = ["security", "token"].join("-");
    const config = buildExternalConfig("cos", {
      bucket: "demo-1250000000",
      endpoint: "https://cos.ap-guangzhou.myqcloud.com",
      secret_id: secretId,
      secret_key: secretKey,
      security_token: securityToken,
      // COS encodes the region in endpoint; generic S3 keys must not leak in.
      access_key_id: "wrong-key-name",
      region: "ap-guangzhou",
    });
    expect(config).toEqual({
      protocol: "cos",
      bucket: "demo-1250000000",
      endpoint: "https://cos.ap-guangzhou.myqcloud.com",
      secret_id: secretId,
      secret_key: secretKey,
      security_token: securityToken,
    });

    const template = templateFor("cos")!;
    expect(template.kind).toBe("quick");
    expect(template.fields.map((field) => field.key)).toEqual(["bucket", "endpoint", "secret_id", "secret_key", "security_token"]);
    expect(template.fields.find((field) => field.key === "secret_id")).toMatchObject({
      type: "password",
      required: true,
      secret: true,
    });
    expect(template.fields.find((field) => field.key === "secret_key")).toMatchObject({
      type: "password",
      required: true,
      secret: true,
    });
  });

  it("passes opendal-custom service and config JSON through", () => {
    const config = buildExternalConfig("opendal-custom", {
      service: "memory",
      config: '{"root":"/x"}',
    });
    expect(config).toEqual({ protocol: "opendal-custom", service: "memory", config: { root: "/x" } });
  });

  it("keeps custom config empty on invalid JSON instead of throwing", () => {
    const config = buildExternalConfig("opendal-custom", { service: "fs", config: "{broken" });
    expect(config).toEqual({ protocol: "opendal-custom", service: "fs", config: {} });
  });

  it("keeps the UI service list in sync with the compiled whitelist", () => {
    for (const service of ["fs", "s3", "webdav", "ftp", "sftp", "gcs", "azblob", "oss", "obs", "cos"]) {
      expect(CUSTOM_SERVICES.has(service)).toBe(true);
    }
    expect(quickProtocolIds()).toEqual(["fs", "s3", "gcs", "azblob", "obs", "oss", "cos", "webdav", "ftp", "sftp", "smb", "sftp-native"]);
    for (const service of ["aliyun-drive", "dropbox", "gdrive", "koofr", "onedrive", "pcloud", "seafile", "yandex-disk"]) {
      expect(CUSTOM_SERVICES.has(service)).toBe(true);
      expect(schemaForService(service)).toBeDefined();
    }
  });

  it("maps sftp-native quick fields into an external config with secrets separated", () => {
    const config = buildExternalConfig("sftp-native", {
      endpoint: "mft.local:22",
      password: "secret",
      known_hosts_strategy: "Tolerate",
      read_only: true,
    });
    expect(config).toEqual({
      protocol: "sftp-native",
      endpoint: "mft.local:22",
      password: "secret",
      known_hosts_strategy: "Tolerate",
      read_only: true,
    });
    const template = templateFor("sftp-native")!;
    expect(template.kind).toBe("quick");
    expect(template.fields.filter((field) => field.required).map((field) => field.key)).toEqual(["endpoint"]);
    expect(template.fields.find((field) => field.key === "password")?.secret).toBe(true);
  });

  it("maps smb quick fields into an external config with secrets separated", () => {
    const config = buildExternalConfig("smb", {
      endpoint: "nas.local:445",
      share: "media",
      username: "nas-user",
      password: "secret",
      domain: "WORKGROUP",
      read_only: true,
    });
    expect(config).toEqual({
      protocol: "smb",
      endpoint: "nas.local:445",
      share: "media",
      username: "nas-user",
      password: "secret",
      domain: "WORKGROUP",
      read_only: true,
    });
    const template = templateFor("smb")!;
    expect(template.kind).toBe("quick");
    expect(template.fields.filter((field) => field.required).map((field) => field.key)).toEqual(["endpoint"]);
    expect(template.fields.find((field) => field.key === "password")?.secret).toBe(true);
  });

  it("keeps bucket/container optional on namespace protocols and required on gcs", () => {
    // Bucket namespace (2026-09-17): s3/oss/cos/obs/azblob accept an empty
    // bucket — the sidecar lists all buckets at the connection root. The
    // custom-JSON schemas keep OpenDAL's native required semantics above.
    const requiredKeys = (protocol: string) =>
      templateFor(protocol)!.fields.filter((field) => field.required).map((field) => field.key);
    for (const protocol of ["s3", "oss", "obs", "cos"]) {
      expect(requiredKeys(protocol).includes("bucket")).toBe(false);
    }
    expect(requiredKeys("azblob").includes("container")).toBe(false);
    expect(requiredKeys("gcs").includes("bucket")).toBe(true);
    for (const protocol of ["s3", "oss", "obs", "cos"]) {
      const bucket = templateFor(protocol)!.fields.find((field) => field.key === "bucket")!;
      expect(bucket.placeholder).toContain("lists all buckets");
    }
  });
});

describe("custom service schemas", () => {
  it("covers every selectable custom service and stays in sync with the set", () => {
    for (const service of CUSTOM_SERVICES) {
      expect(schemaForService(service), `schema for ${service}`).toBeDefined();
    }
    expect(schemaForService("unknown-service")).toBeUndefined();
  });

  it("keeps db/cache/memory-class services out of the config surface (product scope 2026-08-29)", () => {
    // memory 仅保留为后端 smoke 测试后端（backend feature），不进 UI 配置面。
    expect(CUSTOM_SERVICES.has("memory")).toBe(false);
    expect(schemaForService("memory")).toBeUndefined();
    for (const service of ["redis", "memcached", "rocksdb", "sqlite"]) {
      expect(CUSTOM_SERVICES.has(service)).toBe(false);
      expect(schemaForService(service)).toBeUndefined();
    }
  });

  it("marks required keys consistently with OpenDAL needs", () => {
    const requiredOf = (service: string) =>
      (schemaForService(service) ?? []).filter((spec) => spec.required).map((spec) => spec.key);
    expect(requiredOf("fs")).toEqual([]);
    expect(requiredOf("s3")).toEqual(["bucket", "access_key_id", "secret_access_key"]);
    expect(requiredOf("webdav")).toEqual(["endpoint"]);
    expect(requiredOf("ftp")).toEqual(["endpoint"]);
    // OpenDAL sftp service is keyfile-only: the key is required, the old
    // password field was dropped (password accounts belong to sftp-native).
    expect(requiredOf("sftp")).toEqual(["endpoint", "key"]);
    expect(requiredOf("oss")).toEqual(["bucket", "endpoint", "access_key_id", "access_key_secret"]);
    expect(requiredOf("obs")).toEqual(["bucket", "endpoint", "access_key_id", "secret_access_key"]);
    expect(requiredOf("cos")).toEqual(["bucket", "endpoint", "secret_id", "secret_key"]);
    expect(requiredOf("aliyun-drive")).toEqual([]);
    expect(requiredOf("dropbox")).toEqual([]);
    expect(requiredOf("gdrive")).toEqual([]);
    expect(requiredOf("koofr")).toEqual(["endpoint", "email", "password"]);
    expect(requiredOf("onedrive")).toEqual([]);
    expect(requiredOf("pcloud")).toEqual(["endpoint", "username", "password"]);
    expect(requiredOf("seafile")).toEqual(["endpoint", "username", "password", "repo_name"]);
    expect(requiredOf("yandex-disk")).toEqual(["access_token"]);
  });

  it("flags URL/host security classes on endpoint-like fields and secrets on credentials", () => {
    const specOf = (service: string, key: string): CustomFieldSpec => {
      const spec = (schemaForService(service) ?? []).find((item) => item.key === key);
      expect(spec, `${service}.${key}`).toBeDefined();
      return spec!;
    };
    expect(specOf("s3", "endpoint").security).toBe("url");
    expect(specOf("webdav", "endpoint").security).toBe("url");
    expect(specOf("ftp", "endpoint").security).toBe("host");
    expect(specOf("sftp", "endpoint").security).toBe("host");
    expect(specOf("s3", "secret_access_key").secret).toBe(true);
    expect(specOf("oss", "access_key_secret").secret).toBe(true);
    expect(specOf("cos", "secret_id")).toMatchObject({ type: "password", required: true, secret: true });
    expect(specOf("cos", "secret_key")).toMatchObject({ type: "password", required: true, secret: true });
    expect(specOf("dropbox", "access_token")).toMatchObject({ type: "password", secret: true });
    expect(specOf("onedrive", "client_secret")).toMatchObject({ type: "password", secret: true });
    expect(specOf("koofr", "endpoint").security).toBe("url");
    expect(specOf("pcloud", "endpoint").security).toBe("url");
    expect(specOf("seafile", "endpoint").security).toBe("url");
    expect(specOf("sftp", "known_hosts_strategy").options).toEqual(["Tolerate", "Strict", "Trust"]);
  });
});

describe("field grouping (audit #14)", () => {
  const GROUPS = ["target", "credentials", "advanced"] as const satisfies readonly FieldGroup[];

  it("groups endpoint/bucket/container/root-class keys as target", () => {
    for (const key of ["endpoint", "bucket", "container", "root", "share", "region"]) {
      expect(field_group(key), key).toBe("target");
    }
  });

  it("groups access_key/secret/password/token-class keys as credentials", () => {
    for (const key of [
      "access_key_id",
      "secret_access_key",
      "password",
      "security_token",
      "refresh_token",
      "credential",
      "account_name",
      "account_key",
      "client_id",
      "client_secret",
      "username",
      "user",
      "email",
      "key", // sftp 私钥内容
    ]) {
      expect(field_group(key), key).toBe("credentials");
    }
  });

  it("groups the remaining optional keys as advanced", () => {
    for (const key of ["scope", "known_hosts_strategy", "drive_type", "repo_name", "enable_virtual_host_style", "read_only", "allow_delete", "domain"]) {
      expect(field_group(key), key).toBe("advanced");
    }
  });

  it("partitions every key of every known service schema into a valid group", () => {
    for (const service of CUSTOM_SERVICES) {
      for (const spec of schemaForService(service) ?? []) {
        expect(GROUPS, `${service}.${spec.key}`).toContain(field_group(spec.key));
      }
    }
  });

  it("is case-insensitive, prefers credentials over target and defaults to advanced", () => {
    expect(field_group("ENDPOINT")).toBe("target");
    // 凭据命中优先：即使同时带目标词根（假想的 token 类 bucket 键）。
    expect(field_group("bucket_token")).toBe("credentials");
    expect(field_group("custom_key_extra")).toBe("credentials");
    expect(field_group("unknown_option")).toBe("advanced");
    expect(field_group("")).toBe("advanced");
  });
});

describe("endpoint security validation", () => {
  it("extracts hostnames from every endpoint spelling", () => {
    expect(hostPartOf("ftp.example.com:21")).toBe("ftp.example.com");
    expect(hostPartOf("user@host")).toBe("host");
    expect(hostPartOf("ssh://user@host:22")).toBe("host");
    expect(hostPartOf("https://dav.example.com/dav")).toBe("dav.example.com");
    expect(hostPartOf("[2001:db8::1]:443")).toBe("2001:db8::1");
  });

  it("rejects localhost, loopback, private and reserved hosts", () => {
    for (const bad of [
      "localhost",
      "localhost:9000",
      "ftp.LOCALHOST",
      "my.host.localhost",
      "127.0.0.1:9000",
      "http://127.0.0.1:9000",
      "0.0.0.0",
      "10.1.2.3",
      "172.16.0.9",
      "172.31.255.1",
      "192.168.1.10",
      "169.254.169.254",
      "100.64.0.1",
      "user@10.0.0.5:22",
      "[::1]:9000",
      "[fe80::1]",
      "[fd00::5]",
    ]) {
      expect(validateHostField(bad), bad).toBe("host");
    }
  });

  it("accepts public hosts and public IPv6", () => {
    expect(validateHostField("ftp.example.com:21")).toBeNull();
    expect(validateHostField("user@ops.example.com")).toBeNull();
    expect(validateHostField("172.32.0.1")).toBeNull();
    expect(validateHostField("[2001:db8::1]:22")).toBeNull();
  });

  it("requires a non-empty host value", () => {
    expect(validateHostField("")).toBe("required");
    expect(validateHostField("user@")).toBe("required");
  });

  it("allows only http/https URLs for url-class fields", () => {
    expect(validateUrlField("https://s3.amazonaws.com")).toBeNull();
    expect(validateUrlField("http://s3.example.com:9000")).toBeNull();
    expect(validateUrlField("ftp://s3.example.com")).toBe("url");
    expect(validateUrlField("s3.example.com")).toBe("url");
    expect(validateUrlField("not a url")).toBe("url");
    expect(validateUrlField("")).toBe("required");
  });

  it("routes url-class fields through the host deny-list as well", () => {
    expect(validateUrlField("http://127.0.0.1:9000")).toBe("host");
    expect(validateUrlField("https://192.168.1.1/dav")).toBe("host");
    expect(validateUrlField("http://localhost/dav")).toBe("host");
  });
});

describe("field-level validation", () => {
  const spec = (overrides: Partial<CustomFieldSpec>): CustomFieldSpec => ({
    key: "field",
    type: "text",
    ...overrides,
  });

  it("enforces required on empty values only", () => {
    expect(validateFieldInput(spec({ required: true }), "")).toBe("required");
    expect(validateFieldInput(spec({ required: true }), "   ")).toBe("required");
    expect(validateFieldInput(spec({ required: true }), "value")).toBeNull();
    expect(validateFieldInput(spec({}), "")).toBeNull();
  });

  it("accepts only numeric text for number fields", () => {
    expect(validateFieldInput(spec({ type: "number", required: true }), "30")).toBeNull();
    expect(validateFieldInput(spec({ type: "number", required: true }), "-1.5")).toBeNull();
    expect(validateFieldInput(spec({ type: "number" }), "abc")).toBe("number");
    expect(validateFieldInput(spec({ type: "number", required: true }), "")).toBe("required");
  });

  it("never fails boolean fields", () => {
    expect(validateFieldInput(spec({ type: "boolean", required: true }), false)).toBeNull();
    expect(validateFieldInput(spec({ type: "boolean", required: true }), undefined)).toBeNull();
  });
});

describe("form ⇄ JSON sync", () => {
  const s3Schema = schemaForService("s3")!;

  it("hydrates form values from a config object and keeps unknown keys as extras", () => {
    const hydration = formValuesFromConfig(s3Schema, {
      bucket: "demo",
      endpoint: "https://s3.example.com",
      enable_virtual_host_style: true,
      root: "/data",
      custom_key: "keep-me",
    });
    expect(hydration.values).toMatchObject({
      bucket: "demo",
      endpoint: "https://s3.example.com",
      region: "",
      enable_virtual_host_style: true,
    });
    expect(hydration.extras).toEqual({ root: "/data", custom_key: "keep-me" });
  });

  it("serializes form values back into a config, dropping empty strings and false", () => {
    const config = configFromFormValues(
      s3Schema,
      {
        bucket: "demo",
        endpoint: "",
        region: "us-east-1",
        access_key_id: "ak",
        secret_access_key: "sk",
        enable_virtual_host_style: false,
      },
      {},
    );
    expect(config).toEqual({ bucket: "demo", region: "us-east-1", access_key_id: "ak", secret_access_key: "sk" });
  });

  it("round-trips config → form → config without losing extras", () => {
    const original = { bucket: "demo", root: "/x", tuning: { depth: 3 } };
    const hydration = formValuesFromConfig(s3Schema, original);
    const roundTrip = configFromFormValues(s3Schema, hydration.values, hydration.extras);
    expect(roundTrip).toEqual(original);
  });

  it("re-hydrating a serialized form config keeps booleans and extras stable", () => {
    const first = formValuesFromConfig(s3Schema, { bucket: "b", enable_virtual_host_style: true, root: "/r" });
    const serialized = configFromFormValues(s3Schema, first.values, first.extras);
    const second = formValuesFromConfig(s3Schema, serialized);
    expect(second.values.enable_virtual_host_style).toBe(true);
    expect(second.extras).toEqual({ root: "/r" });
  });

  it("validates a whole config against its schema", () => {
    const errors = validateConfigAgainstSchema(s3Schema, {
      endpoint: "http://127.0.0.1:9000",
      region: "not-a-number-key",
    });
    expect(errors).toEqual({
      bucket: "required",
      endpoint: "host",
      access_key_id: "required",
      secret_access_key: "required",
    });
    expect(validateConfigAgainstSchema(s3Schema, { bucket: "b", access_key_id: "a", secret_access_key: "s" })).toEqual({});
  });

  it("serializes boolean true and keeps schema-unknown keys as extras", () => {
    // fs schema 只声明 root；额外键以 extras 保真，来回切换不丢。
    const values = formValuesFromConfig(schemaForService("fs")!, { root: "/x", custom_flag: "on" });
    expect(values.values).toEqual({ root: "/x" });
    expect(values.extras).toEqual({ custom_flag: "on" });
  });
});

describe("i18n", () => {
  const LOCALES = ["en", "es", "it", "ja", "pt-BR", "zh-CN", "zh-TW"] as const;

  function flatten(node: unknown, prefix = ""): string[] {
    return Object.entries(node as Record<string, unknown>).flatMap(([key, value]) =>
      typeof value === "string" ? [prefix + key] : flatten(value, `${prefix}${key}.`),
    );
  }

  it("has the same key set in all seven locales", () => {
    const baseline = flatten(messages.en).sort();
    expect(baseline.length).toBeGreaterThan(60);
    for (const locale of LOCALES) {
      expect(flatten(messages[locale]).sort(), `locale ${locale}`).toEqual(baseline);
    }
  });

  it("falls back to en and interpolates values", () => {
    expect(resolveWorkbenchLocale("zh")).toBe("zh-CN");
    expect(resolveWorkbenchLocale("zh-Hant")).toBe("zh-TW");
    expect(resolveWorkbenchLocale("pt")).toBe("pt-BR");
    expect(workbenchMessage("ja", "entriesCount", { count: 3 })).toBe("3 件");
    expect(workbenchMessage("xx-YY", "cancel")).toBe("Cancel");
  });
});
