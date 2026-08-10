import { execFileSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { homedir, platform } from "node:os";
import { dirname, join } from "node:path";
import process from "node:process";

const variants = {
  dev: {
    identifier: "com.jackvoice.app.dev",
    productName: "JackVoice Dev",
    buildScript: "build:desktop:dev",
  },
  production: {
    identifier: "com.jackvoice.app",
    productName: "JackVoice",
    buildScript: "build:desktop",
  },
};

function usage() {
  console.log(`用法：
  node scripts/reset-first-run.mjs <dev|production> [--quit] [--build] [--open] [--forget-credential] [--dry-run]

选项：
  --quit               先退出正在运行的目标应用（QA 编排使用）
  --build              重置前构建对应的桌面应用
  --open               重置后打开已构建的 macOS .app
  --forget-credential  同时移走开发版 App Key（会先备份；正式版不支持）
  --dry-run            只显示将执行的操作

默认保留共享历史、录音、词库、识别设置和 App Key。`);
}

const args = process.argv.slice(2);
const target = args.find((arg) => !arg.startsWith("--"));
const unknownOptions = args.filter(
  (arg) =>
    arg.startsWith("--") &&
    !["--quit", "--build", "--open", "--forget-credential", "--dry-run"].includes(arg),
);

if (!target || !variants[target] || unknownOptions.length > 0) {
  usage();
  process.exitCode = 2;
} else {
  try {
    resetFirstRun(target, {
      quit: args.includes("--quit"),
      build: args.includes("--build"),
      open: args.includes("--open"),
      forgetCredential: args.includes("--forget-credential"),
      dryRun: args.includes("--dry-run"),
    });
  } catch (error) {
    console.error(`[first-run] ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}

function dataRoot() {
  if (platform() === "darwin") {
    return join(homedir(), "Library", "Application Support");
  }
  if (platform() === "win32") {
    const appData = process.env.APPDATA?.trim();
    if (!appData) throw new Error("找不到 Windows APPDATA 目录。");
    return appData;
  }
  return process.env.XDG_DATA_HOME?.trim() || join(homedir(), ".local", "share");
}

function timestamp() {
  return new Date().toISOString().replaceAll(":", "-").replaceAll(".", "-");
}

function runningProcessList(identifier) {
  if (platform() !== "darwin") return [];
  try {
    const output = execFileSync("/usr/bin/pgrep", ["-x", "jackvoice"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
    return output
      .split("\n")
      .map((value) => value.trim())
      .filter(Boolean)
      .map((pid) => {
        const command = execFileSync("/bin/ps", ["-p", pid, "-o", "command="], {
          encoding: "utf8",
          stdio: ["ignore", "pipe", "ignore"],
        }).trim();
        return { pid, command };
      })
      .filter(({ command }) => {
        if (identifier.endsWith(".dev")) {
          return (
            command.includes("target/debug/jackvoice") ||
            command.includes("target/release/jackvoice") ||
            command.includes("JackVoice Dev.app/Contents/MacOS/jackvoice")
          );
        }
        return command.includes("JackVoice.app/Contents/MacOS/jackvoice");
      });
  } catch {
    return [];
  }
}

function formatRunningProcesses(processes) {
  return processes.map(({ pid, command }) => `${pid} ${command}`).join("\n");
}

function quitRunningProcesses(identifier) {
  const running = runningProcessList(identifier);
  for (const entry of running) {
    const pid = Number(entry.pid);
    if (!Number.isSafeInteger(pid) || pid <= 1) continue;
    if (entry.command.includes("target/debug/jackvoice")) {
      const pgid = Number(
        execFileSync("/bin/ps", ["-p", entry.pid, "-o", "pgid="], {
          encoding: "utf8",
          stdio: ["ignore", "pipe", "ignore"],
        }).trim(),
      );
      if (Number.isSafeInteger(pgid) && pgid > 1) process.kill(-pgid, "SIGINT");
    } else {
      process.kill(pid, "SIGTERM");
    }
  }

  for (let attempt = 0; attempt < 30; attempt += 1) {
    if (runningProcessList(identifier).length === 0) return;
    execFileSync("/bin/sleep", ["0.1"]);
  }
  const remaining = runningProcessList(identifier);
  throw new Error(`无法退出仍在运行的应用：\n${formatRunningProcesses(remaining)}`);
}

function cargoTargetDirectory() {
  const raw = execFileSync(
    "cargo",
    ["metadata", "--format-version", "1", "--no-deps", "--manifest-path", join("src-tauri", "Cargo.toml")],
    { cwd: process.cwd(), encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] },
  );
  return JSON.parse(raw).target_directory;
}

function appBundlePath(variant) {
  return join(cargoTargetDirectory(), "release", "bundle", "macos", `${variant.productName}.app`);
}

function buildDesktopApp(variant) {
  const npmCommand = platform() === "win32" ? "npm.cmd" : "npm";
  execFileSync(npmCommand, ["run", variant.buildScript], {
    cwd: process.cwd(),
    stdio: "inherit",
  });
}

function writeJsonAtomically(path, value) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${process.pid}.tmp`;
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  renameSync(temporary, path);
  if (platform() !== "win32") chmodSync(path, 0o600);
}

function writePrivateText(path, value) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  writeFileSync(path, value, { mode: 0o600 });
  if (platform() !== "win32") chmodSync(path, 0o600);
}

function resetFirstRun(target, options) {
  const variant = variants[target];
  if (options.forgetCredential && target !== "dev") {
    throw new Error(
      "--forget-credential 只支持开发版。正式版 App Key 位于系统凭据库，请在应用设置中主动移除。",
    );
  }
  if (options.forgetCredential && process.env.JACKVOICE_VOLC_API_KEY?.trim()) {
    throw new Error(
      "当前 shell 设置了 JACKVOICE_VOLC_API_KEY；请先 unset 该变量，否则应用仍会读取开发 App Key。",
    );
  }

  let processes = runningProcessList(variant.identifier);
  if (processes.length > 0 && options.quit && !options.dryRun) {
    console.log(`[first-run] 正在退出 ${variant.productName}…`);
    quitRunningProcesses(variant.identifier);
    processes = [];
  }
  if (processes.length > 0 && !options.dryRun) {
    throw new Error(
      `${variant.productName} 仍在运行，请先完全退出后重试：\n${formatRunningProcesses(processes)}`,
    );
  }

  if (options.build) {
    console.log(`[first-run] 构建：npm run ${variant.buildScript}`);
    if (!options.dryRun) buildDesktopApp(variant);
  }

  const variantDirectory = join(dataRoot(), variant.identifier);
  const settingsPath = join(variantDirectory, "variant-settings.json");
  const credentialPath = join(variantDirectory, "dev-credentials.json");
  const credentialMigrationMarkerPath = join(
    variantDirectory,
    ".dev-credential-migration-complete",
  );
  const backupDirectory = join(variantDirectory, ".first-run-backups", timestamp());

  let settings = {};
  if (existsSync(settingsPath)) {
    settings = JSON.parse(readFileSync(settingsPath, "utf8"));
  }
  settings.onboardingCompleted = false;

  console.log(`[first-run] 目标：${variant.productName} (${variant.identifier})`);
  console.log(`[first-run] Onboarding：${settingsPath}`);
  if (platform() === "darwin") {
    console.log(`[first-run] macOS 权限：tccutil reset All ${variant.identifier}`);
  }
  if (options.forgetCredential) {
    console.log(`[first-run] 开发版 App Key：移至 ${backupDirectory}`);
  }
  console.log("[first-run] 共享历史、录音、词库和识别设置：保留");

  if (options.dryRun) {
    console.log("[first-run] dry-run 完成，未修改任何内容。");
    return;
  }

  mkdirSync(backupDirectory, { recursive: true, mode: 0o700 });
  if (platform() !== "win32") chmodSync(backupDirectory, 0o700);
  if (existsSync(settingsPath)) {
    copyFileSync(settingsPath, join(backupDirectory, "variant-settings.json"));
    if (platform() !== "win32") {
      chmodSync(join(backupDirectory, "variant-settings.json"), 0o600);
    }
  }
  writeJsonAtomically(settingsPath, settings);

  if (options.forgetCredential && existsSync(credentialPath)) {
    renameSync(credentialPath, join(backupDirectory, "dev-credentials.json"));
    if (platform() !== "win32") {
      chmodSync(join(backupDirectory, "dev-credentials.json"), 0o600);
    }
  }
  if (options.forgetCredential) {
    // Prevent the app's one-time compatibility migration from restoring an
    // old development Keychain entry during a clean first-run QA pass.
    writePrivateText(credentialMigrationMarkerPath, "version=1\n");
  }

  if (platform() === "darwin") {
    execFileSync("/usr/bin/tccutil", ["reset", "All", variant.identifier], { stdio: "inherit" });
  } else {
    console.log("[first-run] 当前平台没有可由脚本统一重置的 TCC 权限；已重置应用引导状态。");
  }

  if (options.open) {
    if (platform() !== "darwin") {
      throw new Error("--open 目前只支持 macOS .app。");
    }
    const appPath = appBundlePath(variant);
    if (!existsSync(appPath)) {
      throw new Error(`找不到 ${appPath}，请先执行：npm run ${variant.buildScript}`);
    }
    execFileSync("/usr/bin/open", [appPath]);
    console.log(`[first-run] 已打开：${appPath}`);
  }

  const backups = readdirSync(join(variantDirectory, ".first-run-backups"));
  if (backups.length > 0) {
    console.log(`[first-run] 本次备份：${backupDirectory}`);
  }
  if (target === "dev" && !options.open) {
    console.log(
      "[first-run] 权限回归请打开构建后的 JackVoice Dev.app；不要运行 npm run dev，裸调试二进制使用另一套临时 TCC 身份。",
    );
  }
  console.log("[first-run] 重置完成。请从欢迎页依次复测麦克风、辅助功能、隐私确认和服务连接。");
}
