//! 机器码/设备标识重置（原 PS Reset-MachineId / Reset-DeviceIdsOnly 475-630 对译）。

use std::path::Path;

use super::{profile::Layout, ProgressSink, Session, StepStatus};

/// machineid 新值：32 位小写 hex（uuid v4 simple 形态，对齐 PS 32 位随机 hex）
fn random_hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// deviceId 新值：n 位纯数字（对齐 PS 15 位随机数字）
fn random_digits(n: usize) -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut s: String = bytes.iter().map(|b| char::from(b'0' + (b % 10))).collect();
    s.truncate(n);
    s
}

/// 重置 6 层机器码中的 MachineGuid（需管理员）。非管理员时跳过并提示，不阻断切换。
/// 注意：必须用 create()（KEY_READ|KEY_WRITE）——open() 仅 KEY_READ 只读句柄，
/// set_string 必然拒绝访问（os error 5），管理员权限下也一样（issue #33）。
pub fn reset_machine_id(sink: &dyn ProgressSink) -> Result<(), String> {
    let new_guid = uuid::Uuid::new_v4().to_string();
    match windows_registry::LOCAL_MACHINE
        .create("SOFTWARE\\Microsoft\\Cryptography")
        .and_then(|k| k.set_string("MachineGuid", &new_guid))
    {
        Ok(()) => sink.step(
            "machine",
            StepStatus::Ok,
            &format!("机器码已重置为 {new_guid}"),
        ),
        Err(e) => sink.step(
            "machine",
            StepStatus::Skip,
            &format!("重置机器码需要管理员权限，已跳过（不影响账号切换）: {e}"),
        ),
    }
    Ok(())
}

