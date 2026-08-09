# JackVoice

JackVoice 是一个开源桌面语音听写工具：按一次全局快捷键开始说话，再按一次结束，转写结果会尝试插入当前输入位置；无法安全插入时，文字仍会保留在剪贴板和本地历史中。

它不是系统输入法，也不提供大模型润色。当前目标是把“采音、实时转写、轻量词库处理、可靠交付”这条链路做得透明、可控。

## 功能

- `Option + Space` 开始或结束听写，快捷键可修改
- 切换应用不中断，悬浮窗实时预览识别文字
- 识别结束后尝试粘贴到当前焦点，失败时保留到剪贴板
- 热词可提高专有词识别率，替换词只在本地做确定性后处理
- 本地历史支持复制、播放、定位文件、逐条删除和全部清空
- 本地 WAV 可选择不保存、保留 7 天、保留 30 天或永久保存
- 识别服务采用 BYOK：用户自行填写豆包语音 API Key（火山控制台字段名为 APP Key）

> 当前经过完整人工验证的是 macOS 12 及以上版本。Windows 共用大部分 Rust/前端核心，但仍处于实验支持阶段，欢迎测试与贡献。

## 隐私与数据

JackVoice 不托管转写服务。听写时，麦克风音频会通过加密连接发送给用户自行配置的火山引擎豆包语音识别服务；该服务如何处理数据受用户与服务商之间的协议约束。

- 正式版 API Key 存入操作系统凭据库；开发版存入权限受限的隔离凭据文件，二者均不写入共享的 `settings.json`
- 转写文本、词库和偏好设置保存在本机应用数据目录
- 录音是否落盘以及保留期限由用户选择；全新安装默认保留 30 天，旧版本升级时沿用原来的永久保留行为，避免静默删数据
- 历史中的单条删除与“清空全部”会同时处理关联录音和旧备份
- 旧版本若曾把 API Key 写入设置文件，新版首次启动会迁移到系统凭据库并重写设置文件

完整说明见 [PRIVACY.md](PRIVACY.md)。若你使用过早期开发版，仍建议在开源或提交日志前自行检查历史文件和旧备份中是否存在真实凭据。

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

# 构建正式安装包；签名身份应由本机或 CI 环境提供
npm run build:desktop

# Rust 测试
cargo test --locked --manifest-path src-tauri/Cargo.toml

# Rust 格式与静态检查
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --locked --all-targets --manifest-path src-tauri/Cargo.toml -- -D warnings
```

`npm run dev` 和桌面构建命令会固定使用项目内置的 Abseil；若检测到 WebRTC 曾使用 Homebrew Abseil 生成的不兼容缓存，启动脚本会只清理并重建该依赖。一般不再需要手动清理整个 Cargo target 目录。

若绕过 npm 脚本直接执行 Cargo 并再次混入了系统 Abseil，可手动只清理对应依赖后重试：

```bash
cargo clean --manifest-path src-tauri/Cargo.toml -p webrtc-audio-processing-sys
npm run dev
```

### 配置识别服务

1. 在火山引擎豆包语音控制台创建应用并开通流式语音识别。
2. 打开 JackVoice“设置 → 识别”，填写豆包语音 API Key（控制台中的 APP Key），然后点击“保存并测试”。
3. 资源 ID 默认是 `volc.seedasr.sauc.duration`；购买并发版后可改为 `volc.seedasr.sauc.concurrent`。
4. 如需大热词表，可在自学习平台创建热词文件，再填写 `boosting_table_id`；留空时会随请求传入有限数量的本地热词。

开发版不会读取正式版系统凭据。在开发版界面中填写一次并通过连接测试后，会保存到开发版状态目录中的 `dev-credentials.json` 私有文件（macOS/Linux 权限为 `0600`）。这样源码重编译后不会因 Debug 签名变化而丢失 Key，日常运行通常无需设置环境变量。`JACKVOICE_VOLC_API_KEY` 只用于 CI、临时联调或显式覆盖：

```bash
export JACKVOICE_VOLC_API_KEY="你的开发用 API Key（控制台 APP Key）"
npm run dev
```

早期开发版若曾把密钥保存在 Keychain，新版会尽力自动迁移；旧条目无法读取时，在设置中重新填写一次即可。凭据读取失败只会显示可恢复提示，不会阻止应用启动。

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

## 发布与签名

仓库不包含个人 Developer ID。开发构建使用本地临时签名；正式发布时，请通过开发机钥匙串或 CI 密钥配置 Tauri/macOS 签名与公证，不要把证书、私钥、Apple 密码或 API Token 提交到仓库。

当前仓库只提供早期源码，不应把本地临时签名的开发版当作官方发行版。正式安装包发布时必须附带完整第三方许可清单、校验和与对应源码版本，并完成签名、公证和干净机器安装验证。

发布前请完成 [OPEN_SOURCE_CHECKLIST.md](OPEN_SOURCE_CHECKLIST.md) 中的检查，尤其是凭据扫描、历史清理决策、第三方资产授权和品牌检索。

## 参与贡献

请先阅读 [CONTRIBUTING.md](CONTRIBUTING.md)、[SECURITY.md](SECURITY.md) 和 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 许可证

源代码按 [Apache License 2.0](LICENSE) 发布。项目名称与 Logo 的使用规则见 [TRADEMARKS.md](TRADEMARKS.md)；第三方组件仍受各自许可证约束，见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
