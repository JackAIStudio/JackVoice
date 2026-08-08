# JackVoice Logo v4 — Whisper TX

这版只以当前源码行为为设计依据，旧文档仅用于交叉验证。

## 核心概念

**Whisper TX / 轻声发射器**：一个抽象的方形无线麦克风发射器，接住低振幅的气声并将它轻轻托起。

- 圆角方形主体：小型无线麦克风发射器，不是传统手持麦
- 单一拾音孔：靠近左上边缘并适度放大，用最少元素表明它是采集声音的硬件轮廓
- 低振幅气声线：对应图书馆轻声、耳语甚至气口音
- charcoal：沿用应用悬浮胶囊的克制、安静气质

## 源码对应

- `audio.rs`：WebRTC AGC 为轻声提供自动增益，最高约 +24 dB
- `session.rs` / `asr.rs`：全局快捷键会话、实时上屏、二遍终稿
- `hotwords.rs`：热词提高识别率，本地替换词控制最终写法
- `delivery.rs`：先复制、再探测真实输入焦点，安全跨应用插入
- `overlay.rs` / `overlay.ts`：听写期间保持独立悬浮胶囊，不占据输入法

## 文件

- `jackvoice-whisper-tx-app.svg`：应用图标主版本
- `jackvoice-whisper-tx-macos.svg`：带透明安全区的 macOS Dock / ICNS 专用版本
- `jackvoice-whisper-tx-dev-app.svg`：带橙色 `DEV` 角标的开发版图标
- `jackvoice-whisper-tx-dev-macos.svg`：开发版 macOS Dock / ICNS 专用版本
- `jackvoice-whisper-tx-mark-dark.svg`：浅色背景品牌图形
- `jackvoice-whisper-tx-mark-light.svg`：深色背景品牌图形
- `jackvoice-whisper-tx-mono.svg`：单色印刷 / 菜单栏 / 极简场景

## 颜色

- Charcoal `#17171A`
- Warm white `#F7F7F4`
- Live mint `#2AD6AA`（保留给应用运行状态，不强制放进主标志）

## 使用原则

- 不增加传统手持麦轮廓、天线、喇叭或夸张声波
- 不复刻任何具体无线麦产品的按键、夹具或工业设计
- 32 px 以下优先使用 App 版或单色版，不再添加文字
