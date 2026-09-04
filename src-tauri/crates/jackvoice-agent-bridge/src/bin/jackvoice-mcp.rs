fn main() {
    if let Err(error) = run() {
        eprintln!("JackVoice MCP 启动失败：{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--health-check") => {
            let tools = jackvoice_agent_bridge::AgentTools::discover()?;
            let status = tools.call("get_status", serde_json::json!({}))?;
            println!(
                "{}",
                serde_json::to_string(&status)
                    .map_err(|error| format!("序列化健康检查失败：{error}"))?
            );
            Ok(())
        }
        Some(argument) => Err(format!("不支持的启动参数：{argument}")),
        None => jackvoice_agent_bridge::run_stdio_server(),
    }
}
