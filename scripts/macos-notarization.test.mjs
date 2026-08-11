import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";

import {
  loadLocalNotarizationConfig,
  mergeLocalNotarizationConfig,
  notarizeDmg,
  parseNotarytoolSubmission,
  stapleAndVerifyNotarizedDmg,
  validateNotarizationCredentials,
  verifyNotarizedDmg,
  withoutNotarizationCredentials,
} from "./macos-notarization.mjs";

const teamId = "ABCDEFGHIJ";

test("桌面构建入口可以完整解析签名与公证模块", () => {
  const result = spawnSync(process.execPath, ["scripts/run-desktop-dev.mjs", "--notarize"], {
    cwd: process.cwd(),
    encoding: "utf8",
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Apple 公证只能用于 macOS 正式桌面构建/);
  assert.doesNotMatch(result.stderr, /SyntaxError|does not provide an export/);
});

test("本机项目配置只补充缺失的公证变量，显式环境变量优先", () => {
  assert.deepEqual(
    mergeLocalNotarizationConfig(
      { APPLE_API_KEY: "EXPLICIT_KEY" },
      {
        JACKVOICE_APPLE_TEAM_ID: teamId,
        APPLE_API_ISSUER: "issuer-id",
        APPLE_API_KEY: "LOCAL_KEY",
        APPLE_API_KEY_PATH: "/tmp/AuthKey_LOCAL_KEY.p8",
      },
    ),
    {
      JACKVOICE_APPLE_TEAM_ID: teamId,
      APPLE_API_ISSUER: "issuer-id",
      APPLE_API_KEY: "EXPLICIT_KEY",
      APPLE_API_KEY_PATH: "/tmp/AuthKey_LOCAL_KEY.p8",
    },
  );
});

test("release 命令可读取项目内被 Git 忽略的本机公证配置", () => {
  const loaded = loadLocalNotarizationConfig(
    { PATH: "/usr/bin" },
    {
      currentDirectory: "/project",
      fileExists: () => true,
      readFile: () =>
        JSON.stringify({
          JACKVOICE_APPLE_TEAM_ID: teamId,
          APPLE_API_ISSUER: "issuer-id",
          APPLE_API_KEY: "KEY123",
          APPLE_API_KEY_PATH: "/tmp/AuthKey_KEY123.p8",
        }),
    },
  );
  assert.equal(loaded.configPath, "/project/.jackvoice-release.local");
  assert.equal(loaded.environment.JACKVOICE_APPLE_TEAM_ID, teamId);
  assert.equal(loaded.environment.APPLE_API_KEY, "KEY123");
  assert.throws(
    () => mergeLocalNotarizationConfig({}, { APPLE_API_KEY: "" }),
    /必须是非空字符串/,
  );
});

test("正式发布接受完整的 App Store Connect API Key 凭据", () => {
  assert.deepEqual(
    validateNotarizationCredentials(
      {
        JACKVOICE_APPLE_TEAM_ID: teamId,
        APPLE_API_ISSUER: "issuer-id",
        APPLE_API_KEY: "KEY123",
        APPLE_API_KEY_PATH: "/tmp/AuthKey_KEY123.p8",
      },
      () => true,
    ),
    { method: "app-store-connect-api", expectedTeamId: teamId },
  );
});

test("正式发布接受完整且团队一致的 Apple ID 凭据", () => {
  assert.deepEqual(
    validateNotarizationCredentials({
      JACKVOICE_APPLE_TEAM_ID: teamId,
      APPLE_ID: "release@example.com",
      APPLE_PASSWORD: "@env:APPLE_APP_PASSWORD",
      APPLE_TEAM_ID: teamId,
    }),
    { method: "apple-id", expectedTeamId: teamId },
  );
});

test("正式发布拒绝缺失、混用或团队不一致的公证凭据", () => {
  assert.throws(
    () => validateNotarizationCredentials({ JACKVOICE_APPLE_TEAM_ID: teamId }),
    /缺少 Apple 公证凭据/,
  );
  assert.throws(
    () =>
      validateNotarizationCredentials({
        JACKVOICE_APPLE_TEAM_ID: teamId,
        APPLE_API_KEY: "KEY123",
      }),
    /缺少 APPLE_API_ISSUER、APPLE_API_KEY_PATH/,
  );
  assert.throws(
    () =>
      validateNotarizationCredentials({
        JACKVOICE_APPLE_TEAM_ID: teamId,
        APPLE_API_ISSUER: "issuer-id",
        APPLE_API_KEY: "KEY123",
        APPLE_API_KEY_PATH: "/tmp/AuthKey_KEY123.p8",
        APPLE_ID: "release@example.com",
      }),
    /不能同时设置/,
  );
  assert.throws(
    () =>
      validateNotarizationCredentials({
        JACKVOICE_APPLE_TEAM_ID: teamId,
        APPLE_ID: "release@example.com",
        APPLE_PASSWORD: "app-password",
        APPLE_TEAM_ID: "KLMNOPQRST",
      }),
    /不一致/,
  );
});

test("普通 Developer ID 构建会移除所有公证凭据", () => {
  const sanitized = withoutNotarizationCredentials({
    PATH: "/usr/bin",
    APPLE_ID: "release@example.com",
    APPLE_PASSWORD: "secret",
    APPLE_TEAM_ID: teamId,
    APPLE_API_KEY: "KEY123",
    APPLE_API_ISSUER: "issuer-id",
    APPLE_API_KEY_PATH: "/tmp/key.p8",
    JACKVOICE_APPLE_TEAM_ID: teamId,
  });
  assert.deepEqual(sanitized, {
    PATH: "/usr/bin",
    JACKVOICE_APPLE_TEAM_ID: teamId,
  });
});

test("公证验收同时检查 stapled ticket 与 Gatekeeper", () => {
  const commands = [];
  assert.deepEqual(
    verifyNotarizedDmg("/tmp/JackVoice.dmg", {
      fileExists: () => true,
      runCommand(command, args) {
        commands.push([command, args]);
        return "accepted";
      },
    }),
    {
      status: "accepted",
      stapled: true,
      gatekeeperAssessment: "accepted",
    },
  );
  assert.deepEqual(commands, [
    ["/usr/bin/xcrun", ["stapler", "validate", "/tmp/JackVoice.dmg"]],
    [
      "/usr/sbin/spctl",
      [
        "--assess",
        "--type",
        "open",
        "--context",
        "context:primary-signature",
        "--verbose=4",
        "/tmp/JackVoice.dmg",
      ],
    ],
  ]);
});

test("解析 notarytool Accepted 结果并拒绝其他状态", () => {
  assert.deepEqual(
    parseNotarytoolSubmission(JSON.stringify({ id: "submission-id", status: "Accepted" })),
    { submissionId: "submission-id" },
  );
  assert.throws(
    () =>
      parseNotarytoolSubmission(
        JSON.stringify({ id: "submission-id", status: "Invalid", message: "bad signature" }),
      ),
    /Invalid.*bad signature/,
  );
  assert.throws(() => parseNotarytoolSubmission("not json"), /无法解析/);
});

test("App Store Connect API Key 用于提交最终 DMG 公证", () => {
  const commands = [];
  const submission = notarizeDmg(
    "/tmp/JackVoice.dmg",
    { method: "app-store-connect-api", expectedTeamId: teamId },
    {
      APPLE_API_KEY_PATH: "/tmp/AuthKey_KEY123.p8",
      APPLE_API_KEY: "KEY123",
      APPLE_API_ISSUER: "issuer-id",
    },
    {
      fileExists: () => true,
      runPrivateCommand(command, args, operation) {
        commands.push([command, args, operation]);
        return JSON.stringify({ id: "submission-id", status: "Accepted" });
      },
    },
  );
  assert.deepEqual(submission, { submissionId: "submission-id" });
  assert.deepEqual(commands, [
    [
      "/usr/bin/xcrun",
      [
        "notarytool",
        "submit",
        "/tmp/JackVoice.dmg",
        "--key",
        "/tmp/AuthKey_KEY123.p8",
        "--key-id",
        "KEY123",
        "--issuer",
        "issuer-id",
        "--wait",
        "--output-format",
        "json",
      ],
      "Apple DMG 公证提交",
    ],
  ]);
});

test("最终 DMG 在 Accepted 后执行 staple、validate 与 Gatekeeper 验收", () => {
  const commands = [];
  assert.deepEqual(
    stapleAndVerifyNotarizedDmg(
      "/tmp/JackVoice.dmg",
      { submissionId: "submission-id" },
      {
        fileExists: () => true,
        runCommand(command, args) {
          commands.push([command, args]);
          return "accepted";
        },
      },
    ),
    {
      status: "accepted",
      stapled: true,
      gatekeeperAssessment: "accepted",
      submissionId: "submission-id",
    },
  );
  assert.deepEqual(commands.map(([, args]) => args.slice(0, 2)), [
    ["stapler", "staple"],
    ["stapler", "validate"],
    ["--assess", "--type"],
  ]);
});
