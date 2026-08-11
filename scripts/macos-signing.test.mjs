import assert from "node:assert/strict";
import test from "node:test";

import {
  parseCodesignDetails,
  parseDeveloperIdIdentities,
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
