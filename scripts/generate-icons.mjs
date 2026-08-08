import { copyFile, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const brandingRoot = join(projectRoot, "branding", "logo-v4-whisper-tx");
const appSource = join(brandingRoot, "jackvoice-whisper-tx-app.svg");
const macOSSource = join(brandingRoot, "jackvoice-whisper-tx-macos.svg");
const devAppSource = join(brandingRoot, "jackvoice-whisper-tx-dev-app.svg");
const devMacOSSource = join(brandingRoot, "jackvoice-whisper-tx-dev-macos.svg");
const iconOutput = join(projectRoot, "src-tauri", "icons");
const devIconOutput = join(projectRoot, "src-tauri", "icons-dev");
const appPngSizes = [32, 64, 128, 256, 512, 1024];
const markExports = [
  "jackvoice-whisper-tx-mark-dark",
  "jackvoice-whisper-tx-mark-light",
  "jackvoice-whisper-tx-mono",
];
const tauri = join(
  projectRoot,
  "node_modules",
  ".bin",
  process.platform === "win32" ? "tauri.cmd" : "tauri",
);
const temporaryRoot = await mkdtemp(join(tmpdir(), "jackvoice-icons-"));

function generate(source, output, extraArguments = []) {
  execFileSync(tauri, ["icon", source, "--output", output, ...extraArguments], {
    cwd: projectRoot,
    stdio: "inherit",
  });
}

try {
  // The common source stays full-size for Windows, mobile and web resources.
  generate(appSource, iconOutput);

  // Export all canonical PNGs directly through Tauri's SVG renderer so
  // transparent corners never become opaque white pixels.
  const pngOutput = join(temporaryRoot, "png");
  generate(
    appSource,
    pngOutput,
    appPngSizes.flatMap((size) => ["--png", String(size)]),
  );
  for (const size of appPngSizes) {
    await copyFile(
      join(pngOutput, `${size}x${size}.png`),
      join(brandingRoot, `jackvoice-whisper-tx-app-${size}.png`),
    );
  }

  for (const mark of markExports) {
    const markOutput = join(temporaryRoot, mark);
    generate(join(brandingRoot, `${mark}.svg`), markOutput, ["--png", "1024"]);
    await copyFile(
      join(markOutput, "1024x1024.png"),
      join(brandingRoot, `${mark}-1024.png`),
    );
  }

  // macOS needs its own optical safe area. Only replace ICNS here; applying
  // this padding to every platform would make the other app icons too small.
  const macOSOutput = join(temporaryRoot, "macos");
  generate(macOSSource, macOSOutput);
  await copyFile(join(macOSOutput, "icon.icns"), join(iconOutput, "icon.icns"));

  // Development builds keep the production artwork intact and add only a
  // high-contrast DEV badge. A separate icon directory prevents accidental
  // production bundles from picking up development artwork.
  generate(devAppSource, devIconOutput);
  const devMacOSOutput = join(temporaryRoot, "macos-dev");
  generate(devMacOSSource, devMacOSOutput);
  await copyFile(
    join(devMacOSOutput, "icon.icns"),
    join(devIconOutput, "icon.icns"),
  );

  console.log("JackVoice production and development icons generated successfully.");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}
