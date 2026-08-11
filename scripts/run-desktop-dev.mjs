import { execFileSync, spawn } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { homedir, platform } from "node:os";
import { join } from "node:path";

import {
  discoverProductionSigningIdentity,
  productionAppPath,
  productionSigningConfig,
  verifyProductionApp,
} from "./macos-signing.mjs";
import {
  RELEASE_BUILD_ID_ENV,
  createDeliveryArtifact,
  detachMountedJackVoiceBuildImages,
  productionBundleDirectory,
  productionDmgPath,
  resolveReleaseBuildId,
  validateReleaseVersions,
} from "./release-artifacts.mjs";

const cliArgs = new Set(process.argv.slice(2));
const build = cliArgs.has("--build");
const production = cliArgs.has("--production");
const packageMetadata = JSON.parse(
  readFileSync(join(process.cwd(), "package.json"), "utf8"),
);
const tauriMetadata = JSON.parse(
  readFileSync(join(process.cwd(), "src-tauri", "tauri.conf.json"), "utf8"),
);
const cargoVersion = readFileSync(join(process.cwd(), "src-tauri", "Cargo.toml"), "utf8")
  .match(/^version\s*=\s*"([^"]+)"$/m)?.[1];
const releaseVersion = validateReleaseVersions({
  package: packageMetadata.version,
  tauri: tauriMetadata.version,
  cargo: cargoVersion || "missing",
});
const releaseBuildId = build && production && platform() === "darwin"
  ? resolveReleaseBuildId(process.env[RELEASE_BUILD_ID_ENV])
  : null;
const npmCommand = platform() === "win32" ? "npm.cmd" : "npm";
const args = [
  "run",
  "tauri",
  "--",
  build ? "build" : "dev",
];

if (!production) {
  args.push("--config", "src-tauri/tauri.dev.conf.json");
}

if (build && !production && platform() === "darwin") {
  args.push("--bundles", "app", "--no-sign");
}

function resolveCargoTargetDir() {
  if (process.env.CARGO_TARGET_DIR?.trim()) {
    return process.env.CARGO_TARGET_DIR.trim();
  }
  // A signed .app cannot contain FinderInfo/resource-fork metadata. iCloud
  // File Provider may attach it immediately when the repository lives in
  // Desktop/Documents, before Tauri reaches codesign. Keep production bundle
  // artifacts in a local, non-synced cache so signing is deterministic.
  if (build && production && platform() === "darwin") {
    return join(homedir(), "Library", "Caches", "JackVoice", "release-cargo-target");
  }
  const metadata = execFileSync(
    "cargo",
    ["metadata", "--format-version", "1", "--no-deps", "--manifest-path", join("src-tauri", "Cargo.toml")],
    { cwd: process.cwd(), encoding: "utf8" },
  );
  return JSON.parse(metadata).target_directory;
}

function resolveBuildEnvironment(cargoTargetDir) {
  const env = {
    ...process.env,
    CARGO_TARGET_DIR: cargoTargetDir,
  };

  // Keep both stages of webrtc-audio-processing-sys on its bundled Abseil.
  // Otherwise Meson can cache a Homebrew installation while the Rust build
  // script chooses the bundled copy (or vice versa), producing missing headers
  // and non-portable Homebrew dylib references in packaged applications.
  if (platform() === "darwin") {
    env.PKG_CONFIG_LIBDIR = "";
    env.PKG_CONFIG_PATH = "";
    console.log("[JackVoice] WebRTC Abseil: 项目内置版本");
  }

  return env;
}

function findIncompatibleWebRtcCaches(cargoTargetDir) {
  if (platform() !== "darwin" || !existsSync(cargoTargetDir)) {
    return [];
  }

  const incompatible = [];
  for (const profile of readdirSync(cargoTargetDir, { withFileTypes: true })) {
    if (!profile.isDirectory()) {
      continue;
    }

    const buildDir = join(cargoTargetDir, profile.name, "build");
    if (!existsSync(buildDir)) {
      continue;
    }

    for (const entry of readdirSync(buildDir, { withFileTypes: true })) {
      if (!entry.isDirectory() || !entry.name.startsWith("webrtc-audio-processing-sys-")) {
        continue;
      }

      const outDir = join(buildDir, entry.name, "out");
      const mesonCache = join(
        outDir,
        "webrtc-audio-processing-build",
        "meson-private",
        "coredata.dat",
      );
      const mesonDependencies = join(
        outDir,
        "webrtc-audio-processing-build",
        "meson-info",
        "intro-dependencies.json",
      );
      const bundledHeader = join(outDir, "include", "absl", "base", "config.h");
      if (!existsSync(mesonCache)) {
        continue;
      }

      let usesExternalAbseil = false;
      if (existsSync(mesonDependencies)) {
        try {
          const dependencies = JSON.parse(readFileSync(mesonDependencies, "utf8"));
          usesExternalAbseil = dependencies.some(
            (dependency) => dependency.name === "absl_base" && dependency.type !== "internal",
          );
        } catch {
          // A damaged Meson introspection file is itself an unsafe cache state.
          usesExternalAbseil = true;
        }
      }

      if (usesExternalAbseil || !existsSync(bundledHeader)) {
        incompatible.push(outDir);
      }
    }
  }

  return incompatible;
}

