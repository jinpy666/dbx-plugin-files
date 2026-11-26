// S3 user-metadata edit model (issue #100): pure draft → wire-map translation
// for `files/setMetadata`. Mirrors the backend validation so the dialog can
// reject bad input before a round trip, while the backend stays authoritative
// (rc errors / capability errors still surface through showError).
//
// Wire vocabulary (matches `files/info` detail keys):
//   content-type / cache-control / …  → HTTP headers
//   x-amz-meta-<key>                  → S3 user metadata
// The dialog edits content-type plus bare x-amz-meta-* suffixes; rows are
// re-prefixed here.

export interface MetadataDraftRow {
  key: string;
  value: string;
}

export interface MetadataDetailLike {
  mimeType?: string;
  metadata?: { key: string; value: string }[];
}

export interface MetadataDraft {
  contentType: string;
  rows: MetadataDraftRow[];
}

export type MetadataBuildReason =
  | "noChanges"
  | "emptyValue"
  | "invalidKey"
  | "duplicateKey"
  | "invalidValue";

export type MetadataBuild =
  | { ok: true; metadata: Record<string, string> }
  | { ok: false; reason: MetadataBuildReason; key?: string };

export const USER_METADATA_PREFIX = "x-amz-meta-";

/** Editable draft seeded from a `files/info` detail payload. */
export function draftFromDetail(detail: MetadataDetailLike | null | undefined): MetadataDraft {
  const rows: MetadataDraftRow[] = [];
  for (const pair of detail?.metadata ?? []) {
    const key = pair.key.toLowerCase();
    if (!key.startsWith(USER_METADATA_PREFIX)) continue;
    rows.push({ key: key.slice(USER_METADATA_PREFIX.length), value: pair.value });
  }
  rows.sort((left, right) => left.key.localeCompare(right.key));
  return { contentType: contentTypeOf(detail), rows };
}

/** Current content type: stat MimeType first, then the metadata entry. */
export function contentTypeOf(detail: MetadataDetailLike | null | undefined): string {
  if (detail?.mimeType) return detail.mimeType;
  const pair = (detail?.metadata ?? []).find((item) => item.key.toLowerCase() === "content-type");
  return pair?.value ?? "";
}

/** Wire map the draft would currently send (used for the dirty check too). */
function wireMapOf(draft: MetadataDraft): Record<string, string> {
  const map: Record<string, string> = {};
  for (const row of draft.rows) {
    const key = row.key.trim().toLowerCase();
    if (!key) continue;
    map[`${USER_METADATA_PREFIX}${key}`] = row.value.trim();
  }
  return map;
}

/**
 * Validates the draft and translates it to the `files/setMetadata` wire map.
 * Rejects no-op saves, empty values (removal is unsupported by the rewrite
 * path), bad key charsets, duplicates and control characters.
 */
export function draftToWireMap(initial: MetadataDraft, draft: MetadataDraft): MetadataBuild {
  const contentType = draft.contentType.trim();
  if (contentType && hasControlChars(contentType)) {
    return { ok: false, reason: "invalidValue", key: "content-type" };
  }

  const seen = new Set<string>();
  const metadata: Record<string, string> = {};
  for (const row of draft.rows) {
    const rawKey = row.key.trim().toLowerCase();
    const value = row.value.trim();
    if (!rawKey) {
      // Blank trailing row: ignore only when the value is blank too.
      if (value) return { ok: false, reason: "invalidKey" };
      continue;
    }
    const key = rawKey.startsWith(USER_METADATA_PREFIX)
      ? rawKey.slice(USER_METADATA_PREFIX.length)
      : rawKey;
    if (!/^[a-z0-9._-]+$/.test(key)) {
      return { ok: false, reason: "invalidKey", key: row.key };
    }
    if (!value) {
      return { ok: false, reason: "emptyValue", key: `${USER_METADATA_PREFIX}${key}` };
    }
    if (hasControlChars(value)) {
      return { ok: false, reason: "invalidValue", key: `${USER_METADATA_PREFIX}${key}` };
    }
    if (seen.has(key)) {
      return { ok: false, reason: "duplicateKey", key: `${USER_METADATA_PREFIX}${key}` };
    }
    seen.add(key);
    metadata[`${USER_METADATA_PREFIX}${key}`] = value;
  }

  if (contentType) metadata["content-type"] = contentType;

  const initialWire: Record<string, string> = {
    ...wireMapOf(initial),
    ...(initial.contentType.trim() ? { "content-type": initial.contentType.trim() } : {}),
  };
  const dirty = Object.entries(metadata).some(([key, value]) => initialWire[key] !== value);
  if (!dirty) return { ok: false, reason: "noChanges" };

  return { ok: true, metadata };
}

function hasControlChars(value: string): boolean {
  // eslint-disable-next-line no-control-regex
  return /[\u0000-\u001f\u007f]/.test(value);
}
