//! `cut --segment-map`（issue #57）の E2E。
//!
//! `tachikaze cut` は配線済みなので `CARGO_BIN_EXE_tachikaze` を起動する形の E2E にする
//! （`tests/audio_e2e.rs` / `tests/video_e2e.rs` の `--cm-output` テストと同じ方式）。
//! JSON の読み戻しは `src/segmap.rs` が書き出し専用（依存を増やさない判断、同ファイルの
//! doc comment参照）なので、この程度の固定スキーマなら文字列探索で十分という判断で
//! 本ファイル内に最小限のヘルパを用意する（汎用 JSON パーサは持たない）。

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// フィクスチャ（GOP=120・599フレーム）に対する Trim リスト。他の E2E テスト
/// （`tests/video_e2e.rs` / `tests/audio_e2e.rs`）と同じ値。`Snap::Outward`（既定）で
/// `[10,110)` は `[0,120)` へ、`[370,470)` は `[360,480)` へ広がる。
const TRIM_AVS_CONTENT: &str = "Trim(10,109) ++ Trim(370,469)";

fn dtvi_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.dtvi")
}

fn tools_available() -> bool {
    for bin in ["ffmpeg", "ffprobe"] {
        match Command::new(bin).arg("-version").output() {
            Ok(output) if output.status.success() => {}
            _ => {
                eprintln!("{bin} が無いためスキップします。");
                return false;
            }
        }
    }
    true
}

fn make_tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tachikaze-segmap-e2e-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("一時ディレクトリを作れること");
    dir
}

/// `path` の映像ストリームの表示順(pts昇順)の `pts`(整数、timebase単位)を取得する。
/// `tests/video_e2e.rs::frame_pts_in_display_order` と同じ手法。
fn frame_pts_in_display_order(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe を起動できること");
    assert!(
        output.status.success(),
        "ffprobe (frame pts) が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("ffprobe の出力が utf-8 であること")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let first_field = line.split(',').next().unwrap_or(line);
            first_field
                .parse::<i64>()
                .unwrap_or_else(|e| panic!("pts が整数であること: {first_field:?} ({e})"))
        })
        .collect()
}

// ---------------------------------------------------------------------
// `src/segmap.rs` が書き出す JSON を読み戻す最小限のヘルパ。
// 汎用 JSON パーサではなく、このプロジェクトが書き出すフォーマット（固定キー、
// 数値はそのまま・文字列は素朴なエスケープのみ）専用。
// ---------------------------------------------------------------------

/// `"key": <number>` という並びを最初から順にすべて拾う。ヘッダのキー
/// （`video_timescale` 等）は1回、区間ごとのキー（`frame_count` 等）は区間数ぶん出てくる。
fn json_numbers(json: &str, key: &str) -> Vec<u64> {
    let needle = format!("\"{key}\": ");
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(idx) = json[pos..].find(&needle) {
        let start = pos + idx + needle.len();
        let rest = &json[start..];
        let end = rest
            .find([',', '\n'])
            .unwrap_or_else(|| panic!("{key} の値の終端が見つからない: {rest:?}"));
        let value = rest[..end].trim();
        out.push(
            value
                .parse::<u64>()
                .unwrap_or_else(|e| panic!("{key}={value:?} が数値でない: {e}")),
        );
        pos = start + end;
    }
    out
}

fn json_number(json: &str, key: &str) -> u64 {
    let values = json_numbers(json, key);
    assert_eq!(values.len(), 1, "{key} は1回だけ出てくるはず: {values:?}");
    values[0]
}

fn json_string(json: &str, key: &str) -> String {
    let needle = format!("\"{key}\": \"");
    let start = json
        .find(&needle)
        .unwrap_or_else(|| panic!("{key} が見つからない"))
        + needle.len();
    let end = json[start..]
        .find('"')
        .unwrap_or_else(|| panic!("{key} の終端の引用符が見つからない"));
    json[start..start + end].to_string()
}

