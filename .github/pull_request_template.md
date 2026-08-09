## 改了什么

请简要说明用户可见变化和原因。

## 如何验证

- [ ] `npm run build`
- [ ] `cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check`
- [ ] `cargo clippy --locked --all-targets --manifest-path src-tauri/Cargo.toml -- -D warnings`
- [ ] `cargo test --locked --manifest-path src-tauri/Cargo.toml`
- [ ] 已在受影响平台手动验证

## 风险检查

- [ ] 未提交真实凭据、录音、转写文本或个人路径
- [ ] 新增网络、持久化或系统权限时，已更新隐私与安全说明
- [ ] 新增素材或依赖时，已确认许可证与归属要求
