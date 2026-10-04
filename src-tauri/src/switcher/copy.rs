//! 快照复制原语（原 PS Copy-SnapshotItem / .bak 单代轮转 / 槽位解析对译）。

use std::path::Path;

use super::{ProgressSink, Session, StepStatus};

/// Copy-SnapshotItem 等价物：文件/目录自适应 + 目标存在性回传。
/// 文件先删旧目标再拷贝：源被锁拷贝失败时不留旧文件冒充"备份成功"
///（PS 审查修复点：曾致快照留陈旧 cookie 且校验通过，恢复后登录态错乱）。
pub fn copy_snapshot_item(src: &Path, dst: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(src) else { return false };
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ok = if meta.is_dir() {
        let _ = std::fs::remove_dir_all(dst);
        dir_copy_recursive(src, dst).is_ok()
    } else {
        let _ = std::fs::remove_file(dst);
        std::fs::copy(src, dst).is_ok()
    };
    ok && dst.exists()
}

/// 递归目录复制：任一文件失败即整体失败（PS Copy-Item -ErrorAction
/// SilentlyContinue 部分失败仍算成功；Rust 更严 → 更易触发 icube「0 项恢复」
/// 回滚，偏安全方向，8.2 #9）
fn dir_copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            dir_copy_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// .bak 两代轮转（PS 689-698/975-984/1269-1278 三处同构，收敛为一个函数）：
/// 覆盖已有槽位前，把现有快照整体挪到 <slot>.bak，上一代 .bak 再挪到
/// <slot>.bak2（三代滚动淘汰，.bak2 仅供人工找回，不参与自动回退链）。
/// 背景：Switch 的"备份当前登录态到来源槽"依赖 current_account.txt 与实际登录
/// 一致；一旦不一致会把错误状态反复刷进该槽——单代 .bak 在连续两次误覆盖后
/// 原始快照即永久丢失（2026-09-29 实测事故），双代留出人工找回窗口。
pub fn rotate_bak(dest: &Path, slot: &str, sink: &dyn ProgressSink) {
    if !dest.exists() {
        return;
    }
    // 显式拼名 <slot>.bak（不用 with_extension：slot 含点号时扩展名歧义）
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let bak = parent.join(format!("{slot}.bak"));
    let bak2 = parent.join(format!("{slot}.bak2"));
    // 顺序敏感：Windows rename 目标存在即失败，必须先腾出 .bak 再逐代后移
    let _ = std::fs::remove_dir_all(&bak2);
    // .bak 不存在（首轮轮转）时 rename 失败属预期；存在却失败（如被占用）则保留
    // .bak 原样，后续 rename(dest→bak) 失败仅 Warn，尽量少丢数据
    let _ = std::fs::rename(&bak, &bak2);
    match std::fs::rename(dest, &bak) {
        Ok(()) => sink.step(
            "backup",
            StepStatus::Info,
            &format!("原 {slot} 快照已备份到 {slot}.bak（连同 .bak2 可回滚两代）"),
        ),
        Err(e) => sink.step(
            "backup",
            StepStatus::Warn,
            &format!("旧快照挪移失败（将直接覆盖）: {e}"),
        ),
    }
}

