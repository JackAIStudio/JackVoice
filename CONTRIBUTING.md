# 参与贡献

感谢你改进 JackVoice。提交代码即表示你有权贡献这些内容，并同意按项目的 Apache-2.0 许可证提供该贡献。

## 开发流程

1. Fork 仓库并从默认分支创建短分支。
2. 安装 README 中列出的 Node、Rust 和原生构建依赖。
3. 只提交与问题相关的改动，避免混入个人配置、构建产物、录音或真实凭据。
4. 提交前运行：

```bash
npm ci
npm run build
cargo test --locked --manifest-path src-tauri/Cargo.toml
```

5. Pull Request 中说明用户可见变化、验证结果、平台差异和隐私/权限影响。

## 代码与产品约束

- 默认使用简体中文编写界面文案和面向用户的错误提示。
- 不在日志、测试夹具或截图中放入真实 APP Key、录音或转写内容。
- 新增网络请求、数据持久化、系统权限或第三方 SDK 时，必须同步更新 `PRIVACY.md`、首次启动披露和第三方许可说明。
- 新的 Tauri 命令应遵循最小权限原则，不要恢复全局 `window.__TAURI__` 注入或宽泛 capability。
- 视觉改动应基于 JackVoice 自己的任务模型与品牌语言，不复刻其他产品的页面结构、插画或文案。

## 报告问题

公开 Issue 请提供可复现步骤、预期/实际结果、系统版本和脱敏日志。安全问题使用 [SECURITY.md](SECURITY.md) 中的私密渠道。
