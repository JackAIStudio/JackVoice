import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";

const APPLE_ID_NOTARIZATION_ENV = ["APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"];
const API_KEY_NOTARIZATION_ENV = [
  "APPLE_API_ISSUER",
  "APPLE_API_KEY",
  "APPLE_API_KEY_PATH",
];

export const NOTARIZATION_ENV_NAMES = [
  ...APPLE_ID_NOTARIZATION_ENV,
  ...API_KEY_NOTARIZATION_ENV,
  "API_PRIVATE_KEYS_DIR",
  "APPLE_PROVIDER_SHORT_NAME",
];

function present(env, name) {
  return Boolean(env[name]?.trim());
}

function missingNames(env, names) {
  return names.filter((name) => !present(env, name));
}

export function validateNotarizationCredentials(env = process.env, fileExists = existsSync) {
  const expectedTeamId = env.JACKVOICE_APPLE_TEAM_ID?.trim();
  if (!/^[A-Z0-9]{10}$/.test(expectedTeamId ?? "")) {
    throw new Error("正式发布公证必须设置 10 位 JACKVOICE_APPLE_TEAM_ID。");
  }

  const hasAppleIdCredential = APPLE_ID_NOTARIZATION_ENV.some((name) => present(env, name));
  const hasApiKeyCredential = API_KEY_NOTARIZATION_ENV.some((name) => present(env, name));
  if (hasAppleIdCredential && hasApiKeyCredential) {
    throw new Error("Apple ID 与 App Store Connect API Key 公证凭据不能同时设置。");
  }

  if (hasApiKeyCredential) {
    const missing = missingNames(env, API_KEY_NOTARIZATION_ENV);
    if (missing.length > 0) {
      throw new Error(`App Store Connect API Key 公证凭据不完整：缺少 ${missing.join("、")}。`);
    }
    const keyPath = env.APPLE_API_KEY_PATH.trim();
    if (!fileExists(keyPath)) {
      throw new Error(`找不到 App Store Connect API 私钥：${keyPath}`);
    }
    return { method: "app-store-connect-api", expectedTeamId };
  }

  if (hasAppleIdCredential) {
    const missing = missingNames(env, APPLE_ID_NOTARIZATION_ENV);
    if (missing.length > 0) {
      throw new Error(`Apple ID 公证凭据不完整：缺少 ${missing.join("、")}。`);
    }
    if (env.APPLE_TEAM_ID.trim() !== expectedTeamId) {
      throw new Error("APPLE_TEAM_ID 与 JACKVOICE_APPLE_TEAM_ID 不一致。");
    }
    return { method: "apple-id", expectedTeamId };
  }

  throw new Error(
    "正式发布缺少 Apple 公证凭据；请配置 App Store Connect API Key，或 APPLE_ID、APPLE_PASSWORD 和 APPLE_TEAM_ID。",
  );
}

export function withoutNotarizationCredentials(env) {
  const sanitized = { ...env };
  for (const name of NOTARIZATION_ENV_NAMES) delete sanitized[name];
  return sanitized;
}

function commandOutput(command, args) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`.trim();
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} 执行失败：${output || `退出码 ${result.status}`}`);
  }
  return output;
}

function privateCommandOutput(command, args, operation) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const stdout = result.stdout?.trim() ?? "";
  const stderr = result.stderr?.trim() ?? "";
  const output = `${stdout}\n${stderr}`.trim();
  if (result.error) throw result.error;
  if (result.status !== 0) {
    // Do not include args here: Apple ID authentication can place an app-specific
    // password on the notarytool command line.
    throw new Error(`${operation}失败：${output || `退出码 ${result.status}`}`);
  }
  return stdout || stderr;
}

export function parseNotarytoolSubmission(output) {
  let submission;
  try {
    submission = JSON.parse(output);
  } catch {
    throw new Error("Apple 公证返回了无法解析的结果。");
  }
  if (
    submission.status !== "Accepted" ||
    typeof submission.id !== "string" ||
    !submission.id.trim()
  ) {
    throw new Error(
      `Apple 公证未通过：${submission.status || "未知状态"}` +
        `${submission.message ? `（${submission.message}）` : ""}`,
    );
  }
  return { submissionId: submission.id.trim() };
}

export function notarizeDmg(
  dmgPath,
  credentials,
  env = process.env,
  { fileExists = existsSync, runPrivateCommand = privateCommandOutput } = {},
) {
  if (!fileExists(dmgPath)) throw new Error(`找不到待提交公证的 DMG：${dmgPath}`);

  const args = ["notarytool", "submit", dmgPath];
  if (credentials.method === "app-store-connect-api") {
    args.push(
      "--key",
      env.APPLE_API_KEY_PATH,
      "--key-id",
      env.APPLE_API_KEY,
      "--issuer",
      env.APPLE_API_ISSUER,
    );
  } else if (credentials.method === "apple-id") {
    args.push(
      "--apple-id",
      env.APPLE_ID,
      "--password",
      env.APPLE_PASSWORD,
      "--team-id",
      env.APPLE_TEAM_ID,
    );
  } else {
    throw new Error(`不支持的 Apple 公证认证方式：${credentials.method || "空"}`);
  }
  args.push("--wait", "--output-format", "json");

  const output = runPrivateCommand("/usr/bin/xcrun", args, "Apple DMG 公证提交");
  return parseNotarytoolSubmission(output);
}

export function verifyNotarizedDmg(
  dmgPath,
  { fileExists = existsSync, runCommand = commandOutput } = {},
) {
  if (!fileExists(dmgPath)) throw new Error(`找不到待验收的公证 DMG：${dmgPath}`);

  runCommand("/usr/bin/xcrun", ["stapler", "validate", dmgPath]);
  runCommand("/usr/sbin/spctl", [
    "--assess",
    "--type",
    "open",
    "--context",
    "context:primary-signature",
    "--verbose=4",
    dmgPath,
  ]);

  return {
    status: "accepted",
    stapled: true,
    gatekeeperAssessment: "accepted",
  };
}

export function stapleAndVerifyNotarizedDmg(
  dmgPath,
  submission,
  { fileExists = existsSync, runCommand = commandOutput } = {},
) {
  runCommand("/usr/bin/xcrun", ["stapler", "staple", dmgPath]);
  return {
    ...verifyNotarizedDmg(dmgPath, { fileExists, runCommand }),
    submissionId: submission.submissionId,
  };
}
