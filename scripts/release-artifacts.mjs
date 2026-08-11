import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  constants,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";

export const RELEASE_BUILD_ID_ENV = "JACKVOICE_BUILD_ID";

export function validateReleaseVersions(versions) {
  const entries = Object.entries(versions);
  const invalid = entries.filter(([, version]) => !/^\d+\.\d+\.\d+$/.test(version));
  if (invalid.length > 0) {
    throw new Error(
      `正式版版本号必须使用 x.y.z：${invalid.map(([source, version]) => `${source}=${version}`).join("，")}`,
    );
  }
  const uniqueVersions = new Set(entries.map(([, version]) => version));
  if (uniqueVersions.size !== 1) {
    throw new Error(
      `正式版版本号不一致：${entries.map(([source, version]) => `${source}=${version}`).join("，")}`,
    );
  }
  return entries[0][1];
}

export function createReleaseBuildId(date = new Date()) {
  if (Number.isNaN(date.getTime())) throw new Error("无法为正式版生成有效构建标识。");
  return date.toISOString().replaceAll("-", "").replaceAll(":", "").replace(".", "");
}

export function resolveReleaseBuildId(explicitBuildId, date = new Date()) {
  const buildId = explicitBuildId?.trim() || createReleaseBuildId(date);
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(buildId)) {
    throw new Error(
      `${RELEASE_BUILD_ID_ENV} 只能包含字母、数字、点、下划线和连字符，且最长 64 位。`,
    );
  }
  return buildId;
}

export function parseMountedDiskImages(output) {
  const images = [];
  for (const block of output.split(/^={10,}\s*$/m)) {
    const imagePath = block.match(/^image-path\s+:\s+(.+)$/m)?.[1]?.trim();
    if (!imagePath) continue;

    let device = null;
    const mountPoints = [];
    for (const line of block.split("\n")) {
      const columns = line.split(/\t+/).map((column) => column.trim()).filter(Boolean);
      if (!columns[0]?.startsWith("/dev/disk")) continue;
      if (!device && /^\/dev\/disk\d+$/.test(columns[0])) device = columns[0];
      const candidateMountPoint = columns.at(-1);
      if (columns.length >= 3 && candidateMountPoint?.startsWith("/")) {
        mountPoints.push(candidateMountPoint);
      }
    }
    images.push({ imagePath, device, mountPoints });
  }
  return images;
}

function normalizeMacPath(value) {
  return resolve(value).normalize("NFC").toLowerCase();
}

export function findMountedJackVoiceBuildImages(images, bundleDirectory) {
  const normalizedBundleDirectory = `${normalizeMacPath(bundleDirectory)}${sep}`;
  return images.filter(({ imagePath }) => {
    const normalizedImagePath = normalizeMacPath(imagePath);
    if (!normalizedImagePath.startsWith(normalizedBundleDirectory)) return false;
    const fileName = basename(imagePath).toLowerCase();
    return (
      fileName.endsWith(".dmg") &&
      (fileName.startsWith("jackvoice_") ||
        (fileName.startsWith("rw.") && fileName.includes(".jackvoice_")))
    );
  });
}

export function detachMountedJackVoiceBuildImages(bundleDirectory) {
  const output = execFileSync("/usr/bin/hdiutil", ["info"], { encoding: "utf8" });
  const mountedImages = findMountedJackVoiceBuildImages(
    parseMountedDiskImages(output),
    bundleDirectory,
  );
  for (const image of mountedImages) {
    const detachTarget = image.device || image.mountPoints[0];
    if (!detachTarget) {
      throw new Error(`无法确定旧镜像的挂载设备：${image.imagePath}`);
    }
    execFileSync("/usr/bin/hdiutil", ["detach", detachTarget], { stdio: "inherit" });
  }
  return mountedImages;
}

export function sha256File(filePath) {
  return createHash("sha256").update(readFileSync(filePath)).digest("hex");
}

