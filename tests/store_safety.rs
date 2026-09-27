use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "subbar-store-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        // Хвост аварийного прогона с тем же pid не должен ронять тест.
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("не создать отдельный каталог данных");
        Self(path)
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_subbar"))
            .args(args)
            .env("SUBBAR_DATA_DIR", &self.0)
            .output()
            .expect("run isolated CLI")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn repeated_corruption_keeps_both_backups() {
    let dir = TestDir::new();
    fs::write(dir.0.join("state.json"), "{synthetic-invalid-one").unwrap();
    // Битый файл отложен, список пуст — это сбой, а не «пусто»: код 1.
    assert_eq!(dir.cli(&["accounts"]).status.code(), Some(1));
    fs::write(dir.0.join("state.json"), "{synthetic-invalid-two").unwrap();
    assert_eq!(dir.cli(&["accounts"]).status.code(), Some(1));
    let mut backups: Vec<_> = fs::read_dir(&dir.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state.corrupt.")
        })
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    backups.sort();
    assert_eq!(
        backups,
        vec!["{synthetic-invalid-one", "{synthetic-invalid-two"]
    );
}

#[cfg(unix)]
#[test]
fn saved_state_and_lock_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TestDir::new();
    let result = dir.cli(&["add", "codex", "Synthetic"]);
    assert!(result.status.success(), "CLI failed: {:?}", result.status);
    for name in ["state.json", "state.lock"] {
        let mode = fs::metadata(dir.0.join(name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name} закрыт от других пользователей");
    }
    let mode = fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "каталог данных закрыт от других пользователей");
}

#[test]
fn concurrent_cli_writers_do_not_lose_accounts() {
    let dir = TestDir::new();
    let mut writers = Vec::new();
    for i in 0..20 {
        writers.push(
            Command::new(env!("CARGO_BIN_EXE_subbar"))
                .args(["add", "codex", &format!("Synthetic-{i}")])
                .env("SUBBAR_DATA_DIR", &dir.0)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut writer in writers {
        assert!(writer.wait().unwrap().success());
    }
    let raw = fs::read_to_string(dir.0.join("state.json")).unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let accounts = state["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 20);
    let ids: std::collections::HashSet<_> = accounts
        .iter()
        .map(|account| account["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 20);
}

#[test]
fn parallel_removal_does_not_erase_new_accounts() {
    let dir = TestDir::new();
    assert!(dir.cli(&["add", "codex", "Old"]).status.success());
    let mut writers = Vec::new();
    writers.push(
        Command::new(env!("CARGO_BIN_EXE_subbar"))
            .args(["remove", "Old"])
            .env("SUBBAR_DATA_DIR", &dir.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    for i in 0..12 {
        writers.push(
            Command::new(env!("CARGO_BIN_EXE_subbar"))
                .args(["add", "codex", &format!("New-{i}")])
                .env("SUBBAR_DATA_DIR", &dir.0)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut writer in writers {
        assert!(writer.wait().unwrap().success());
    }
    let raw = fs::read_to_string(dir.0.join("state.json")).unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let accounts = state["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 12);
    assert!(!accounts.iter().any(|account| account["label"] == "Old"));
}

#[test]
fn old_claude_account_can_be_explicitly_linked_without_exposing_token() {
    let dir = TestDir::new();
    assert!(dir
        .cli(&["add", "claude", "Legacy", "accessToken=synthetic-secret"])
        .status
        .success());
    // Старая статистика должна сброситься — засеваем её, чтобы очистка была видна.
    let path = dir.0.join("state.json");
    let mut seeded: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    seeded["accounts"][0]["lastUsage"] = serde_json::json!({"status": "ok", "windows": [], "updatedAt": 1});
    fs::write(&path, serde_json::to_string(&seeded).unwrap()).unwrap();
    let linked = dir.cli(&["link-claude", "Legacy"]);
    assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
    assert!(!String::from_utf8_lossy(&linked.stdout).contains("synthetic-secret"));
    assert!(!String::from_utf8_lossy(&linked.stderr).contains("synthetic-secret"));
    let raw = fs::read_to_string(dir.0.join("state.json")).unwrap();
    let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(state["accounts"][0]["options"]["claudeCodeSource"], "true");
    assert!(state["accounts"][0]["lastUsage"].is_null());
    assert!(!dir.cli(&["link-claude", "Missing"]).status.success());
}

#[test]
fn cli_rejects_empty_required_key_and_never_echoes_malformed_secret() {
    let dir = TestDir::new();
    let empty = dir.cli(&["add", "opencode-go", "Synthetic", "apiKey="]);
    assert!(!empty.status.success());
    let malformed = dir.cli(&[
        "add",
        "opencode-go",
        "Synthetic",
        "synthetic-secret-without-equals",
    ]);
    assert!(!malformed.status.success());
    assert!(!String::from_utf8_lossy(&malformed.stderr).contains("synthetic-secret-without-equals"));
    let unknown = dir.cli(&[
        "add",
        "opencode-go",
        "Synthetic",
        "apiKey=synthetic-good",
        "unknown=synthetic-secret",
    ]);
    assert!(!unknown.status.success());
    assert!(!String::from_utf8_lossy(&unknown.stderr).contains("synthetic-secret"));
    let wrong_provider = dir.cli(&["add", "synthetic-secret-as-provider", "Synthetic"]);
    assert!(!wrong_provider.status.success());
    assert!(!String::from_utf8_lossy(&wrong_provider.stderr).contains("synthetic-secret"));
    assert!(!dir.0.join("state.json").exists());
}

#[test]
fn lock_failure_must_not_be_treated_as_empty_state() {
    let dir = TestDir::new();
    let not_a_directory = dir.0.join("file-not-dir");
    fs::write(&not_a_directory, "synthetic").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_subbar"))
        .arg("accounts")
        .env("SUBBAR_DATA_DIR", not_a_directory)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&result.stdout).contains("Список пуст"));
}
