// @vitest-environment node
// files/setMetadata 草稿模型（issue #100）：detail → 可编辑草稿、草稿 → wire
// map 的校验（空值/非法键/重复/无改动）与脏检查。
import { describe, expect, it } from "vitest";
import {
  contentTypeOf,
  draftFromDetail,
  draftToWireMap,
  USER_METADATA_PREFIX,
  type MetadataDraft,
} from "./metadataEdit";

const s3Detail = {
  mimeType: "text/plain",
  metadata: [
    { key: "cache-control", value: "max-age=60" },
    { key: "content-type", value: "text/plain" },
    { key: "tier", value: "STANDARD" },
    { key: "x-amz-meta-owner", value: "alice" },
    { key: "x-amz-meta-Env", value: "prod" },
  ],
};

function draft(contentType: string, rows: [string, string][]): MetadataDraft {
  return { contentType, rows: rows.map(([key, value]) => ({ key, value })) };
}

describe("metadataEdit draft model", () => {
  it("seeds the draft from mimeType and only x-amz-meta-* rows", () => {
    const seeded = draftFromDetail(s3Detail);
    expect(seeded.contentType).toBe("text/plain");
    expect(seeded.rows).toEqual([
      { key: "env", value: "prod" },
      { key: "owner", value: "alice" },
    ]);
    expect(draftFromDetail(null)).toEqual({ contentType: "", rows: [] });
  });

  it("falls back to the metadata content-type when mimeType is absent", () => {
    expect(contentTypeOf({ metadata: [{ key: "Content-Type", value: "image/png" }] })).toBe(
      "image/png",
    );
    expect(contentTypeOf({ mimeType: "application/json" })).toBe("application/json");
  });

  it("translates the draft into the wire map with prefixes restored", () => {
    const initial = draftFromDetail(s3Detail);
    const result = draftToWireMap(
      initial,
      draft("application/json", [
        ["owner", "bob"],
        ["x-amz-meta-env", "staging"],
      ]),
    );
    expect(result).toEqual({
      ok: true,
      metadata: {
        "content-type": "application/json",
        "x-amz-meta-owner": "bob",
        "x-amz-meta-env": "staging",
      },
    });
  });

  it("accepts a new key while keeping untouched keys out of the payload", () => {
    const initial = draftFromDetail(s3Detail);
    const result = draftToWireMap(
      initial,
      draft("text/plain", [
        ["owner", "alice"],
        ["env", "prod"],
        ["team", "storage"],
      ]),
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.metadata["x-amz-meta-team"]).toBe("storage");
      // `cache-control` was not edited and is preserved by Metadata:true.
      expect(result.metadata["cache-control"]).toBeUndefined();
    }
  });

  it("rejects a no-op save", () => {
    const initial = draftFromDetail(s3Detail);
    expect(draftToWireMap(initial, draftFromDetail(s3Detail))).toEqual({
      ok: false,
      reason: "noChanges",
    });
  });

  it("rejects empty values (removal is unsupported)", () => {
    const initial = draftFromDetail(s3Detail);
    const result = draftToWireMap(
      initial,
      draft("text/plain", [
        ["owner", "  "],
        ["env", "prod"],
      ]),
    );
    expect(result).toEqual({
      ok: false,
      reason: "emptyValue",
      key: "x-amz-meta-owner",
    });
  });

  it("rejects invalid keys and duplicated keys", () => {
    const initial = draftFromDetail(s3Detail);
    const invalid = draftToWireMap(
      initial,
      draft("text/plain", [["bad key", "x"]]),
    );
    expect(invalid).toEqual({ ok: false, reason: "invalidKey", key: "bad key" });

    const duplicate = draftToWireMap(
      initial,
      draft("text/plain", [
        ["owner", "bob"],
        ["x-amz-meta-owner", "carol"],
      ]),
    );
    expect(duplicate).toEqual({
      ok: false,
      reason: "duplicateKey",
      key: "x-amz-meta-owner",
    });
  });

  it("rejects control characters in values and content type", () => {
    const initial = draftFromDetail(s3Detail);
    expect(
      draftToWireMap(initial, draft("text/plain", [["owner", "bob\r\nX: 1"]])),
    ).toEqual({
      ok: false,
      reason: "invalidValue",
      key: "x-amz-meta-owner",
    });
    expect(
      draftToWireMap(initial, draft("text/plain\r\nX-Evil: 1", [["owner", "bob"]])),
    ).toEqual({ ok: false, reason: "invalidValue", key: "content-type" });
  });

  it("skips a fully blank trailing row but not a blank key with a value", () => {
    const initial = draftFromDetail(s3Detail);
    const withBlank = draftToWireMap(
      initial,
      draft("text/plain", [
        ["owner", "bob"],
        ["", ""],
      ]),
    );
    expect(withBlank.ok).toBe(true);

    const valueOnly = draftToWireMap(initial, draft("text/plain", [["", "orphan"]]));
    expect(valueOnly).toEqual({ ok: false, reason: "invalidKey" });
  });

  it("keeps the prefix constant in sync with the protocol doc", () => {
    expect(USER_METADATA_PREFIX).toBe("x-amz-meta-");
  });
});
