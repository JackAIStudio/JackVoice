import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";

export const PRODUCTION_BUNDLE_ID = "com.jackvoice.app";
export const PRODUCTION_APP_NAME = "JackVoice.app";
export const SIGNING_IDENTITY_ENV = "JACKVOICE_MACOS_SIGNING_IDENTITY";

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

export function parseDeveloperIdIdentity(value) {
  const identity = value?.trim() ?? "";
  const match = identity.match(/^Developer ID Application: .+ \(([A-Z0-9]{10})\)$/);
  if (!match) return null;
  return { identity, teamId: match[1] };
}

export function parseDeveloperIdIdentities(output) {
  const identities = [];
  for (const line of output.split("\n")) {
    const quotedIdentity = line.match(/^\s*\d+\)\s+[0-9A-F]+\s+"(.+)"\s*$/)?.[1];
    const parsed = parseDeveloperIdIdentity(quotedIdentity);
    if (parsed) identities.push(parsed);
  }
  return identities;
}

export function selectProductionSigningIdentity({
  availableIdentities,
  explicitIdentity,
  certificateProvided = false,
  expectedTeamId,
}) {
  const explicit = explicitIdentity?.trim();
  const requestedTeamId = expectedTeamId?.trim();
  if (requestedTeamId && !/^[A-Z0-9]{10}$/.test(requestedTeamId)) {
    throw new Error("JACKVOICE_APPLE_TEAM_ID 必须是 10 位 Apple Team ID。");
  }

  if (explicit) {
    const parsed = parseDeveloperIdIdentity(explicit);
    if (!parsed) {
      throw new Error(
        `${SIGNING_IDENTITY_ENV} 必须是完整的 Developer ID Application 身份，例如 ` +
          "Developer ID Application: Example (ABCDEFGHIJ)。",
      );
    }
    if (requestedTeamId && requestedTeamId !== parsed.teamId) {
      throw new Error(`${SIGNING_IDENTITY_ENV} 与 JACKVOICE_APPLE_TEAM_ID 不属于同一团队。`);
    }
    if (
      !certificateProvided &&
      !availableIdentities.some((candidate) => candidate.identity === parsed.identity)
    ) {
      throw new Error(`${SIGNING_IDENTITY_ENV} 指定的证书不在当前钥匙串中。`);
    }
    return { ...parsed, source: "explicit" };
  }

  if (availableIdentities.length === 1) {
    const selected = availableIdentities[0];
    if (requestedTeamId && requestedTeamId !== selected.teamId) {
      throw new Error("钥匙串中唯一的 Developer ID Application 与 JACKVOICE_APPLE_TEAM_ID 不匹配。");
    }
    return { ...selected, source: "keychain" };
  }

  if (availableIdentities.length > 1) {
    throw new Error(
      `检测到 ${availableIdentities.length} 个 Developer ID Application 身份，请通过 ` +
        `${SIGNING_IDENTITY_ENV} 明确指定生产签名。`,
    );
  }

  if (certificateProvided) {
    return { identity: null, teamId: requestedTeamId || null, source: "ci-certificate" };
  }

  throw new Error(
    "找不到 Developer ID Application 证书。生产版禁止使用临时签名；" +
      `请安装发布证书，或在 CI 中提供 APPLE_CERTIFICATE，并按需设置 ${SIGNING_IDENTITY_ENV}。`,
  );
}

export function discoverProductionSigningIdentity(env = process.env) {
  const identityOutput = commandOutput("/usr/bin/security", [
    "find-identity",
    "-v",
    "-p",
    "codesigning",
  ]);
  return selectProductionSigningIdentity({
    availableIdentities: parseDeveloperIdIdentities(identityOutput),
    explicitIdentity: env[SIGNING_IDENTITY_ENV],
    certificateProvided: Boolean(env.APPLE_CERTIFICATE?.trim()),
    expectedTeamId: env.JACKVOICE_APPLE_TEAM_ID,
  });
}

export const DEV_SIGNING_IDENTITY_ENV = "JACKVOICE_DEV_SIGNING_IDENTITY";

/** `security find-identity` 输出里所有带引号的身份名。 */
export function parseCodesigningIdentityNames(output) {
  const names = [];
  for (const line of output.split("\n")) {
    const quoted = line.match(/^\s*\d+\)\s+[0-9A-F]+\s+"(.+)"\s*$/)?.[1]?.trim();
    if (quoted && !names.includes(quoted)) names.push(quoted);
  }
  return names;
}

/**
 * 开发版 App 的签名身份。
 *
 * 临时签名（ad-hoc）的 Designated Requirement 只绑 CDHash，每次重新打包都会变，
 * macOS 就把它当成没见过的新应用，辅助功能、麦克风要全部重新授权。换成钥匙串里
 * 的固定证书后，DR 绑的是「Bundle ID + 证书」，授权一次以后一直有效。
 *
 * 优先 Apple Development（本机开发证书），其次 Developer ID Application；多张时
 * 按名字排序取第一张，保证每次选中同一张。`JACKVOICE_DEV_SIGNING_IDENTITY` 可以
 * 指定某张证书，设成 `-` 则强制临时签名。没有证书时退回临时签名，不挡构建。
 */
