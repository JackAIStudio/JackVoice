import { mkdir, mkdtemp, readFile, copyFile, rm, writeFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const branding = join(root, "branding", "logo-v4-whisper-tx");
const temporary = await mkdtemp(join(tmpdir(), "jackvoice-dock-icons-"));
const tauri = join(root, "node_modules", ".bin", process.platform === "win32" ? "tauri.cmd" : "tauri");

try {
  for (const [variant, source] of [
    ["production", "jackvoice-whisper-tx-macos.svg"],
    ["development", "jackvoice-whisper-tx-dev-macos.svg"],
  ]) {
    const original = await readFile(join(branding, source), "utf8");
    const output = join(root, "src-tauri", "dock-icons", variant);
    await mkdir(output, { recursive: true });
    const states = [];
    for (let frame = 0; frame < 8; frame += 1) {
      // 这是「收到声音」的明确指示，不再用微小幅度表示精确音量。
      const heights = [0, 2, 4, 1, 3].map((offset) =>
        28 + (1 + Math.sin((frame + offset) * Math.PI / 4)) * 138,
      );
      const bars = heights.map((height, index) => {
        const x = 352 + index * 80;
        return `<path d="M${x} ${(544 - height / 2).toFixed(2)} V${(544 + height / 2).toFixed(2)}"/>`;
      }).join("\n");
      states.push([`recording-${frame}`, bars]);
    }
    states.push(
      ["quiet", '<path d="M344 544 H680"/>'],
      ["silent", '<path d="M512 410 V560"/><path d="M512 632 V634"/>'],
      ["disconnected", `<path d="M452 474 V414 A60 60 0 0 1 572 414 V520 M452 520 A60 60 0 0 0 540 573 M404 514 V530 A108 108 0 0 0 596 598 M512 638 V688 M452 688 H572 M368 388 L656 676"/>`],
    );
    for (const [name, paths] of states) {
      const wave = `<g fill="none" stroke="currentColor" color="#17171A"
        stroke-width="44" stroke-linecap="round" stroke-linejoin="round">${paths}</g>
        <rect x="12" y="12" width="1000" height="1000" rx="216"
          fill="none" stroke="#F7F7F4" stroke-width="12" stroke-opacity="0.5"/>`;
      if ((original.match(/<path\b[\s\S]*?\/>/g) ?? []).length !== 1) {
        throw new Error(`${source} 应当只包含一条声波路径，请检查图标生成逻辑。`);
      }
      const svg = original.replace(/<path\b[\s\S]*?\/>/, wave);
      const input = join(temporary, `${variant}-${name}.svg`);
      const rendered = join(temporary, `${variant}-${name}`);
      await writeFile(input, svg);
      execFileSync(tauri, ["icon", input, "--output", rendered, "--png", "512"], {
        cwd: root,
        stdio: "pipe",
      });
      await copyFile(join(rendered, "512x512.png"), join(output, `${name}.png`));
    }
  }
  console.log("Dock 录音波形图标已生成（正式版 / 开发版）。");
} finally {
  await rm(temporary, { recursive: true, force: true });
}
