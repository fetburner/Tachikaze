//! テスト前提に依存する単体テストで共有する補助関数。
//!
//! 統合テスト側の同じ関数は `tests/common/mod.rs` にある。

pub(crate) const REQUIRE_TEST_PREREQS_ENV: &str = "TACHIKAZE_REQUIRE_TEST_PREREQS";

/// `TACHIKAZE_REQUIRE_TEST_PREREQS` が `1` のときは理由をメッセージにして panic する。
/// それ以外は理由を `eprintln!` して `true` を返し、呼び出し元がテストをスキップする。
/// 統合テスト側の同じ関数は `tests/common/mod.rs` にある。
pub(crate) fn skip(reason: &str) -> bool {
    if matches!(std::env::var(REQUIRE_TEST_PREREQS_ENV), Ok(value) if value == "1") {
        panic!("{reason}");
    }
    eprintln!("{reason}");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Output};

    const PROBE_TEST_NAME: &str = "test_support::tests::skip_environment_probe";

    fn run_probe(value: Option<&str>) -> Output {
        let mut command = Command::new(std::env::current_exe().expect("test executable path"));
        command.args(["--exact", PROBE_TEST_NAME, "--ignored", "--nocapture"]);
        if let Some(value) = value {
            command.env(REQUIRE_TEST_PREREQS_ENV, value);
        } else {
            command.env_remove(REQUIRE_TEST_PREREQS_ENV);
        }
        command.output().expect("probe test process starts")
    }

    fn assert_probe_succeeded(output: Output) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "probe did not pass\nstdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn skip_panics_when_prerequisites_are_required() {
        assert_probe_succeeded(run_probe(Some("1")));
    }

    #[test]
    fn skip_returns_true_when_prerequisites_are_not_required() {
        assert_probe_succeeded(run_probe(None));
    }

    #[test]
    #[ignore]
    fn skip_environment_probe() {
        let reason = "fixture tests/fixtures/sample.mp4 is missing";
        let result = std::panic::catch_unwind(|| skip(reason));
        if matches!(std::env::var(REQUIRE_TEST_PREREQS_ENV), Ok(value) if value == "1") {
            let panic = result.expect_err("required prerequisites must panic");
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("");
            assert!(message.contains(reason), "panic message={message}");
        } else {
            assert!(result.expect("optional prerequisites must not panic"));
        }
    }
}