/// 6 层设备标识重置（本项目自主设计，PS 498-509 注释保留）：
/// 1. machineid 文件 → 新 hex32 UUID
/// 2. storage.json telemetry.machineId / telemetry.sqmId → 替换（点号键名，非嵌套！）
/// 3. storage.json aha.device.device_id → 替换 + 删 has_device_id_updated_to_aha 标记位
/// 4. aha\TinyStorage device_id → 内容含 device_id 的文件全删
/// 5. 注册表 MachineGuid → 替换（需管理员，失败跳过不阻断）
/// 6. trae-webview 追踪数据（Network/Local Storage/Session Storage）→ 清除
/// 成功 ≥4/6 报 ok，否则 info。
pub fn reset_device_ids_only(sess: &Session, sink: &dyn ProgressSink) -> Result<(), String> {
    // F-48：6 层重置针对 icube 布局（storage.json/machineid/vscdb），其他布局随各自批次接入
    if sess.prof.layout == Layout::Chromium {
        // 豆包为 Chromium 壳，登录态与 machineId 等设备标识无强绑定（plan §0：设备隔离
        // 风险低），目录级快照恢复即完成账号隔离，无需（也没有）6 层重置语义
        sink.step(
            "device",
            StepStatus::Skip,
            &format!(
                "{} 为 Chromium 布局，登录态与设备标识无强绑定，无需重置（快照恢复即完成隔离）",
                sess.prof.app_name
            ),
        );
        return Ok(());
    }
    if sess.prof.layout != Layout::Icube {
        sink.step(
            "device",
            StepStatus::Error,
            &format!(
                "{} 布局为 '{}'，设备重置管线尚未接入（F-48 预留）",
                sess.prof.app_name,
                sess.prof.layout.as_str()
            ),
        );
        return Err(super::thrown(
            sink,
            &format!(
                "{} 的设备重置尚未实现（布局={}）",
                sess.prof.app_name,
                sess.prof.layout.as_str()
            ),
        ));
    }
    let dir = sess.prof.data_dir.clone();
    if !dir.exists() {
        sink.step(
            "device",
            StepStatus::Error,
            &format!("TRAE 数据目录不存在: {}", dir.display()),
        );
        return Ok(()); // PS 语义：return（非 throw）
    }

    let new_machine_id = random_hex32();
    let new_device_id = random_digits(15);
    let new_sqm_id = uuid::Uuid::new_v4().to_string();
    let mut reset_count = 0usize;

    // 1. machineid 文件（无 BOM 写入：PS 审查修复点——BOM 会被客户端连 BOM 读入；
    //    Rust fs::write 天然无 BOM）
    let machine_id_file = dir.join("machineid");
    if machine_id_file.exists() {
        match std::fs::write(&machine_id_file, &new_machine_id) {
            Ok(()) => {
                sink.step("device", StepStatus::Ok, "[1/6] machineid 已重置");
                reset_count += 1;
            }
            Err(e) => sink.step(
                "device",
                StepStatus::Skip,
                &format!("[1/6] machineid 重置失败: {e}"),
            ),
        }
    } else {
        sink.step("device", StepStatus::Skip, "[1/6] machineid 文件不存在，跳过");
    }

    // 2 & 3. storage.json — telemetry.machineId / sqmId + aha.device.device_id
    // 注意：storage.json 在 User\globalStorage\ 下，且使用点号键名（非嵌套对象）
    let storage_file = dir.join("User").join("globalStorage").join("storage.json");
    if storage_file.exists() {
        match edit_storage_device_ids(&storage_file, &new_machine_id, &new_sqm_id, &new_device_id)
        {
            Ok(true) => {
                sink.step("device", StepStatus::Ok, "[2-3/6] storage.json 设备标识已重置");
                reset_count += 1;
            }
            Ok(false) => sink.step("device", StepStatus::Skip, "[2-3/6] storage.json 无需修改"),
            Err(e) => sink.step(
                "device",
                StepStatus::Skip,
                &format!("[2-3/6] storage.json 重置失败: {e}"),
            ),
        }
    } else {
        sink.step("device", StepStatus::Skip, "[2-3/6] storage.json 不存在，跳过");
    }

    // 4. aha/TinyStorage device_id — 清除。递归遍历目录下全部文件，
    //    内容含 "device_id" 字符串的文件删除（按内容匹配逐文件删，非整目录删除！）
    let tiny_storage_dir = dir.join("aha").join("TinyStorage");
    if tiny_storage_dir.exists() {
        match clear_tiny_storage(&tiny_storage_dir) {
            Ok(removed) => {
                sink.step(
                    "device",
                    StepStatus::Ok,
                    &format!("[4/6] aha/TinyStorage device_id 已清除（删除 {removed} 个文件）"),
                );
                reset_count += 1;
            }
            Err(e) => sink.step(
                "device",
                StepStatus::Skip,
                &format!("[4/6] aha/TinyStorage 清除失败: {e}"),
            ),
        }
    } else {
        sink.step("device", StepStatus::Skip, "[4/6] aha/TinyStorage 目录不存在，跳过");
    }

    // 5. 注册表 MachineGuid（需管理员；写 newSqmId GUID，PS 同款）
    //    create() = KEY_READ|KEY_WRITE（open() 只读，见 reset_machine_id 注释）；
    //    错误透出真实原因，不再一律归因为「需要管理员权限」（issue #33）
    match windows_registry::LOCAL_MACHINE
        .create("SOFTWARE\\Microsoft\\Cryptography")
        .and_then(|k| k.set_string("MachineGuid", &new_sqm_id))
    {
        Ok(()) => {
            sink.step("device", StepStatus::Ok, "[5/6] 注册表 MachineGuid 已重置");
            reset_count += 1;
        }
        Err(e) => sink.step(
            "device",
            StepStatus::Skip,
            &format!("[5/6] 注册表 MachineGuid 重置失败，已跳过（如为拒绝访问请以管理员身份运行）: {e}"),
        ),
    }

    // 6. trae-webview 追踪数据（Cookies/Local Storage/Session Storage）
    let webview_dir = dir.join("Partitions").join("trae-webview");
    if webview_dir.exists() {
        let mut ok = true;
        for d in ["Network", "Local Storage", "Session Storage"] {
            let target = webview_dir.join(d);
            if target.exists() {
                if std::fs::remove_dir_all(&target).is_err() {
                    ok = false;
                }
            }
        }
        if ok {
            sink.step("device", StepStatus::Ok, "[6/6] trae-webview 追踪数据已清除");
            reset_count += 1;
        } else {
            sink.step("device", StepStatus::Skip, "[6/6] trae-webview 清除失败: 部分目录删除失败");
        }
    } else {
        sink.step("device", StepStatus::Skip, "[6/6] trae-webview 目录不存在，跳过");
    }

    let status = if reset_count >= 4 { StepStatus::Ok } else { StepStatus::Info };
    sink.step(
        "device",
        status,
        &format!("6 层设备标识重置完成（{reset_count}/6 层成功）"),
    );
    Ok(())
}

/// TinyStorage 清除：递归删除内容含 "device_id" 的文件（字节级匹配；PS 为
/// Get-Content -Raw -match，二进制内容按 lossy 字符串含子串判定）。
/// TinyStorage 可能是目录也可能是单文件——对文件 read_dir 会报
/// 「目录名称无效 (os error 267)」（issue #33 实测），故先判文件形态。
/// 子目录 read_dir 失败按最佳努力跳过（对齐 PS Get-ChildItem 语义，不中断整体清理）。
/// 返回删除的文件数。
fn clear_tiny_storage(dir: &Path) -> Result<usize, String> {
    fn contains_device_id(p: &Path) -> bool {
        std::fs::read(p)
            .map(|b| b.windows(9).any(|w| w == b"device_id"))
            .unwrap_or(false)
    }
    fn walk(d: &Path, removed: &mut usize) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, removed);
            } else if contains_device_id(&p) && std::fs::remove_file(&p).is_ok() {
                *removed += 1;
            }
        }
    }
    // 单文件形态：仅当内容含 device_id 标记时删除
    if dir.is_file() {
        if contains_device_id(dir) {
            std::fs::remove_file(dir).map_err(|e| e.to_string())?;
            return Ok(1);
        }
        return Ok(0);
    }
    let mut removed = 0usize;
    walk(dir, &mut removed);
    Ok(removed)
}

