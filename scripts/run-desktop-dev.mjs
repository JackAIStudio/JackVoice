import { execFileSync, spawn } from "node:child_process";
import { existsSync } from "node:fs";
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

  // The bundled WebRTC audio processor uses Meson. On macOS, Meson may find a
  // Homebrew Abseil installation through CMake while the Rust build script
  // fails to discover the same installation through pkg-config. In that case
  // the final Rust link cannot find libabsl_strings even though Meson compiled
  // against it. Keep both build stages on the same library search path.
  if (platform() === "darwin") {
    try {
      const abseilPrefix = execFileSync("brew", ["--prefix", "abseil"], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      }).trim();
      const abseilLibDir = join(abseilPrefix, "lib");
      if (existsSync(join(abseilLibDir, "libabsl_strings.dylib"))) {
        env.LIBRARY_PATH = [abseilLibDir, env.LIBRARY_PATH]
          .filter(Boolean)
          .join(":");
        console.log(`[JackVoice] Homebrew Abseil: ${abseilLibDir}`);
      }
    } catch {
      // No Homebrew Abseil installation: Meson uses the bundled copy.
    }
  }

  return env;
}

const cargoTargetDir = resolveCargoTargetDir();
console.log(`[JackVoice] Cargo 构建缓存: ${cargoTargetDir}`);

const child = spawn(npmCommand, args, {
  cwd: process.cwd(),
  env: resolveBuildEnvironment(cargoTargetDir),
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