/// 恢复侧槽位解析（PS Restore-ChromiumProfile / Restore-AuthFileProfile 开头同构）：
/// 主槽缺失时回退 .bak（上次覆盖前的旧快照），两者皆缺报错。
/// 返回 (最终快照路径, 槽位名)。
pub fn resolve_slot(
    sess: &Session,
    slot: &str,
    sink: &dyn ProgressSink,
) -> Result<(std::path::PathBuf, String), String> {
    let main = sess.prof.profiles_dir.join(slot);
    if main.exists() {
        return Ok((main, slot.to_string()));
    }
    let bak = sess.prof.profiles_dir.join(format!("{slot}.bak"));
    if bak.exists() {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!(
                "账号 {slot} 主快照缺失，回退使用上一次覆盖前的备份（{slot}.bak）"
            ),
        );
        return Ok((bak, slot.to_string()));
    }
    sink.step(
        "restore",
        StepStatus::Error,
        &format!("目标账号 {slot} 无快照，请先登录该账号并保存登录态"),
    );
    Err(super::thrown(sink, &format!("目标账号 {slot} 无快照")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switcher::{Action, RunArgs, TargetApp};

    struct QuietSink;
    impl ProgressSink for QuietSink {
        fn step(&self, _: &str, _: StepStatus, _: &str) {}
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sw-copy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn copy_snapshot_item_文件先删后拷与目录替换() {
        let base = tmpdir("item");
        let src_f = base.join("src").join("a.txt");
        std::fs::create_dir_all(src_f.parent().unwrap()).unwrap();
        std::fs::write(&src_f, "v1").unwrap();
        let dst_f = base.join("dst").join("a.txt");
        assert!(copy_snapshot_item(&src_f, &dst_f));
        assert_eq!(std::fs::read_to_string(&dst_f).unwrap(), "v1");

        // 源缺失 → false 且目标保持原样（PS Test-Path 前置早退语义：不动目标）
        std::fs::remove_file(&src_f).unwrap();
        assert!(!copy_snapshot_item(&src_f, &dst_f));
        assert!(dst_f.exists(), "源缺失时前置早退，目标不被误删");
        // 先删后拷语义：源存在但内容为空时，拷贝成功后目标被新内容替换（非追加）
        std::fs::write(&src_f, "v2").unwrap();
        assert!(copy_snapshot_item(&src_f, &dst_f));
        assert_eq!(std::fs::read_to_string(&dst_f).unwrap(), "v2");

        // 目录替换语义
        let src_d = base.join("srcd").join("sub");
        std::fs::create_dir_all(&src_d).unwrap();
        std::fs::write(src_d.join("x.bin"), "data").unwrap();
        let dst_d = base.join("dstd").join("sub");
        assert!(copy_snapshot_item(&src_d.parent().unwrap(), &dst_d.parent().unwrap()));
        assert!(dst_d.join("x.bin").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rotate_bak_两代轮转与空格中文路径() {
        let base = tmpdir("带 空格 中文");
        let sink = QuietSink;
        let slot = base.join("profiles").join("槽位 A");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("f.txt"), "gen1").unwrap();
        rotate_bak(&slot, "槽位 A", &sink);
        assert!(!slot.exists());
        assert_eq!(
            std::fs::read_to_string(base.join("profiles").join("槽位 A.bak").join("f.txt"))
                .unwrap(),
            "gen1"
        );
        assert!(!base.join("profiles").join("槽位 A.bak2").exists());
        // 二次轮转：gen1 后移到 .bak2，gen2 进入 .bak
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("f.txt"), "gen2").unwrap();
        rotate_bak(&slot, "槽位 A", &sink);
        assert_eq!(
            std::fs::read_to_string(base.join("profiles").join("槽位 A.bak").join("f.txt"))
                .unwrap(),
            "gen2"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("profiles").join("槽位 A.bak2").join("f.txt"))
                .unwrap(),
            "gen1"
        );
        // 三次轮转：gen1 淘汰，.bak=gen3、.bak2=gen2（原始快照多留一代找回窗口）
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("f.txt"), "gen3").unwrap();
        rotate_bak(&slot, "槽位 A", &sink);
        assert_eq!(
            std::fs::read_to_string(base.join("profiles").join("槽位 A.bak").join("f.txt"))
                .unwrap(),
            "gen3"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("profiles").join("槽位 A.bak2").join("f.txt"))
                .unwrap(),
            "gen2"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_slot_主槽缺失回退bak_两者皆缺报错() {
        let base = tmpdir("slot");
        let mut args = RunArgs {
            action: Action::Switch,
            target_app: TargetApp::Doubao,
            user_id: Some("123".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        args.include_indexeddb = false;
        let sess = super::Session::new(&args);
        let sink = QuietSink;
        // 两者皆缺 → Err（fatal 已输出）
        assert!(resolve_slot(&sess, "404", &sink).is_err());
        // 仅 .bak → 回退
        let bak = sess.prof.profiles_dir.join("404.bak");
        std::fs::create_dir_all(&bak).unwrap();
        std::fs::write(bak.join("x"), "1").unwrap();
        let (p, slot) = resolve_slot(&sess, "404", &sink).unwrap();
        assert_eq!(slot, "404");
        assert!(p.ends_with("404.bak"));
        // 主槽出现 → 优先主槽
        std::fs::create_dir_all(sess.prof.profiles_dir.join("404")).unwrap();
        let (p, _) = resolve_slot(&sess, "404", &sink).unwrap();
        assert!(p.ends_with("404") && !p.to_string_lossy().ends_with(".bak"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
