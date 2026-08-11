use std::path::PathBuf;

use crate::models::Settings;

/// 应用全局状态。data_dir 指向 %APPDATA%\TraeWorkAssistant；
/// python_dir 指向打包后的 python 脚本目录（Tauri resource `python/`）。
pub struct AppState {
    pub data_dir: PathBuf,
    pub python_dir: PathBuf,
    pub python_exe: String,
}

impl AppState {
    pub fn new() -> Result<Self, String> {
        // 数据目录：%APPDATA%\TraeWorkAssistant，不存在则创建
        let appdata = std::env::var("APPDATA")
            .map(PathBuf::from)
            .map_err(|_| "无法读取 APPDATA 环境变量".to_string())?;
        let data_dir = appdata.join("TraeWorkAssistant");
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("创建数据目录失败: {e}"))?;

        // python 脚本目录：优先取 Tauri 资源目录下的 python/，否则回退到源码目录
        let python_dir = resolve_python_dir();

        // python 解释器：资源目录内嵌的 python.exe 优先，否则系统 python3
        let embedded = python_dir.join("python.exe");
        let python_exe = if embedded.exists() {
            embedded.to_string_lossy().to_string()
        } else {
            "python3".to_string()
        };

        Ok(Self {
            data_dir,
            python_dir,
            python_exe,
        })
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.data_dir.join(name)
    }

    pub fn settings(&self) -> Settings {
        let p = self.path("app_settings.json");
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }
}

/// 定位 python 脚本目录：先尝试 Tauri 资源（运行期），再尝试开发期相对路径。
fn resolve_python_dir() -> PathBuf {
    if let Ok(res) = std::env::var("TAURI_RESOURCE_DIR") {
        let p = PathBuf::from(res).join("python");
        if p.exists() {
            return p;
        }
    }
    // 开发期：可执行文件旁或仓库 src-python
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("python");
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("src-python")
}