/// storage.json 设备标识编辑：点号键名整体替换 + 删标记位。
/// 返回 false = 三键与标记位均不存在（无需修改）。
/// 注：serde_json 序列化键序字母化——PS ConvertTo-Json 同样重排格式，
/// Electron 仅 JSON.parse，无影响；且无 -Depth 20 截断风险（严格更优）。
fn edit_storage_device_ids(
    path: &Path,
    machine: &str,
    sqm: &str,
    device: &str,
) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut v: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|e| e.to_string())?;
    let Some(obj) = v.as_object_mut() else {
        return Err("storage.json 非对象".to_string());
    };
    let mut changed = false;
    for (k, val) in [
        ("telemetry.machineId", machine),
        ("telemetry.sqmId", sqm),
        ("aha.device.device_id", device),
    ] {
        if obj.contains_key(k) {
            obj.insert(k.to_string(), serde_json::Value::String(val.to_string()));
            changed = true;
        }
    }
    if obj.remove("has_device_id_updated_to_aha").is_some() {
        changed = true;
    }
    if changed {
        std::fs::write(path, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 随机值格式() {
        let h = random_hex32();
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        let d = random_digits(15);
        assert_eq!(d.len(), 15);
        assert!(d.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn storage_json_设备标识编辑() {
        let dir = std::env::temp_dir().join(format!("sw-machine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("storage.json");
        // BOM 输入容错 + 三键替换 + 标记位删除
        std::fs::write(
            &p,
            "\u{feff}{\"telemetry.machineId\":\"old\",\"telemetry.sqmId\":\"s1\",\"aha.device.device_id\":\"d1\",\"has_device_id_updated_to_aha\":true,\"keep\":1}",
        )
        .unwrap();
        let changed = edit_storage_device_ids(&p, "m32", "g1", "d15").unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["telemetry.machineId"], "m32");
        assert_eq!(v["telemetry.sqmId"], "g1");
        assert_eq!(v["aha.device.device_id"], "d15");
        assert!(v.get("has_device_id_updated_to_aha").is_none());
        assert_eq!(v["keep"], 1);
        // 二次编辑：标记位已不在 → 仅三键替换仍算 changed
        assert!(edit_storage_device_ids(&p, "m2", "g2", "d2").unwrap());
        // 全部键缺失 → false
        std::fs::write(&p, "{\"other\":1}").unwrap();
        assert!(!edit_storage_device_ids(&p, "m3", "g3", "d3").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tinystorage_按内容匹配逐文件删() {
        let base = std::env::temp_dir().join(format!("sw-tiny-{}", std::process::id()));
        let tiny = base.join("aha").join("TinyStorage");
        std::fs::create_dir_all(&tiny).unwrap();
        std::fs::write(tiny.join("has_id.bin"), b"\x01\x02device_id\x03").unwrap();
        std::fs::write(tiny.join("clean.txt"), "no marker here").unwrap();
        std::fs::write(tiny.join("sub_has.bin"), b"prefix device_id suffix").unwrap();
        std::fs::create_dir_all(tiny.join("nested")).unwrap();
        std::fs::write(tiny.join("nested").join("deep.txt"), "has device_id inside").unwrap();
        assert_eq!(clear_tiny_storage(&tiny).unwrap(), 3);
        assert!(!tiny.join("has_id.bin").exists());
        assert!(tiny.join("clean.txt").exists());
        assert!(!tiny.join("sub_has.bin").exists());
        assert!(!tiny.join("nested").join("deep.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tinystorage_单文件形态按内容删() {
        // issue #33：TinyStorage 为单文件时对文件 read_dir 报 os error 267
        let base = std::env::temp_dir().join(format!("sw-tiny-file-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let f = base.join("TinyStorage");
        std::fs::write(&f, b"\x01device_id\x02").unwrap();
        assert_eq!(clear_tiny_storage(&f).unwrap(), 1);
        assert!(!f.exists());
        // 无标记的单文件保留不删
        std::fs::write(&f, b"clean content").unwrap();
        assert_eq!(clear_tiny_storage(&f).unwrap(), 0);
        assert!(f.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
