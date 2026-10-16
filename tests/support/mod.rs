use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

// 编译一次原生测试客户端，再为各测试复制；无需 sh、Node 或 PowerShell。
// 源码仅用标准库，题库内容由测试生成，所有样本都在本机模拟。
pub fn install_client(path: &Path) {
    static BINARY: OnceLock<Vec<u8>> = OnceLock::new();
    let binary = BINARY.get_or_init(|| {
        let directory = std::env::temp_dir().join(format!("msd-test-build-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let bank: serde_json::Value =
            serde_json::from_str(include_str!("../../assets/knowledge-reference.json")).unwrap();
        let mut references = String::new();
        for reference in bank["references"].as_array().unwrap() {
            for (index, probe) in reference["probes"].as_array().unwrap().iter().enumerate() {
                references.push_str(&format!(
                    "({:?}, {:?}, {}),\n",
                    probe["prompt"].as_str().unwrap(),
                    probe["expected"].as_f64().unwrap().to_string(),
                    [0, 8, 12, 20].contains(&index),
                ));
            }
        }
        let source =
            include_str!("mock_client.rs").replace("/* KNOWLEDGE_REFERENCES */", &references);
        let source_path = directory.join("client.rs");
        let executable = directory.join(format!("client{}", std::env::consts::EXE_SUFFIX));
        fs::write(&source_path, source).unwrap();
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-O", "--crate-name", "mock_client"])
            .arg(&source_path)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let binary = fs::read(executable).unwrap();
        fs::remove_dir_all(directory).unwrap();
        binary
    });
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, binary).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
