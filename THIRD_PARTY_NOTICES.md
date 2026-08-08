# 第三方组件说明

JackVoice 通过 Cargo 和 npm 使用第三方开源组件。每个组件仍受其自身许可证约束；构建时锁定的完整版本见 `src-tauri/Cargo.lock` 和 `package-lock.json`，准确许可文本以对应组件源码包为准。

主要组件包括：

| 组件 | 用途 | 常见许可证 |
| --- | --- | --- |
| Tauri 及官方插件 | 桌面运行时、快捷键、剪贴板、开机启动、文件定位 | Apache-2.0 / MIT |
| WebRTC Audio Processing | 自动增益和音频处理 | BSD-3-Clause |
| cpal | 跨平台音频采集 | Apache-2.0 |
| Tokio、Reqwest、Tungstenite | 异步运行时与网络通信 | MIT / Apache-2.0 |
| keyring | 操作系统凭据库抽象 | MIT / Apache-2.0 |
| Vite、TypeScript | 前端构建工具 | MIT / Apache-2.0 |

二进制发布者有责任基于实际锁文件和目标平台生成完整的第三方许可清单，并随安装包提供所要求的文本与归属信息。依赖升级后应重新审计，不能只依赖本页的概览。

项目文档中出现的 macOS、Windows、火山引擎、豆包语音等名称属于各自权利人，仅用于说明兼容平台或用户可配置的服务，不表示合作或背书。

## WebRTC Audio Processing 许可文本

`webrtc-audio-processing`、`webrtc-audio-processing-sys` 和 `webrtc-audio-processing-config` 源码包随附以下 BSD-3-Clause 文本：

```text
Copyright (c) 2011, Google Inc. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in the
    documentation and/or other materials provided with the distribution.

  * Neither the name of Google nor the names of its contributors may be
    used to endorse or promote products derived from this software without
    specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS;
OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR
OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF
ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
