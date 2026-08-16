# JackVoice

JackVoice 是一个开源桌面语音听写工具：按一次全局快捷键开始说话，再按一次结束，转写结果会尝试插入当前输入位置；无法安全插入时，文字仍会保留在剪贴板和本地历史中。

它不是系统输入法，也不提供大模型润色。当前目标是把“采音、实时转写、轻量词库处理、可靠交付”这条链路做得透明、可控。

## 功能

- `Option + Space` 开始或结束听写，快捷键可修改
- 切换应用不中断，悬浮窗实时预览识别文字
- 识别结束后尝试粘贴到当前焦点，失败时保留到剪贴板
- 自动添加标点默认开启；可选“自动整理口语”，默认关闭以忠实保留原话
- 热词可提高专有词识别率，替换词只在本地做确定性后处理
- 本地历史支持复制、播放、定位文件和逐条删除
- 每次听写都会优先生成本地 WAV，网络或识别失败不影响录音保存
- 录音不会自动到期或删除，可从听写详情定位对应文件
- macOS 可选在听写录音期间临时静音系统输出，停止录音后自动恢复
- 识别服务采用 BYOK：用户自行填写豆包语音 API Key

> 当前经过完整人工验证的是 macOS 12 及以上版本。Windows 共用大部分 Rust/前端核心，但仍处于实验支持阶段，欢迎测试与贡献。

## 隐私与数据

JackVoice 不托管转写服务。听写时，麦克风音频会通过加密连接发送给用户自行配置的火山引擎豆包语音识别服务；该服务如何处理数据受用户与服务商之间的协议约束。

- 正式版 API Key 存入操作系统凭据库；开发版存入权限受限的隔离凭据文件，二者均不写入共享的 `settings.json`
- 转写文本、词库和偏好设置保存在本机应用数据目录
- 本地录音永久保存在应用数据目录，JackVoice 不执行自动清理
- 历史中的单条删除只清理应用内索引；录音文件由用户在听写详情中定位后自行管理

完整说明见 [PRIVACY.md](PRIVACY.md)。参与开发时，仍建议在开源或提交日志前自行检查历史文件和旧备份中是否存在真实凭据。

## 快速开始

### 环境要求

- Node.js 20 或更高版本
- Rust 1.88 或更高版本
- macOS 12 或更高版本（当前主要支持平台）
- 构建 WebRTC 音频处理依赖所需的 Meson、Ninja 和 pkg-config

macOS 可执行：

```bash
brew install meson ninja pkg-config
npm ci
npm run dev
```

常用命令：

```bash
# 仅构建前端
npm run build

# 启动独立开发版（com.jackvoice.app.dev）
npm run dev

# 构建开发版 .app（本地临时签名）
npm run build:desktop:dev

# 构建本地签名 DMG；使用 Developer ID，但不提交 Apple 公证
npm run build:desktop

# 构建 GitHub Release 正式 DMG；强制 Developer ID 签名、Apple 公证和 Gatekeeper 验收
npm run build:desktop:release

# 运行构建脚本回归测试
npm test

# Rust 测试
cargo test --locked --manifest-path src-tauri/Cargo.toml

# Rust 格式与静态检查
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --locked --all-targets --manifest-path src-tauri/Cargo.toml -- -D warnings
```

`npm run dev` 和桌面构建命令会固定使用项目内置的 Abseil；若检测到 WebRTC 曾使用 Homebrew Abseil 生成的不兼容缓存，启动脚本会只清理并重建该依赖。一般不再需要手动清理整个 Cargo target 目录。macOS Developer ID 构建产物默认放在 `~/Library/Caches/JackVoice/release-cargo-target`，避免仓库位于 iCloud 的“桌面”或“文稿”目录时，File Provider 元数据破坏 Developer ID 签名。

macOS Developer ID 构建不允许使用 ad-hoc 临时签名。`npm run build:desktop` 会自动选择钥匙串中唯一的 `Developer ID Application`；如果存在多个发布身份，必须显式指定完整证书名称：

```bash
export JACKVOICE_MACOS_SIGNING_IDENTITY="Developer ID Application: Example Studio (ABCDEFGHIJ)"
npm run build:desktop
```

`npm run build:desktop` 只生成 Developer ID 签名、尚未公证的本地 QA 包，并会主动忽略环境中可能残留的 Apple 公证凭据。构建结束后会检查 `com.jackvoice.app`、Team ID、Developer ID 签名链、CodeResources 和 Designated Requirement；该包不能作为 GitHub Release 正式分发。

