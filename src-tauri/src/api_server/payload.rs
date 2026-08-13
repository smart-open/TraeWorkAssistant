use serde_json::{json, Value};

/// OpenAI 请求体 → SOLO llm_utils_chat 请求体改写
/// 参考 traework2api/internal/upstream/payload.go
pub fn prepare_body(src: &[u8], default_model: &str) -> Vec<u8> {
    let mut obj: Value = match serde_json::from_slice(src) {
        Ok(v) => v,
        Err(_) => return src.to_vec(),
    };
    let obj_mut = obj.as_object_mut().unwrap();

    // 强制 stream + function
    obj_mut.insert("stream".into(), json!(true));
    obj_mut.insert("function".into(), json!(super::FUNCTION));

    // messages content string → [{type:text, text:...}]
    if let Some(msgs) = obj_mut.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for mi in msgs.iter_mut() {
            if let Some(m) = mi.as_object_mut() {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();

                // assistant tool_calls: function → function_call
                if role == "assistant" {
                    if let Some(tcs) = m.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                        let kept: Vec<Value> = tcs
                            .iter_mut()
                            .filter_map(|tc| {
                                let tcm = tc.as_object_mut()?;
                                if let Some(fn_val) = tcm.remove("function") {
                                    tcm.insert("function_call".into(), fn_val);
                                }
                                let has_name = tcm
                                    .get("function_call")
                                    .and_then(|fc| fc.get("name"))
                                    .and_then(|n| n.as_str())
                                    .map(|s| !s.trim().is_empty())
                                    .unwrap_or(false);
                                if has_name { Some(tc.clone()) } else { None }
                            })
                            .collect();
                        if kept.is_empty() {
                            m.remove("tool_calls");
                        } else {
                            *tcs = kept;
                        }
                    }
                }

                // content string → array
                if let Some(content) = m.get("content") {
                    if let Some(s) = content.as_str() {
                        m.insert(
                            "content".into(),
                            json!([{ "type": "text", "text": s }]),
                        );
                    }
                }
            }
        }
    }

    // model → config_name + model
    let model = obj_mut
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_model.to_string());
    obj_mut.insert("config_name".into(), json!(model));
    obj_mut.insert("model".into(), json!(model));

    // normalize tool_choice
    normalize_tool_choice(obj_mut);
    // normalize tools
    normalize_tools(obj_mut);

    serde_json::to_vec(&obj).unwrap_or_else(|_| src.to_vec())
}

fn normalize_tool_choice(obj: &mut serde_json::Map<String, Value>) {
    let suppress = |obj: &mut serde_json::Map<String, Value>| {
        obj.remove("tools");
        obj.remove("functions");
    };
    let tc = match obj.remove("tool_choice") {
        Some(v) => v,
        None => return,
    };
    match tc {
        Value::String(s) => {
            if s.trim().eq_ignore_ascii_case("none") {
                suppress(obj);
            } else {
                obj.insert("tool_choice".into(), Value::String(s));
            }
        }
        Value::Object(map) => {
            let typ = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            match typ.as_str() {
                "none" => suppress(obj),
                "auto" | "required" => {
                    obj.insert("tool_choice".into(), Value::String(typ));
                }
                "function" => {
                    let name = map
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| map.get("name").and_then(|n| n.as_str()))
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "auto".to_string());
                    obj.insert("tool_choice".into(), Value::String(name));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn normalize_tools(obj: &mut serde_json::Map<String, Value>) {
    let raw = match obj.get_mut("tools") {
        Some(v) => v,
        None => return,
    };
    let list = match raw.as_array_mut() {
        Some(a) => a,
        None => return,
    };
    if list.is_empty() {
        obj.remove("tools");
        return;
    }
    let mut out = Vec::new();
    for item in list.iter_mut() {
        let t = match item.as_object_mut() {
            Some(o) => o,
            None => continue,
        };
        let fn_obj = match t.get_mut("function").and_then(|f| f.as_object_mut()) {
            Some(o) => o,
            None => continue,
        };
        // parameters object → JSON string
        if let Some(params) = fn_obj.get("parameters") {
            if params.is_object() {
                if let Ok(s) = serde_json::to_string(params) {
                    fn_obj.insert("parameters".into(), Value::String(s));
                }
            }
        }
        out.push(item.clone());
    }
    if out.is_empty() {
        obj.remove("tools");
    } else {
        *raw = Value::Array(out);
    }
}
