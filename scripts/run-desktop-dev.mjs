import { execFileSync, spawn } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { platform } from "node:os";
import { join } from "node:path";

const cliArgs = new Set(process.argv.slice(2));
const build = cliArgs.has("--build");
const production = cliArgs.has("--production");
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
repairIncompatibleWebRtcCache(cargoTargetDir, buildEnvironment);

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
  if (!signal && code === 0 && build && !production && platform() === "darwin") {
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
  process.exitCode = signal ? 1 : (code ?? 1);
});
