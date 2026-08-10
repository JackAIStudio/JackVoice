import { execFileSync } from "node:child_process";
import { realpathSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { platform } from "node:os";
import { fileURLToPath } from "node:url";

const DEV_PORTS = [1427, 1428];
const projectRoot = realpathSync(resolve(dirname(fileURLToPath(import.meta.url)), ".."));
const isWindows = platform() === "win32";

function run(command, args) {
  try {
    return execFileSync(command, args, {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    }).trim();
  } catch (error) {
    // lsof and PowerShell both use a non-zero status when no matching listener exists.
    if (error.status === 1) {
      return "";
    }
    throw error;
  }
}

function listeningPids(port) {
  const output = isWindows
    ? run("powershell.exe", [
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        `Get-NetTCPConnection -State Listen -LocalPort ${port} -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess -Unique`,
      ])
    : run("lsof", ["-nP", `-tiTCP:${port}`, "-sTCP:LISTEN"]);

  return [...new Set(output.split(/\s+/).filter(Boolean).map(Number).filter(Number.isInteger))];
}

function processInfo(pid) {
  if (isWindows) {
    const output = run("powershell.exe", [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      `Get-CimInstance Win32_Process -Filter \"ProcessId = ${pid}\" | Select-Object CommandLine,ExecutablePath | ConvertTo-Json -Compress`,
    ]);
    const info = output ? JSON.parse(output) : {};
    return {
      command: info.CommandLine || info.ExecutablePath || "（无法读取命令）",
      cwd: "",
    };
  }

  const command = run("ps", ["-p", String(pid), "-o", "command="]) || "（无法读取命令）";
  const cwdOutput = run("lsof", ["-a", "-p", String(pid), "-d", "cwd", "-Fn"]);
  const cwd = cwdOutput
    .split("\n")
    .find((line) => line.startsWith("n"))
    ?.slice(1) || "";
  return { command, cwd };
}

function normalizePath(value) {
  const normalized = value.replaceAll("\\", "/").replace(/\/$/, "");
  return isWindows ? normalized.toLowerCase() : normalized;
}

function belongsToThisProject(info) {
  const command = normalizePath(info.command);
  const cwd = normalizePath(info.cwd);
  const root = normalizePath(projectRoot);
  const runsVite = /(?:^|\/)\.bin\/vite(?:\.cmd)?(?:[\s"]|$)/i.test(command)
    || /(?:^|\/)vite\/bin\/vite\.js(?:[\s"]|$)/i.test(command);
  const usesProjectPath = command.includes(`${root}/`) || cwd === root || cwd.startsWith(`${root}/`);
  return runsVite && usesProjectPath;
}

function isRunning(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

function wait(milliseconds) {
  return new Promise((resolveWait) => setTimeout(resolveWait, milliseconds));
}

async function stopProcess(pid) {
  process.kill(pid, "SIGTERM");
  for (let elapsed = 0; elapsed < 4000; elapsed += 100) {
    if (!isRunning(pid)) {
      return;
    }
    await wait(100);
  }

  console.log(`[JackVoice] 旧进程 ${pid} 未正常退出，正在强制关闭…`);
  process.kill(pid, "SIGKILL");
  for (let elapsed = 0; elapsed < 1000; elapsed += 100) {
    if (!isRunning(pid)) {
      return;
    }
    await wait(100);
  }
  throw new Error(`无法关闭旧进程 ${pid}`);
}

const listeners = new Map();
for (const port of DEV_PORTS) {
  for (const pid of listeningPids(port)) {
    const listener = listeners.get(pid) || { pid, ports: [], info: processInfo(pid) };
    listener.ports.push(port);
    listeners.set(pid, listener);
  }
}

if (listeners.size === 0) {
  console.log(`[JackVoice] 开发端口 ${DEV_PORTS.join("、")} 可用`);
  process.exit(0);
}

const foreignListeners = [...listeners.values()].filter(
  (listener) => !belongsToThisProject(listener.info),
);

if (foreignListeners.length > 0) {
  console.error("[JackVoice] 开发端口被其他程序占用。为避免误杀，已停止启动：");
  for (const listener of foreignListeners) {
    console.error(`  - 端口 ${listener.ports.join("、")}，PID ${listener.pid}`);
    if (listener.info.cwd) {
      console.error(`    目录：${listener.info.cwd}`);
    }
    console.error(`    命令：${listener.info.command}`);
  }
  console.error("请先关闭上面的程序，再重新运行 npm run dev。");
  process.exit(1);
}

for (const listener of listeners.values()) {
  console.log(
    `[JackVoice] 检测到旧的本项目 Vite 进程 ${listener.pid}（端口 ${listener.ports.join("、")}），正在关闭…`,
  );
  await stopProcess(listener.pid);
}

const remainingPorts = DEV_PORTS.filter((port) => listeningPids(port).length > 0);
if (remainingPorts.length > 0) {
  console.error(`[JackVoice] 端口 ${remainingPorts.join("、")} 仍被占用，已停止启动。`);
  process.exit(1);
}

console.log("[JackVoice] 旧开发进程已关闭，继续启动");