/// `tachikaze cut` を起動する。`dtvi` を差し替えられるのは検査4の失敗を再現する
/// テストのため。`with_segment_map` が `true` なら `tmp_dir/seg.json` を
/// `--segment-map` に渡す。
fn run_cut(
    label: &str,
    trim_content: &str,
    dtvi: &Path,
    cache_root: &Path,
    with_segment_map: bool,
) -> (PathBuf, PathBuf, Option<PathBuf>, std::process::Output) {
    let fixture = common::fixture_path();
    let tmp_dir = make_tmp_dir(label);
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    std::fs::write(&trim_path, trim_content).expect("trim.avs を書けること");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
    cmd.arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--dtvi")
        .arg(dtvi)
        // 既定のキャッシュ書き込みが実行者の実際の `~/.cache/tachikaze` を汚さない
        // ようにする（`Command::env` は子プロセスの環境だけに効く。テストプロセス側の
        // 環境変数は書き換えないので、他のテストと競合しない）。
        .env("TACHIKAZE_CACHE_DIR", cache_root);

    let segment_map_path = if with_segment_map {
        let path = tmp_dir.join("seg.json");
        cmd.arg("--segment-map").arg(&path);
        Some(path)
    } else {
        None
    };

    let output = cmd.output().expect("tachikaze cut の起動に失敗した");
    (tmp_dir, out_path, segment_map_path, output)
}

