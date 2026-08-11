import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  createDeliveryArtifact,
  createReleaseBuildId,
  deliveryDmgFileName,
  findMountedJackVoiceBuildImages,
  parseMountedDiskImages,
  productionDmgPath,
  resolveReleaseBuildId,
  validateReleaseVersions,
} from "./release-artifacts.mjs";

test("生产构建拒绝 package、Tauri 与 Cargo 版本号漂移", () => {
  assert.equal(
    validateReleaseVersions({ package: "8.12.5", tauri: "8.12.5", cargo: "8.12.5" }),
    "8.12.5",
  );
  assert.throws(
    () => validateReleaseVersions({ package: "8.12.5", tauri: "8.12.4", cargo: "8.12.5" }),
    /版本号不一致/,
  );
});

test("正式构建标识包含毫秒，连续交付不会复用同一文件名", () => {
  assert.equal(
    createReleaseBuildId(new Date("2026-08-11T07:30:45.123Z")),
    "20260811T073045123Z",
  );
  assert.throws(() => resolveReleaseBuildId("../../旧包"), /只能包含/);
});

test("解析 hdiutil 输出并定位当前构建目录中的旧 JackVoice 镜像", () => {
  const output = `
================================================
image-path      : /Users/test/Library/Caches/jackvoice/release-cargo-target/release/bundle/dmg/JackVoice_0.1.0_aarch64.dmg
/dev/disk12\tGUID_partition_scheme
/dev/disk12s1\tUUID\t/Volumes/JackVoice 1
================================================
image-path      : /Users/test/Downloads/JackVoice_0.1.0_aarch64.dmg
/dev/disk13\tGUID_partition_scheme
/dev/disk13s1\tUUID\t/Volumes/JackVoice 2
================================================
image-path      : /Users/test/Library/Caches/JackVoice/release-cargo-target/release/bundle/macos/rw.123.JackVoice_8.12.5_aarch64.dmg
/dev/disk14\tGUID_partition_scheme
/dev/disk14s1\tUUID\t/Volumes/dmg.temp
`;
  const images = parseMountedDiskImages(output);
  assert.deepEqual(images[0], {
    imagePath:
      "/Users/test/Library/Caches/jackvoice/release-cargo-target/release/bundle/dmg/JackVoice_0.1.0_aarch64.dmg",
    device: "/dev/disk12",
    mountPoints: ["/Volumes/JackVoice 1"],
  });
  assert.deepEqual(
    findMountedJackVoiceBuildImages(
      images,
      "/Users/test/Library/Caches/JackVoice/release-cargo-target/release/bundle",
    ).map((image) => image.device),
    ["/dev/disk12", "/dev/disk14"],
  );
});

test("交付文件名同时绑定版本、构建标识和内容哈希", () => {
  assert.equal(
    deliveryDmgFileName(
      "JackVoice_8.12.5_aarch64.dmg",
      "20260811T073045123Z",
      "a".repeat(64),
    ),
    "JackVoice_8.12.5_aarch64_build-20260811T073045123Z_aaaaaaaaaaaaaaaa.dmg",
  );
  assert.equal(
    productionDmgPath("/tmp/target", "8.12.5", "arm64"),
    "/tmp/target/release/bundle/dmg/JackVoice_8.12.5_aarch64.dmg",
  );
});

test("交付产物包含可独立核验的 DMG、SHA-256 和清单", () => {
  const testRoot = mkdtempSync(join(tmpdir(), "jackvoice-release-artifact-"));
  try {
    const sourceDmgPath = join(testRoot, "JackVoice_8.12.5_aarch64.dmg");
    const appExecutablePath = join(testRoot, "jackvoice");
    writeFileSync(sourceDmgPath, "signed dmg fixture");
    writeFileSync(appExecutablePath, "signed app fixture");
    const expectedDmgSha = createHash("sha256").update("signed dmg fixture").digest("hex");

    const artifact = createDeliveryArtifact({
      sourceDmgPath,
      buildId: "20260811T073045123Z",
      version: "8.12.5",
      bundleIdentifier: "com.jackvoice.app",
      teamId: "ABCDEFGHIJ",
      appExecutablePath,
      notarization: {
        status: "accepted",
        stapled: true,
        gatekeeperAssessment: "accepted",
        submissionId: "00000000-0000-0000-0000-000000000000",
      },
      createdAt: new Date("2026-08-11T07:30:45.123Z"),
    });

    assert.equal(artifact.dmgSha256, expectedDmgSha);
    assert.ok(existsSync(artifact.deliveryPath));
    assert.equal(
      readFileSync(artifact.checksumPath, "utf8"),
      `${expectedDmgSha}  ${artifact.deliveryPath.split("/").at(-1)}\n`,
    );
    const manifest = JSON.parse(readFileSync(artifact.manifestPath, "utf8"));
    assert.equal(manifest.version, "8.12.5");
    assert.equal(manifest.buildId, "20260811T073045123Z");
    assert.equal(manifest.dmg.sha256, expectedDmgSha);
    assert.equal(manifest.bundleIdentifier, "com.jackvoice.app");
    assert.equal(manifest.schemaVersion, 2);
    assert.deepEqual(manifest.notarization, {
      status: "accepted",
      stapled: true,
      gatekeeperAssessment: "accepted",
      submissionId: "00000000-0000-0000-0000-000000000000",
    });
  } finally {
    rmSync(testRoot, { recursive: true, force: true });
  }
});

test("没有公证、staple 或 Gatekeeper 证据时拒绝生成交付目录", () => {
  const testRoot = mkdtempSync(join(tmpdir(), "jackvoice-unnotarized-artifact-"));
  try {
    const sourceDmgPath = join(testRoot, "JackVoice_8.12.5_aarch64.dmg");
    const appExecutablePath = join(testRoot, "jackvoice");
    writeFileSync(sourceDmgPath, "signed only dmg fixture");
    writeFileSync(appExecutablePath, "signed app fixture");

    assert.throws(
      () =>
        createDeliveryArtifact({
          sourceDmgPath,
          buildId: "20260811T073045123Z",
          version: "8.12.5",
          bundleIdentifier: "com.jackvoice.app",
          teamId: "ABCDEFGHIJ",
          appExecutablePath,
        }),
      /必须先通过 Apple 公证/,
    );
  } finally {
    rmSync(testRoot, { recursive: true, force: true });
  }
});
