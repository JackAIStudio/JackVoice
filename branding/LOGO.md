# JackVoice Logo

## 正式版本

当前唯一正式 Logo 是 **Whisper TX / 轻声发射器**：抽象的方形无线麦克风发射器、一个拾音孔和一条低振幅气声线。

主源文件：

- `logo-v4-whisper-tx/jackvoice-whisper-tx-app.svg`
- `logo-v4-whisper-tx/jackvoice-whisper-tx-app-1024.png`
- `logo-v4-whisper-tx/jackvoice-whisper-tx-macos.svg`（透明画布与 macOS 光学校准留白，仅用于 ICNS）
- `logo-v4-whisper-tx/jackvoice-whisper-tx-mark-dark.svg`
- `logo-v4-whisper-tx/jackvoice-whisper-tx-mark-light.svg`
- `logo-v4-whisper-tx/jackvoice-whisper-tx-mono.svg`

其他 `logo/`、`logo-v2/`、`logo-v3-wireless/` 和 `recommended/` 目录均为历史探索稿，不再用于应用或文档。

## 设计依据

Logo 来自当前源码的真实产品特性：

1. WebRTC AGC 自动抬升轻声与耳语音量（`src-tauri/src/audio.rs`）
2. 全局快捷键启动独立听写会话，切换应用不中断（`src-tauri/src/session.rs`）
3. 热词提高识别率，本地替换词控制最终写法（`src-tauri/src/hotwords.rs`）
4. 识别结果先复制，再探测真实输入焦点后安全插入（`src-tauri/src/delivery.rs`）

视觉气质：安静、克制、工具感、适合开源项目。

## 运行时同步点

- `src-tauri/icons/`：Tauri 开发和生产打包图标，包含 PNG、ICNS、ICO、Windows、iOS 与 Android 资源
- `public/jackvoice-icon.svg`：主界面、首次引导和开发浏览器 favicon
- `README.md`：GitHub / 开源项目头图

更新正式 Logo 后，统一运行：

```bash
npm run icons
```

脚本直接从 SVG 生成通用资源和透明的 1024 PNG，避免预览工具把透明圆角烘成白底；随后只用带 96 px 透明安全区的 macOS 专用源文件覆盖 `icon.icns`，不会把 macOS 留白错误应用到 Windows、iOS 或 Android 图标。