/// 完了条件:
/// - `output_start` の累積が、実出力の ffprobe 上の区間境界(pts)と一致する
/// - `source_start_frame` / `source_end_frame` / `frame_count` が snap 後の区間と一致する
/// - `--segment-map PATH` で任意の場所に書ける
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn segment_map_output_start_matches_ffprobe_frame_boundaries() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !tools_available() {
        return;
    }

    let cache_root = make_tmp_dir("happy-cache");
    let (tmp_dir, out_path, segmap_path, output) =
        run_cut("happy", TRIM_AVS_CONTENT, &dtvi_path(), &cache_root, true);
    let segmap_path = segmap_path.expect("with_segment_map=true のはず");

    assert!(
        output.status.success(),
        "tachikaze cut --segment-map が失敗した: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json = std::fs::read_to_string(&segmap_path).expect("区間マップを読めること");

    // --- ヘッダ ---
    assert_eq!(json_number(&json, "video_timescale"), 30000);
    assert_eq!(json_number(&json, "frame_rate_num"), 30000);
    assert_eq!(json_number(&json, "frame_rate_den"), 1001);
    assert_eq!(json_number(&json, "total_frames"), 599);
    let input = json_string(&json, "input");
    assert!(
        Path::new(&input).is_absolute(),
        "input は絶対パスのはず: {input}"
    );
    assert!(
        input.ends_with("sample.mp4"),
        "input はフィクスチャを指すはず: {input}"
    );

    // --- 区間 ---
    let start_frames = json_numbers(&json, "source_start_frame");
    let end_frames = json_numbers(&json, "source_end_frame");
    let frame_counts = json_numbers(&json, "frame_count");
    let output_starts = json_numbers(&json, "output_start");
    let durations = json_numbers(&json, "duration");
    let source_start_dts = json_numbers(&json, "source_start_dts");

    assert_eq!(start_frames, vec![0, 360]);
    assert_eq!(end_frames, vec![120, 480]);
    assert_eq!(frame_counts, vec![120, 120]);
    // 完了条件: 区間数とframe_countの合計が保持側の総パケット数(240)と一致する。
    assert_eq!(frame_counts.iter().sum::<u64>(), 240);

    // source_start_dts は DTS(罠4: 合成時刻/PTSではない)。区間1の開始はファイル
    // 先頭なので DTS=0。区間2の開始(表示順360)はキーフレームで、このフィクスチャは
    // GOP=120なのでDTSも表示順どおりに揃う(open GOPではない)。
    assert_eq!(source_start_dts[0], 0);

    // --- output_start の累積が実出力の区間境界(ffprobe pts)と一致する ---
    assert_eq!(output_starts[0], 0);
    assert_eq!(output_starts[1], durations[0]);

    // 差分(delta)で比較する: `write.rs` は各サンプルの `ctts`(合成時刻オフセット)を
    // ソースの値のまま引き継ぐため(`crate::commands::segment_video_source_starts` の
    // doc comment、CLAUDE.md 罠4)、ffprobe の pts には出力全体に一律で乗る定数オフセット
    // （このフィクスチャでは実測 +2002 = Bフレーム並べ替え2枚分）が残る。したがって
    // pts の**絶対値**を output_start と直接比較してはいけない。一方 `output_start`
    // は区間ごとの `duration`(再生時間)の累積であり、この一律オフセットは差分を取れば
    // 消える。「区間2の先頭フレームのpts」-「区間1の先頭フレームのpts」が
    // 区間1の再生時間(=区間2のoutput_start)と一致することを見れば、オフセットの影響を
    // 受けずに snap 後の区間境界が実出力に正しく反映されていることを確認できる。
    let output_pts = frame_pts_in_display_order(&out_path);
    assert_eq!(output_pts.len(), 240);
    let pts_delta = output_pts[120] - output_pts[0];
    assert_eq!(
        pts_delta as u64, output_starts[1],
        "区間2の output_start が実出力のpts差分と一致しない(定数オフセットは打ち消した差分で比較)"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// 完了条件: `--segment-map` 未指定でも、キャッシュ（`work.mp4.segmap.json`）に
/// 区間マップが残る。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn segment_map_is_written_to_default_cache_without_explicit_flag() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !tools_available() {
        return;
    }

    let cache_root = make_tmp_dir("default-cache");
    let (tmp_dir, _out_path, _segmap_path, output) = run_cut(
        "default-cache-run",
        TRIM_AVS_CONTENT,
        &dtvi_path(),
        &cache_root,
        false,
    );

    assert!(
        output.status.success(),
        "tachikaze cut が失敗した: status={:?}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // `cached_segment_map_path` と同じ規則(`<cache_root>/<入力ハッシュ>-<stem>/
    // work.mp4.segmap.json`)を自前で辿るのではなく、キャッシュルート配下を再帰的に
    // 探す(ハッシュ計算はキャッシュパス規則の実装詳細で、テストが重複して知る必要は
    // ない)。
    let found = find_file_recursively(&cache_root, "work.mp4.segmap.json");
    assert!(
        found.is_some(),
        "既定のキャッシュに work.mp4.segmap.json が見当たらない(cache_root={})",
        cache_root.display()
    );

    let json = std::fs::read_to_string(found.unwrap()).expect("キャッシュのマップを読めること");
    assert_eq!(json_numbers(&json, "frame_count"), vec![120, 120]);

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

fn find_file_recursively(dir: &Path, file_name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursively(&path, file_name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(file_name) {
            return Some(path);
        }
    }
    None
}

/// `tests/data/sample.dtvi` の `FRAMES` テーブルのうち frame_number=5 と 6 の行の
/// `sample_number` 列を入れ替えた内容を返す。
///
/// `src/verify.rs` の `cut_and_verify_discards_output_when_dtvi_row_is_corrupted` は
/// パース済みの `Dtvi` を直接壊す（`f.dtvi.frames[5].sample_number = DecodeIdx(9999)`）
/// が、こちらはテキストの `.dtvi` をそのまま `--dtvi` に渡す E2E なので、
/// `dtvi::parse` 自身の「sample_number 列はフレーム数分の順列になっているはず」という
/// 検証（範囲外・重複チェック）を通してから検査4（元ファイルとの突き合わせ）に到達
/// させる必要がある。1個を範囲外の値にすると parse 自体が落ちてしまうため、2行の
/// sample_number を入れ替える（順列としては妥当なまま、値だけが実際の mp4 と食い違う）。
fn corrupted_dtvi_content() -> String {
    let original = std::fs::read_to_string(dtvi_path()).expect("sample.dtvi を読めること");
    let lines: Vec<&str> = original.lines().collect();
    let frames_idx = lines
        .iter()
        .position(|l| *l == "FRAMES")
        .expect("FRAMES マーカーが見つかること");
    let idx_a = frames_idx + 1 + 5; // frame_number=5 の行
    let idx_b = frames_idx + 1 + 6; // frame_number=6 の行

    let mut fields_a: Vec<String> = lines[idx_a].split('\t').map(str::to_string).collect();
    let mut fields_b: Vec<String> = lines[idx_b].split('\t').map(str::to_string).collect();
    assert_eq!(fields_a[0], "5", "frame_number=5 の行のはず: {fields_a:?}");
    assert_eq!(fields_b[0], "6", "frame_number=6 の行のはず: {fields_b:?}");
    assert_ne!(
        fields_a[1], fields_b[1],
        "sample_number が同じだと入れ替えても壊れないので前提が崩れている"
    );
    std::mem::swap(&mut fields_a[1], &mut fields_b[1]);

    let mut new_lines = lines.clone();
    let joined_a = fields_a.join("\t");
    let joined_b = fields_b.join("\t");
    new_lines[idx_a] = &joined_a;
    new_lines[idx_b] = &joined_b;

    let mut out = new_lines.join("\n");
    out.push('\n');
    out
}

/// 完了条件: 自己検証（検査4）に失敗した場合、保持側の mp4 が残らないのと同様、
/// 区間マップ（明示パス・キャッシュのどちらにも）も残らない。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn segment_map_is_not_written_when_self_verification_fails() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !tools_available() {
        return;
    }

    let tmp_dir = make_tmp_dir("check4-fail");
    let bad_dtvi_path = tmp_dir.join("corrupted.dtvi");
    std::fs::write(&bad_dtvi_path, corrupted_dtvi_content()).expect("壊した .dtvi を書けること");

    let cache_root = make_tmp_dir("check4-fail-cache");

    let (tmp_dir2, out_path, segmap_path, output) = run_cut(
        "check4-fail-run",
        TRIM_AVS_CONTENT,
        &bad_dtvi_path,
        &cache_root,
        true,
    );
    let segmap_path = segmap_path.expect("with_segment_map=true のはず");

    assert!(
        !output.status.success(),
        "壊れた .dtvi では cut が失敗するはず"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("検査4"),
        "検査4(.dtvi突き合わせ)の失敗であること: {stderr}"
    );
    assert!(!out_path.exists(), "保持側の mp4 が残っていないこと");
    assert!(
        !segmap_path.exists(),
        "明示パスに区間マップが残っていないこと"
    );
    assert!(
        find_file_recursively(&cache_root, "work.mp4.segmap.json").is_none(),
        "キャッシュにも区間マップが残っていないこと"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&tmp_dir2);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// 完了条件（レビュー指摘#4）: 一度成功した cut がキャッシュへ区間マップを書いた後、
/// 同じ入力・同じキャッシュに対して自己検証が失敗する cut を実行すると、古い
/// マップが削除され、失敗後はキャッシュに区間マップが残らない。`remap-subs` が
/// 古いマップを鮮度チェックなしに使ってしまう事故を防ぐための検査
/// （`src/commands.rs::clear_stale_cached_segment_map`）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn stale_segment_map_is_removed_after_a_later_failed_cut() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !tools_available() {
        return;
    }

    let cache_root = make_tmp_dir("stale-map-cache");

    // 1回目: 正常な .dtvi で成功させ、キャッシュに区間マップを残す。
    let (tmp_dir1, _out_path1, _segmap_path1, output1) = run_cut(
        "stale-map-first",
        TRIM_AVS_CONTENT,
        &dtvi_path(),
        &cache_root,
        false,
    );
    assert!(
        output1.status.success(),
        "1回目の cut は成功するはず: stderr={}",
        String::from_utf8_lossy(&output1.stderr)
    );
    assert!(
        find_file_recursively(&cache_root, "work.mp4.segmap.json").is_some(),
        "1回目成功後はキャッシュに区間マップが残っているはず"
    );

    // 2回目: 同じ入力・同じキャッシュに対して、壊れた .dtvi で自己検証を失敗させる。
    let bad_dtvi_dir = make_tmp_dir("stale-map-bad-dtvi");
    let bad_dtvi_path = bad_dtvi_dir.join("corrupted.dtvi");
    std::fs::write(&bad_dtvi_path, corrupted_dtvi_content()).expect("壊した .dtvi を書けること");

    let (tmp_dir2, _out_path2, _segmap_path2, output2) = run_cut(
        "stale-map-second",
        TRIM_AVS_CONTENT,
        &bad_dtvi_path,
        &cache_root,
        false,
    );
    assert!(
        !output2.status.success(),
        "2回目の cut は壊れた .dtvi で失敗するはず"
    );

    assert!(
        find_file_recursively(&cache_root, "work.mp4.segmap.json").is_none(),
        "失敗後は古い区間マップがキャッシュから消えているはず"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir1);
    let _ = std::fs::remove_dir_all(&tmp_dir2);
    let _ = std::fs::remove_dir_all(&bad_dtvi_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// 完了条件: `--cm-output` 指定時、CM側の区間マップは作られない（保持側だけ出す）。
/// 保持側のマップの中身が「保持区間」（[0,120)+[360,480)）であって、CM側の補集合
/// （[120,360)+[480,599)）ではないことも確認する。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn segment_map_with_cm_output_only_covers_kept_side() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !tools_available() {
        return;
    }

    let fixture = common::fixture_path();
    let tmp_dir = make_tmp_dir("cm-output");
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    let cm_path = tmp_dir.join("cm.mp4");
    let segmap_path = tmp_dir.join("seg.json");
    std::fs::write(&trim_path, TRIM_AVS_CONTENT).expect("trim.avs を書けること");

    let cache_root = make_tmp_dir("cm-output-cache");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--dtvi")
        .arg(dtvi_path())
        .arg("--cm-output")
        .arg(&cm_path)
        .arg("--segment-map")
        .arg(&segmap_path)
        .env("TACHIKAZE_CACHE_DIR", &cache_root)
        .output()
        .expect("tachikaze cut の起動に失敗した");

    assert!(
        output.status.success(),
        "tachikaze cut --cm-output --segment-map が失敗した: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(segmap_path.is_file(), "保持側の区間マップは作られるはず");
    let json = std::fs::read_to_string(&segmap_path).expect("区間マップを読めること");
    // 保持側([0,120)+[360,480))であって、CM側の補集合([120,360)+[480,599))ではない。
    assert_eq!(json_numbers(&json, "source_start_frame"), vec![0, 360]);
    assert_eq!(json_numbers(&json, "source_end_frame"), vec![120, 480]);
    assert_eq!(json_numbers(&json, "frame_count"), vec![120, 120]);

    // キャッシュにも保持側のマップだけが1つ残り、CM側用の別ファイルは存在しない。
    let cached = find_all_files_recursively(&cache_root, "segmap.json");
    assert_eq!(
        cached.len(),
        1,
        "キャッシュに区間マップが複数(CM側の分も)残っている: {cached:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

fn find_all_files_recursively(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(find_all_files_recursively(&path, suffix));
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
            {
                out.push(path);
            }
        }
    }
    out
}

/// フィクスチャ/ツールが無い環境でもテストの意図が読めるようにするプレースホルダ。
#[test]
fn segmap_e2e_module_compiles_and_helpers_are_reachable() {
    let _ = tools_available;
    let _ = json_numbers;
    let _ = json_number;
    let _ = json_string;
    let _ = frame_pts_in_display_order;
    let _ = corrupted_dtvi_content;
}