export function deliveryDmgFileName(sourceFileName, buildId, sha256) {
  const normalizedBuildId = resolveReleaseBuildId(buildId);
  if (!/^[a-f0-9]{64}$/i.test(sha256)) throw new Error("DMG SHA-256 格式无效。");
  if (!sourceFileName.endsWith(".dmg")) throw new Error("正式交付源文件必须是 DMG。");
  return `${sourceFileName.slice(0, -4)}_build-${normalizedBuildId}_${sha256.slice(0, 16)}.dmg`;
}

export function createDeliveryArtifact({
  sourceDmgPath,
  buildId,
  version,
  bundleIdentifier,
  teamId,
  appExecutablePath,
  notarization,
  createdAt = new Date(),
}) {
  if (!existsSync(sourceDmgPath)) throw new Error(`找不到正式 DMG：${sourceDmgPath}`);
  if (!existsSync(appExecutablePath)) throw new Error(`找不到正式应用可执行文件：${appExecutablePath}`);
  if (
    notarization?.status !== "accepted" ||
    notarization?.stapled !== true ||
    notarization?.gatekeeperAssessment !== "accepted" ||
    typeof notarization?.submissionId !== "string" ||
    !notarization.submissionId.trim()
  ) {
    throw new Error("正式交付 DMG 必须先通过 Apple 公证、票据 stapling 和 Gatekeeper 验收。");
  }

  const normalizedBuildId = resolveReleaseBuildId(buildId);
  const dmgSha256 = sha256File(sourceDmgPath);
  const appSha256 = sha256File(appExecutablePath);
  const deliveryDirectory = join(dirname(sourceDmgPath), "delivery");
  const fileName = deliveryDmgFileName(
    basename(sourceDmgPath),
    normalizedBuildId,
    dmgSha256,
  );
  const deliveryPath = join(deliveryDirectory, fileName);
  mkdirSync(deliveryDirectory, { recursive: true });

  if (existsSync(deliveryPath)) {
    if (sha256File(deliveryPath) !== dmgSha256) {
      throw new Error(`交付文件名发生内容冲突，已拒绝覆盖：${deliveryPath}`);
    }
  } else {
    copyFileSync(sourceDmgPath, deliveryPath, constants.COPYFILE_EXCL);
  }
  if (sha256File(deliveryPath) !== dmgSha256) {
    throw new Error("交付 DMG 复制后的 SHA-256 与源文件不一致。");
  }

  const bytes = statSync(deliveryPath).size;
  const checksumPath = `${deliveryPath}.sha256`;
  writeFileSync(checksumPath, `${dmgSha256}  ${fileName}\n`, { encoding: "utf8" });

  const manifestPath = `${deliveryPath}.json`;
  const manifest = {
    schemaVersion: 2,
    product: "JackVoice",
    version,
    buildId: normalizedBuildId,
    createdAt: createdAt.toISOString(),
    bundleIdentifier,
    teamId,
    notarization,
    dmg: { fileName, bytes, sha256: dmgSha256 },
    appExecutable: { sha256: appSha256 },
  };
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, { encoding: "utf8" });

  return {
    buildId: normalizedBuildId,
    deliveryPath,
    checksumPath,
    manifestPath,
    dmgSha256,
    appSha256,
  };
}

export function productionBundleDirectory(cargoTargetDirectory) {
  return join(cargoTargetDirectory, "release", "bundle");
}

export function productionDmgPath(cargoTargetDirectory, version, architecture = process.arch) {
  const tauriArchitecture = { arm64: "aarch64", x64: "x64" }[architecture];
  if (!tauriArchitecture) throw new Error(`暂不支持为 ${architecture} 解析 macOS DMG 文件名。`);
  return join(
    productionBundleDirectory(cargoTargetDirectory),
    "dmg",
    `JackVoice_${version}_${tauriArchitecture}.dmg`,
  );
}