`npm run build:desktop:release` 才是正式交付命令。它要求完整的 Apple 公证凭据，在 Tauri 生成并签名最终 DMG 后，通过 `notarytool` 提交该 DMG 并等待 Apple 返回 `Accepted`，再执行 ticket stapling、`stapler validate` 与 Gatekeeper 验收；任一步失败都不会生成 `delivery`。验收通过后，脚本才会在 `bundle/dmg/delivery` 生成带构建标识和最终 DMG 内容哈希的唯一文件名，并同时生成 `.sha256` 与包含公证 Submission ID 的 `.json` 清单。只应分发 `delivery` 目录中的 DMG。

如需让 CI 使用自己的可追溯构建号，可显式设置 `JACKVOICE_BUILD_ID`；该值会同时进入交付文件名、清单和应用“关于”页：

```bash
export JACKVOICE_BUILD_ID="20260811T073045123Z"
npm run build:desktop:release
```

稳定的 Developer ID 与固定 Bundle ID 共同构成 macOS TCC 权限身份，因此首发后的正常升级可以继续沿用麦克风和辅助功能授权。

若绕过 npm 脚本直接执行 Cargo 并再次混入了系统 Abseil，可手动只清理对应依赖后重试：

```bash
cargo clean --manifest-path src-tauri/Cargo.toml -p webrtc-audio-processing-sys
npm run dev
```

### 配置识别服务

1. 在火山引擎豆包语音控制台创建应用并开通流式语音识别。
2. 在首次引导或 JackVoice“设置 → 识别”中填写 API Key，然后点击“验证并保存”。配置成功后，普通界面只显示验证状态；需要时可点击“更换 API Key”。

普通界面会自动使用小时版识别资源，并隐藏服务商内部参数。源码二次开发需要切换并发版或接入云端热词表时，可在应用退出后修改共享 `settings.json` 中的 `volcResourceId` 与 `volcBoostingTableId`；默认资源为 `volc.seedasr.sauc.duration`，并发版为 `volc.seedasr.sauc.concurrent`。

开发版不会读取正式版系统凭据。在开发版界面中填写一次并通过连接测试后，会保存到开发版状态目录中的 `dev-credentials.json` 私有文件（macOS/Linux 权限为 `0600`）。这样源码重编译后不会因 Debug 签名变化而丢失 Key，日常运行通常无需设置环境变量。`JACKVOICE_VOLC_API_KEY` 只用于 CI、临时联调或显式覆盖：

```bash
export JACKVOICE_VOLC_API_KEY="你的开发用 API Key"
npm run dev
```

早期开发版若曾把密钥保存在 Keychain，新版会尽力自动迁移；旧条目无法读取时，在设置中重新填写一次即可。凭据读取失败只会显示可恢复提示，不会阻止应用启动。

界面会区分“未配置”“已配置、待验证”“验证通过”“验证失败”和“凭据不可用”。首次使用且尚未填写 API Key 时只显示“未配置”，不会提示迁移或升级。保存操作会先连接真实服务验证，再写入系统凭据库，只有两步都成功才显示“验证通过”。

端到端探针同样只读取该环境变量，不会访问正式版凭据或数据。开发期可使用 16 kHz、单声道、16 bit PCM：

```bash
cd src-tauri
cargo run --example volc_probe -- /path/to/sample.pcm
cargo run --example volc_probe -- /path/to/sample.pcm "OpenClaw,Broll"
```

## 本地数据位置

正式版和开发版共用业务数据目录：

```text
共享业务数据：~/Library/Application Support/com.jackvoice.shared
正式版客户端状态：~/Library/Application Support/com.jackvoice.app
开发版客户端状态：~/Library/Application Support/com.jackvoice.app.dev
```

两版会看到并修改同一份历史、录音、词库和非秘密识别设置。API Key 仍按正式版/开发版隔离：正式版使用系统凭据库，开发版使用私有本地文件；引导完成状态、开机启动和悬浮窗位置等客户端状态也分别保存。

由于业务文件会被写入，为避免两个进程并发修改造成数据冲突，正式版和开发版不能同时运行。任意一版首次启动新版时，都会尝试把旧业务数据从 `~/Library/Application Support/com.jackvoice.app` 迁移到共享目录。

## macOS 权限

首次使用可能需要授予：

- 麦克风：采集听写音频
- 辅助功能：判断焦点并模拟粘贴
- 输入监控或相关权限：注册全局快捷键，具体取决于系统版本

应用只会在检测到真实文本输入位置时尝试自动粘贴；否则不会发送粘贴按键，文字会留在剪贴板。

### 复测首次启动流程

开发或发布前，不必手工删除整份业务数据。先完全退出正在运行的 JackVoice，再执行：

