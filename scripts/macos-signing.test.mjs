import assert from "node:assert/strict";
import test from "node:test";

import {
  designatedRequirementIsStable,
  developmentCodesignArgs,
  parseCodesignDetails,
  parseCodesigningIdentityNames,
  parseDeveloperIdIdentities,
  selectDevelopmentSigningIdentity,
  selectProductionSigningIdentity,
  validateProductionSignatureEvidence,
} from "./macos-signing.mjs";

const developerIdentity = "Developer ID Application: Example Studio (ABCDEFGHIJ)";
const secondDeveloperIdentity = "Developer ID Application: Other Studio (KLMNOPQRST)";

test("只解析 Developer ID Application 身份", () => {
  const output = `
  1) AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA "Apple Development: Example (ABCDEFGHIJ)"
  2) BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB "${developerIdentity}"
     2 valid identities found
  `;
  assert.deepEqual(parseDeveloperIdIdentities(output), [
    { identity: developerIdentity, teamId: "ABCDEFGHIJ" },
  ]);
});

test("唯一的 Developer ID 自动成为生产签名", () => {
  assert.deepEqual(
    selectProductionSigningIdentity({
      availableIdentities: [{ identity: developerIdentity, teamId: "ABCDEFGHIJ" }],
    }),
    { identity: developerIdentity, teamId: "ABCDEFGHIJ", source: "keychain" },
  );
});

test("多个 Developer ID 时必须显式选择", () => {
  assert.throws(
    () =>
      selectProductionSigningIdentity({
        availableIdentities: [
          { identity: developerIdentity, teamId: "ABCDEFGHIJ" },
          { identity: secondDeveloperIdentity, teamId: "KLMNOPQRST" },
        ],
      }),
    /明确指定生产签名/,
  );
});

test("没有本机身份时允许 Tauri 导入 CI 证书", () => {
  assert.deepEqual(
    selectProductionSigningIdentity({
      availableIdentities: [],
      certificateProvided: true,
      expectedTeamId: "ABCDEFGHIJ",
    }),
    { identity: null, teamId: "ABCDEFGHIJ", source: "ci-certificate" },
  );
});

test("显式签名身份必须存在或由 CI 导入", () => {
  assert.throws(
    () =>
      selectProductionSigningIdentity({
        availableIdentities: [],
        explicitIdentity: developerIdentity,
      }),
    /不在当前钥匙串/,
  );
});

test("解析正式签名详情", () => {
  assert.deepEqual(
    parseCodesignDetails(`
Identifier=com.jackvoice.app
Authority=${developerIdentity}
Authority=Developer ID Certification Authority
TeamIdentifier=ABCDEFGHIJ
    `),
    {
      identifier: "com.jackvoice.app",
      teamId: "ABCDEFGHIJ",
      signature: null,
      authorities: [developerIdentity, "Developer ID Certification Authority"],
    },
  );
});

test("稳定的 Developer ID 签名通过验收", () => {
  assert.doesNotThrow(() =>
    validateProductionSignatureEvidence({
      bundleIdentifier: "com.jackvoice.app",
      codesignDetails: {
        identifier: "com.jackvoice.app",
        teamId: "ABCDEFGHIJ",
        signature: null,
        authorities: [developerIdentity, "Developer ID Certification Authority"],
      },
      designatedRequirement:
        '# designated => identifier "com.jackvoice.app" and anchor apple generic and certificate leaf[subject.OU] = "ABCDEFGHIJ"',
      hasCodeResources: true,
      expectedTeamId: "ABCDEFGHIJ",
    }),
  );
});

test("拒绝造成 TCC 授权失效的临时签名", () => {
  assert.throws(
    () =>
      validateProductionSignatureEvidence({
        bundleIdentifier: "com.jackvoice.app",
        codesignDetails: {
          identifier: "jackvoice-a68265107b8b93e1",
          teamId: "not set",
          signature: "adhoc",
          authorities: [],
        },
        designatedRequirement: '# designated => cdhash H"0123456789ABCDEF"',
        hasCodeResources: false,
      }),
    /生产签名验收失败/,
  );
});

const appleDevelopment = "Apple Development: Example (ZYXWVUTSRQ)";
const secondAppleDevelopment = "Apple Development: Another (QRSTUVWXYZ)";

test("解析钥匙串里全部签名身份", () => {
  const output = `
  1) AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA "${developerIdentity}"
  2) BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB "${appleDevelopment}"
     2 valid identities found
  `;
  assert.deepEqual(parseCodesigningIdentityNames(output), [developerIdentity, appleDevelopment]);
});

test("开发版优先用 Apple Development 证书", () => {
  assert.deepEqual(
    selectDevelopmentSigningIdentity({ availableNames: [developerIdentity, appleDevelopment] }),
    { identity: appleDevelopment, source: "keychain" },
  );
});

test("多张开发证书时每次都选同一张", () => {
  const expected = { identity: secondAppleDevelopment, source: "keychain" };
  assert.deepEqual(
    selectDevelopmentSigningIdentity({ availableNames: [appleDevelopment, secondAppleDevelopment] }),
    expected,
  );
  assert.deepEqual(
    selectDevelopmentSigningIdentity({ availableNames: [secondAppleDevelopment, appleDevelopment] }),
    expected,
  );
});

test("没有开发证书时退回 Developer ID，再退回临时签名", () => {
  assert.deepEqual(selectDevelopmentSigningIdentity({ availableNames: [developerIdentity] }), {
    identity: developerIdentity,
    source: "keychain",
  });
  assert.deepEqual(selectDevelopmentSigningIdentity({ availableNames: [] }), {
    identity: null,
    source: "none",
  });
});

test("开发版签名可以显式指定或强制临时签名", () => {
  assert.deepEqual(
    selectDevelopmentSigningIdentity({
      availableNames: [appleDevelopment, developerIdentity],
      explicitIdentity: developerIdentity,
    }),
    { identity: developerIdentity, source: "explicit" },
  );
  assert.deepEqual(
    selectDevelopmentSigningIdentity({ availableNames: [appleDevelopment], explicitIdentity: "-" }),
    { identity: null, source: "explicit-adhoc" },
  );
  assert.throws(
    () =>
      selectDevelopmentSigningIdentity({
        availableNames: [appleDevelopment],
        explicitIdentity: secondAppleDevelopment,
      }),
    /不在当前钥匙串/,
  );
});

test("开发版 codesign 参数", () => {
  assert.deepEqual(developmentCodesignArgs(null, "/tmp/Dev.app"), [
    "--force",
    "--deep",
    "--sign",
    "-",
    "/tmp/Dev.app",
  ]);
  const signed = developmentCodesignArgs(appleDevelopment, "/tmp/Dev.app");
  assert.ok(signed.includes(appleDevelopment));
  assert.ok(!signed.includes("--options"));
});

test("只绑 CDHash 的 DR 视为不稳定", () => {
  assert.equal(designatedRequirementIsStable('# designated => cdhash H"0123456789ABCDEF"'), false);
  assert.equal(
    designatedRequirementIsStable(
      `# designated => identifier "com.jackvoice.app.dev" and anchor apple generic and certificate leaf[subject.CN] = "${appleDevelopment}"`,
    ),
    true,
  );
});