export function selectDevelopmentSigningIdentity({ availableNames, explicitIdentity }) {
  const explicit = explicitIdentity?.trim();
  if (explicit === "-") return { identity: null, source: "explicit-adhoc" };
  if (explicit) {
    if (!availableNames.includes(explicit)) {
      throw new Error(`${DEV_SIGNING_IDENTITY_ENV} 指定的证书不在当前钥匙串中：${explicit}`);
    }
    return { identity: explicit, source: "explicit" };
  }

  for (const prefix of ["Apple Development: ", "Developer ID Application: "]) {
    const matches = availableNames.filter((name) => name.startsWith(prefix)).sort();
    if (matches.length > 0) return { identity: matches[0], source: "keychain" };
  }
  return { identity: null, source: "none" };
}

export function discoverDevelopmentSigningIdentity(env = process.env) {
  let names = [];
  try {
    names = parseCodesigningIdentityNames(
      commandOutput("/usr/bin/security", ["find-identity", "-v", "-p", "codesigning"]),
    );
  } catch {
    // 读不了钥匙串时按「没有证书」处理，开发构建照常用临时签名。
  }
  return selectDevelopmentSigningIdentity({
    availableNames: names,
    explicitIdentity: env[DEV_SIGNING_IDENTITY_ENV],
  });
}

/**
 * 给开发版 App 签名的 codesign 参数。不开 hardened runtime：开发版不公证，
 * 开了反而要补齐 entitlements 才能用麦克风。也不打时间戳，免得构建依赖网络。
 */
export function developmentCodesignArgs(identity, appPath) {
  if (!identity) return ["--force", "--deep", "--sign", "-", appPath];
  return ["--force", "--deep", "--timestamp=none", "--sign", identity, appPath];
}

/** DR 只绑 CDHash 时，每次重新打包都要重新授权。 */
export function designatedRequirementIsStable(designatedRequirement) {
  return !/^# designated => cdhash\b/m.test(designatedRequirement);
}

export function productionSigningConfig(signing) {
  if (!signing.identity) return null;
  return JSON.stringify({
    bundle: {
      macOS: {
        hardenedRuntime: true,
        signingIdentity: signing.identity,
      },
    },
  });
}

export function parseCodesignDetails(output) {
  const value = (key) => output.match(new RegExp(`^${key}=(.+)$`, "m"))?.[1]?.trim() ?? null;
  return {
    identifier: value("Identifier"),
    teamId: value("TeamIdentifier"),
    signature: value("Signature"),
    authorities: [...output.matchAll(/^Authority=(.+)$/gm)].map((match) => match[1].trim()),
  };
}

export function validateProductionSignatureEvidence({
  bundleIdentifier,
  codesignDetails,
  designatedRequirement,
  hasCodeResources,
  expectedBundleIdentifier = PRODUCTION_BUNDLE_ID,
  expectedTeamId,
}) {
  const errors = [];
  if (bundleIdentifier !== expectedBundleIdentifier) {
    errors.push(`Info.plist Bundle ID 为 ${bundleIdentifier || "空"}`);
  }
  if (codesignDetails.identifier !== expectedBundleIdentifier) {
    errors.push(`代码签名 Identifier 为 ${codesignDetails.identifier || "空"}`);
  }
  if (!codesignDetails.teamId || codesignDetails.teamId === "not set") {
    errors.push("代码签名没有 Team ID");
  } else if (expectedTeamId && codesignDetails.teamId !== expectedTeamId) {
    errors.push(`代码签名 Team ID 为 ${codesignDetails.teamId}，预期 ${expectedTeamId}`);
  }
  if (codesignDetails.signature?.toLowerCase() === "adhoc") {
    errors.push("代码签名仍是 ad-hoc 临时签名");
  }
  if (!codesignDetails.authorities.some((authority) => authority.startsWith("Developer ID Application:"))) {
    errors.push("签名链不包含 Developer ID Application");
  }
  if (!hasCodeResources) {
    errors.push("应用包缺少 _CodeSignature/CodeResources");
  }
  if (!designatedRequirement.includes(`identifier \"${expectedBundleIdentifier}\"`)) {
    errors.push("Designated Requirement 未绑定生产 Bundle ID");
  }
  if (/^# designated => cdhash\b/m.test(designatedRequirement)) {
    errors.push("Designated Requirement 只绑定易变的 CDHash");
  }
  if (errors.length > 0) {
    throw new Error(`生产签名验收失败：\n- ${errors.join("\n- ")}`);
  }
}

export function productionAppPath(cargoTargetDirectory) {
  return join(cargoTargetDirectory, "release", "bundle", "macos", PRODUCTION_APP_NAME);
}

export function verifyProductionApp(appPath, expectedTeamId) {
  if (!existsSync(appPath)) throw new Error(`找不到生产应用包：${appPath}`);

  commandOutput("/usr/bin/codesign", ["--verify", "--deep", "--strict", "--verbose=2", appPath]);
  const bundleIdentifier = commandOutput("/usr/libexec/PlistBuddy", [
    "-c",
    "Print :CFBundleIdentifier",
    join(appPath, "Contents", "Info.plist"),
  ]);
  const codesignDetails = parseCodesignDetails(
    commandOutput("/usr/bin/codesign", ["-dvvv", appPath]),
  );
  const designatedRequirement = commandOutput("/usr/bin/codesign", ["-d", "-r-", appPath]);
  const hasCodeResources = existsSync(
    join(appPath, "Contents", "_CodeSignature", "CodeResources"),
  );

  validateProductionSignatureEvidence({
    bundleIdentifier,
    codesignDetails,
    designatedRequirement,
    hasCodeResources,
    expectedTeamId,
  });
  return { bundleIdentifier, teamId: codesignDetails.teamId };
}