```bash
# 构建独立开发版 .app，重置开发版 Onboarding 与全部 macOS TCC 权限，然后打开
# 保留 API Key、历史、录音、词库和识别设置
npm run qa:first-run:dev

# 模拟连 API Key 都不存在的新用户；原开发版凭据会先备份再移走
npm run qa:first-run:dev:fresh
```

只重置状态而不构建、不打开应用时，可以使用：

```bash
npm run reset:first-run:dev
npm run reset:first-run:production
```

脚本会把原客户端状态备份到对应数据目录下的 `.first-run-backups`，不会触碰共享业务数据。正式版 API Key 位于系统凭据库，脚本不会删除；如需复测正式版的密钥录入，请先在应用设置中主动移除。

macOS QA 构建产物默认放在 `~/Library/Caches/JackVoice/qa-cargo-target`。这样即使源码仓库位于 iCloud 同步的“桌面”或“文稿”目录，File Provider 附加的 FinderInfo 也不会破坏开发版 `.app` 的本地签名；首次构建缓存会稍慢，之后会增量复用。

macOS 权限测试必须使用 `qa:first-run:dev` 打开的构建后 `.app`，期间不要再执行 `npm run dev`。后者直接运行 `target/debug/jackvoice` 裸二进制，它的临时代码签名哈希是另一套 TCC 身份：按 `com.jackvoice.app.dev` 重置的权限不会作用到这个裸进程，也不能代表用户下载、安装后的真实授权行为。

这套本机重置适合快速回归；发布前仍应在新的 macOS 用户账户或干净虚拟机中安装最终签名、公证的产物，完成一次真正无旧数据、无旧权限、无旧钥匙串条目的验收。

## 发布与签名

仓库不包含个人 Developer ID。开发构建使用本地临时签名；正式发布时，请通过开发机钥匙串或 CI 密钥配置 Tauri/macOS 签名与公证，不要把证书、私钥、Apple 密码或 API Token 提交到仓库。

本机正式发布优先使用 App Store Connect API Key：设置 `JACKVOICE_APPLE_TEAM_ID`、`APPLE_API_ISSUER`、`APPLE_API_KEY` 和 `APPLE_API_KEY_PATH` 后运行 `npm run build:desktop:release`。也可以改用 `APPLE_ID`、应用专用的 `APPLE_PASSWORD` 和 `APPLE_TEAM_ID`；此时 `APPLE_TEAM_ID` 必须与 `JACKVOICE_APPLE_TEAM_ID` 一致。两套公证凭据不能同时设置。

如果不希望每次手动 `export`，可在仓库根目录创建被 `*.local` 规则忽略的 `.jackvoice-release.local`，使用 JSON 保存上述四个 App Store Connect 标识和本机 `.p8` 路径。`build:desktop:release` 只在正式发布构建中读取该文件，显式环境变量优先；普通桌面构建和其他项目不会继承这些公证变量。私钥内容不得写入配置文件或仓库。

GitHub Actions 的 `Release macOS` 工作流只接受 `v8.16.12` 形式且与 `package.json` 一致的现有标签，使用受保护的 `release` Environment 构建 Apple Silicon DMG，并创建草稿 GitHub Release。该 Environment 需要配置变量 `JACKVOICE_APPLE_TEAM_ID`，以及 Secrets：`APPLE_CERTIFICATE`、`APPLE_CERTIFICATE_PASSWORD`、`APPLE_API_ISSUER`、`APPLE_API_KEY`、`APPLE_API_PRIVATE_KEY`。建议限制可部署标签并启用人工批准；正式发布凭据不得提供给 Pull Request 工作流。

开发版仍使用独立的 `com.jackvoice.app.dev` 和本地临时签名，不得作为正式版分发。正式安装包发布时必须使用 `bundle/dmg/delivery` 中的唯一命名产物，附带生成的校验和、交付清单、完整第三方许可清单与对应源码版本，并完成 Developer ID 签名、公证和干净机器安装验证。

发布前请完成 [OPEN_SOURCE_CHECKLIST.md](OPEN_SOURCE_CHECKLIST.md) 中的检查，尤其是凭据扫描、历史清理决策、第三方资产授权和品牌检索。

## 参与贡献

请先阅读 [CONTRIBUTING.md](CONTRIBUTING.md)、[SECURITY.md](SECURITY.md) 和 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 许可证

源代码按 [Apache License 2.0](LICENSE) 发布。项目名称与 Logo 的使用规则见 [TRADEMARKS.md](TRADEMARKS.md)；第三方组件仍受各自许可证约束，见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
