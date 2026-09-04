use crate::AgentTools;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const SERVER_NAME: &str = "jackvoice";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const INSTRUCTIONS: &str = "JackVoice MCP 只读取本机热词和替换词，不读写听写历史、录音或 API Key。校正转写时先 get_glossary：热词是常用专有词表，替换词是已知 A→B。再对文本调用 apply_replacements 做确定性替换（含最长匹配和短语锁）。若某热词等于某条替换的 from，最终写法用 to。当前版本不会修改词库。";

pub fn run_stdio_server() -> Result<(), String> {
    let tools = AgentTools::discover()?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("读取 MCP 请求失败：{error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_message(
                    &mut stdout,
                    rpc_error(Value::Null, -32700, &error.to_string()),
                )?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request["method"].as_str().unwrap_or_default();
        let response = match method {
            "initialize" => initialize_response(id, &request["params"]),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({"tools": tool_definitions()})),
            "tools/call" => call_tool_response(id, &tools, &request["params"]),
            _ => rpc_error(id, -32601, &format!("不支持的 MCP 方法：{method}")),
        };
        write_message(&mut stdout, response)?;
    }
    Ok(())
}

fn initialize_response(id: Value, params: &Value) -> Value {
    let protocol_version = params["protocolVersion"].as_str().unwrap_or("2025-06-18");
    rpc_result(
        id,
        json!({
            "protocolVersion": protocol_version,
            "capabilities": {"tools":{"listChanged":false}},
            "serverInfo": {"name":SERVER_NAME,"version":SERVER_VERSION},
            "instructions": INSTRUCTIONS
        }),
    )
}

fn call_tool_response(id: Value, tools: &AgentTools, params: &Value) -> Value {
    let Some(name) = params["name"].as_str() else {
        return rpc_error(id, -32602, "tools/call 缺少工具名称。");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match tools.call(name, arguments) {
        Ok(result) => rpc_result(id, tool_result(result, false)),
        Err(error) => rpc_result(id, tool_result(json!({"error":error}), true)),
    }
}

fn tool_result(structured: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&structured)
        .unwrap_or_else(|_| "无法序列化 JackVoice 工具结果。".to_string());
    json!({
        "content":[{"type":"text","text":text}],
        "structuredContent":structured,
        "isError":is_error
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "get_status",
            "检查本机 JackVoice 词库是否可读。不返回听写历史、录音或 API Key。",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "get_glossary",
            "一次读取热词、替换词和规范写法。校正转写时优先用这个。热词是常用专有词表；替换词是已知 A→B；canonicalTerms 是最终写法（替换词的 to，加上没有对应 from 的热词）。可用 query 过滤。",
            list_input_schema(),
        ),
        tool(
            "get_hotwords",
            "读取本机热词。英文热词可能是识别形（去空格/标点），最终写法见替换词的 to。可用 query 过滤。",
            list_input_schema(),
        ),
        tool(
            "get_replacements",
            "读取本机替换词（识别结果 A → 最终写法 B），含 from==to 的短语锁。可用 query 过滤。",
            list_input_schema(),
        ),
        tool(
            "apply_replacements",
            "用本机替换词对一段文本做确定性最长匹配替换，忽略大小写，不会二次误伤已被替换的短语。",
            json!({
                "type":"object",
                "properties":{
                    "text":{"type":"string","description":"要套用替换词的原文。"}
                },
                "required":["text"],
                "additionalProperties":false
            }),
        ),
    ]
}

fn list_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "query":{"type":"string","description":"可选。对热词或替换词 from/to 做不区分大小写的包含过滤。"},
            "limit":{"type":"integer","minimum":1,"maximum":5000,"description":"最多返回多少条，默认全部，上限 5000。"}
        },
        "additionalProperties":false
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name":name,
        "description":description,
        "inputSchema":input_schema,
        "annotations":{
            "readOnlyHint":true,
            "destructiveHint":false,
            "idempotentHint":true,
            "openWorldHint":false
        }
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "error":{"code":code,"message":message}
    })
}

fn write_message(output: &mut impl Write, message: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, &message)
        .map_err(|error| format!("写入 MCP 响应失败：{error}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|error| format!("刷新 MCP 响应失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_read_only_glossary_tools() {
        let names = tool_definitions()
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "get_status",
                "get_glossary",
                "get_hotwords",
                "get_replacements",
                "apply_replacements"
            ]
        );
    }

    #[test]
    fn tool_schemas_have_object_roots_without_root_unions() {
        for tool in tool_definitions() {
            let name = tool["name"].as_str().unwrap_or("?");
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{name} 的 inputSchema.type");
            assert!(schema.get("anyOf").is_none(), "{name} 不得使用根级 anyOf");
            assert!(schema.get("oneOf").is_none(), "{name} 不得使用根级 oneOf");
            assert!(schema.get("allOf").is_none(), "{name} 不得使用根级 allOf");
        }
    }
}