function repairIncompatibleWebRtcCache(cargoTargetDir, env) {
  const incompatible = findIncompatibleWebRtcCaches(cargoTargetDir);
  if (incompatible.length === 0) {
    return;
  }

  console.log(
    "[JackVoice] 检测到 WebRTC/Abseil 构建缓存与当前环境不一致，正在自动重建该依赖…",
  );
  execFileSync(
    "cargo",
    [
      "clean",
      "--manifest-path",
      join("src-tauri", "Cargo.toml"),
      "--target-dir",
      cargoTargetDir,
      "-p",
      "webrtc-audio-processing-sys",
    ],
    { cwd: process.cwd(), env, stdio: "inherit" },
  );
}

const cargoTargetDir = resolveCargoTargetDir();
console.log(`[JackVoice] Cargo 构建缓存: ${cargoTargetDir}`);
const buildEnvironment = resolveBuildEnvironment(cargoTargetDir);
buildEnvironment.VITE_JACKVOICE_VERSION = releaseVersion;
buildEnvironment.VITE_JACKVOICE_BUILD_ID = releaseBuildId || "development";
if (releaseBuildId) buildEnvironment[RELEASE_BUILD_ID_ENV] = releaseBuildId;
repairIncompatibleWebRtcCache(cargoTargetDir, buildEnvironment);

let productionSigning = null;
if (build && production && platform() === "darwin") {
  try {
    const detachedImages = detachMountedJackVoiceBuildImages(
      productionBundleDirectory(cargoTargetDir),
    );
    if (detachedImages.length > 0) {
      console.log(
        `[JackVoice] 构建前已弹出 ${detachedImages.length} 个旧产物挂载，防止 macOS 复用旧镜像。`,
      );
    }
    productionSigning = discoverProductionSigningIdentity(buildEnvironment);
    const signingConfig = productionSigningConfig(productionSigning);
    if (signingConfig) args.push("--config", signingConfig);
    console.log(
      `[JackVoice] 生产签名预检通过：Team ID ${productionSigning.teamId ?? "由 CI 证书提供"}`,
    );
  } catch (error) {
    console.error(`[JackVoice] 生产构建已阻止：${error.message}`);
    process.exit(1);
  }
}

const child = spawn(npmCommand, args, {
  cwd: process.cwd(),
  env: buildEnvironment,
  stdio: "inherit",
});

child.on("error", (error) => {
  console.error(`无法启动 JackVoice Dev：${error.message}`);
  process.exitCode = 1;
});

child.on("exit", (code, signal) => {
  if (signal || code !== 0) {
    process.exitCode = 1;
    return;
  }

  if (build && !production && platform() === "darwin") {
    const appPath = join(
      cargoTargetDir,
      "release",
      "bundle",
      "macos",
      "JackVoice Dev.app",
    );
    try {
      // Repeated local bundles can retain Finder metadata or quarantine xattrs
      // from an older .app directory. codesign may appear to succeed while a
      // strict verification still rejects those attributes, so normalize the
      // freshly bundled development app before applying its ad-hoc signature.
      execFileSync("xattr", ["-cr", appPath], { stdio: "inherit" });
      // Tauri's --no-sign avoids using the production Developer ID. Apply a
      // local ad-hoc signature so macOS still validates the development app's
      // resource envelope and keeps it distinct from the release identity.
      execFileSync(
        "codesign",
        ["--force", "--deep", "--sign", "-", appPath],
        { stdio: "inherit" },
      );
      execFileSync(
        "codesign",
        ["--verify", "--deep", "--strict", "--verbose=2", appPath],
        { stdio: "inherit" },
      );
    } catch (error) {
      console.error(`无法为 JackVoice Dev 添加本地签名：${error.message}`);
      process.exitCode = 1;
      return;
    }
  }

  if (build && production && platform() === "darwin") {
    try {
      const appPath = productionAppPath(cargoTargetDir);
      const verified = verifyProductionApp(appPath, productionSigning?.teamId);
      console.log(
        `[JackVoice] 生产签名验收通过：${verified.bundleIdentifier} / Team ID ${verified.teamId}`,
      );
      const delivery = createDeliveryArtifact({
        sourceDmgPath: productionDmgPath(cargoTargetDir, releaseVersion),
        buildId: releaseBuildId,
        version: releaseVersion,
        bundleIdentifier: verified.bundleIdentifier,
        teamId: verified.teamId,
        appExecutablePath: join(appPath, "Contents", "MacOS", "jackvoice"),
      });
      console.log(`[JackVoice] 本次构建标识：${delivery.buildId}`);
      console.log(`[JackVoice] 唯一交付包：${delivery.deliveryPath}`);
      console.log(`[JackVoice] SHA-256：${delivery.dmgSha256}`);
      console.log(`[JackVoice] 交付清单：${delivery.manifestPath}`);
    } catch (error) {
      console.error(`[JackVoice] 生产构建已拒绝：${error.message}`);
      process.exitCode = 1;
      return;
    }
  }

  process.exitCode = 0;
});
