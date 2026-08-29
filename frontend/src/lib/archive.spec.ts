import { describe, expect, it } from "vitest";
import { archiveKind, isArchivePath } from "./archive";

describe("archive detection", () => {
  it("recognizes zip archives", () => {
    expect(archiveKind("/backup.zip")).toBe("zip");
    expect(archiveKind("/backup.ZIP")).toBe("zip");
  });

  it("recognizes tar family archives", () => {
    expect(archiveKind("/site.tar.gz")).toBe("tar");
    expect(archiveKind("/site.TGZ")).toBe("tar");
    expect(archiveKind("/dump.tar")).toBe("tar");
  });

  it("rejects non-archive extensions and bare names", () => {
    expect(archiveKind("/photo.png")).toBeNull();
    expect(archiveKind("/archive.zipx")).toBeNull();
    expect(archiveKind("/tar")).toBeNull();
    expect(archiveKind("/dir.gz")).toBeNull();
    expect(archiveKind("")).toBeNull();
  });

  it("isArchivePath mirrors archiveKind", () => {
    expect(isArchivePath("/a.tgz")).toBe(true);
    expect(isArchivePath("/a.tar.gz")).toBe(true);
    expect(isArchivePath("/a.zip")).toBe(true);
    expect(isArchivePath("/a.tar")).toBe(true);
    expect(isArchivePath("/a.txt")).toBe(false);
  });
});
