use std::path::PathBuf;
use std::process::Command;

const SMCUP: &str = "\x1b[?1049h";

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nsql-it-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str], extra_env: &[(&str, &str)], home: &PathBuf) -> (String, String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nsql"));
    cmd.args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env_remove("NSQL_EDITOR")
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run nsql");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn execute_prints_result_and_no_altscreen() {
    let home = unique_dir("exec");
    let (stdout, stderr, ok) = run(
        &["-e", "select 7 as answer, 'hi' as g, null as n"],
        &[],
        &home,
    );
    assert!(ok, "nsql -e failed: {stderr}");
    assert!(stdout.contains('7'), "missing value 7 in: {stdout}");
    assert!(stdout.contains("answer"), "missing header in: {stdout}");
    assert!(!stdout.contains(SMCUP) && !stderr.contains(SMCUP), "emitted alt-screen escape!");
}

#[test]
fn json_output() {
    let home = unique_dir("json");
    let (stdout, stderr, ok) = run(&["--json", "-e", "select 1 as a, 'x' as b"], &[], &home);
    assert!(ok, "nsql --json failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v[0]["a"], 1);
    assert_eq!(v[0]["b"], "x");
}

#[test]
fn editor_loop_runs_saved_buffer() {
    let home = unique_dir("edit");

    let editor = home.join("fake-editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf 'select 42 as life;\\n' >> \"$1\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let (stdout, stderr, ok) = run(
        &["--edit"],
        &[("NSQL_EDITOR", editor.to_str().unwrap())],
        &home,
    );
    assert!(ok, "nsql --edit failed: {stderr}");
    assert!(stdout.contains("42"), "missing value 42 in: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("life"), "missing column in: {stdout}");
    assert!(!stdout.contains(SMCUP) && !stderr.contains(SMCUP), "emitted alt-screen escape!");
}

#[test]
fn editor_cancel_runs_nothing() {
    let home = unique_dir("cancel");

    let editor = home.join("cancel-editor.sh");
    std::fs::write(&editor, "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let (stdout, stderr, ok) = run(
        &["--edit"],
        &[("NSQL_EDITOR", editor.to_str().unwrap())],
        &home,
    );
    assert!(ok, "cancel should be a clean exit: {stderr}");
    assert!(stderr.contains("cancelled"), "expected a cancel notice: {stderr}");
    assert!(stdout.trim().is_empty(), "cancel should print no result: {stdout}");
}

#[test]
fn postgres_backend_when_available() {
    let Ok(url) = std::env::var("NSQL_TEST_PG_URL") else {
        eprintln!("skipping postgres test: set NSQL_TEST_PG_URL to run it");
        return;
    };
    let home = unique_dir("pg");

    let (_o, e, ok) = run(&["connect", "pg", "--url", &url], &[], &home);
    assert!(ok, "connect failed: {e}");

    let (stdout, stderr, ok) = run(
        &[
            "@pg",
            "--format",
            "table",
            "-e",
            "select 7 as answer, null as n, 'x' as s",
        ],
        &[],
        &home,
    );
    assert!(ok, "pg select failed: {stderr}");
    assert!(stdout.contains('7'), "missing value: {stdout}");
    assert!(stdout.contains("(null)"), "NULL not distinct: {stdout}");
    assert!(!stdout.contains(SMCUP) && !stderr.contains(SMCUP), "emitted alt-screen escape!");
}

#[test]
fn adhoc_url_runs_without_a_profile() {
    let home = unique_dir("adhoc");
    let db = home.join("adhoc.db");
    let url = format!("sqlite://{}", db.display());

    let (_o, e, ok) = run(
        &[&url, "-e", "create table t(x int); insert into t values (1),(2),(3)"],
        &[],
        &home,
    );
    assert!(ok, "ad-hoc DDL failed: {e}");

    let (stdout, stderr, ok) = run(&[&url, "--json", "-e", "select count(*) as n from t"], &[], &home);
    assert!(ok, "ad-hoc select failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(v[0]["n"], 3);
}

#[test]
fn readonly_profile_blocks_writes() {
    let home = unique_dir("ro");
    let (_o, e, ok) = run(
        &[
            "connect",
            "ro",
            "--url",
            "sqlite::memory:",
            "--readonly",
        ],
        &[],
        &home,
    );
    assert!(ok, "connect failed: {e}");

    let (_stdout, stderr, ok) = run(
        &["@ro", "-e", "create table t(x int)"],
        &[],
        &home,
    );
    assert!(!ok, "read-only profile should reject a write");
    assert!(stderr.contains("read-only"), "unexpected error: {stderr}");
}
