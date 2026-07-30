#!/usr/bin/env bash
# tachikaze の analyze → cut を機械的に流すラッパー。
#
# 設計方針:
#   - CLI に `auto` を足さない（docs/architecture.md）。検出の見逃しがあるため、
#     既定では analyze 後に人間の確認を挟む。`--yes` で省略可。
#   - パス結線・edit list 除去・出力名決め・--cm-output 付与など、毎回同じで
#     判断が要らない手順だけを自動化する。
#   - `-o` と `--work-dir` 内の trim.avs を同じパスにしない
#     （同一パスへの copy で空ファイルになることがある）。
#
# 使い方:
#   scripts/cmcut.sh IN.mp4
#   scripts/cmcut.sh --yes IN.mp4 [IN2.mp4 ...]
#   scripts/cmcut.sh --analyze-only IN.mp4
#   scripts/cmcut.sh --cut-only IN.mp4          # 既存 work-dir / trim を使う
#   scripts/cmcut.sh -o OUT.mp4 IN.mp4
#   scripts/cmcut.sh --work-dir DIR --yes IN.mp4

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# --- defaults ---------------------------------------------------------------

TACHIKAZE="${TACHIKAZE:-}"
TOOL_DIR="${TOOL_DIR:-${REPO_ROOT}/tools}"
WORK_ROOT="${WORK_ROOT:-${REPO_ROOT}/work}"
MODE="all"          # all | analyze-only | cut-only
AUTO_YES=0
NO_CM=0
VERIFY=0
SNAP="outward"
OUT_PATH=""         # empty = derive from input
CM_OUT_PATH=""      # empty = derive (unless --no-cm)
WORK_DIR_OPT=""     # empty = per-input under WORK_ROOT
JL_FILE=""
JLS_SETS=()
KEEP_STRIPPED=0

usage() {
  cat <<'EOF'
Usage: cmcut.sh [options] IN.mp4 [IN2.mp4 ...]

Options:
  -y, --yes              analyze 後の確認を省略して cut まで進む
  --analyze-only         検出と report だけ（cut しない）
  --cut-only             既存の work-dir / trim.avs で cut だけ
  -o, --output PATH      本編出力（単一入力時のみ）
  --cm-output PATH       CM 側出力（単一入力時のみ）
  --no-cm                CM 側ファイルを出さない
  --work-dir DIR         中間ファイル置き場（単一入力時のみ）
  --work-root DIR        複数入力時の work 親ディレクトリ (default: <repo>/work)
  --jl-file FILE         join_logo_scp の JL コマンドファイル
  --jls-set KEY=VALUE    join_logo_scp の -set（繰り返し可）
  --snap outward|inward  キーフレーム丸め方向 (default: outward)
  --verify               cut に --verify を付ける
  --keep-stripped        elst 除去後の中間 mp4 を消さない
  --tachikaze PATH       tachikaze バイナリ
  --tool-dir DIR         外部ツールディレクトリ (default: <repo>/tools)
  -h, --help             このヘルプ

Environment:
  TACHIKAZE, TOOL_DIR, WORK_ROOT  上記と同義

Typical:
  scripts/cmcut.sh ~/Downloads/録画.mp4
  scripts/cmcut.sh --yes ~/Downloads/*.mp4
  # analyze 結果を直したあと:
  scripts/cmcut.sh --cut-only --work-dir work/cmcut_xxx ~/Downloads/録画.mp4
EOF
}

# log は stderr へ（$(...) で返す関数の stdout を汚さない）
log()  { printf '==> %s\n' "$*" >&2; }
warn() { printf '警告: %s\n' "$*" >&2; }
die()  { printf 'エラー: %s\n' "$*" >&2; exit 1; }

# --- arg parse --------------------------------------------------------------

INPUTS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    -y|--yes) AUTO_YES=1; shift ;;
    --analyze-only) MODE="analyze-only"; shift ;;
    --cut-only) MODE="cut-only"; shift ;;
    -o|--output) OUT_PATH="${2:-}"; shift 2 ;;
    --cm-output) CM_OUT_PATH="${2:-}"; shift 2 ;;
    --no-cm) NO_CM=1; shift ;;
    --work-dir) WORK_DIR_OPT="${2:-}"; shift 2 ;;
    --work-root) WORK_ROOT="${2:-}"; shift 2 ;;
    --jl-file) JL_FILE="${2:-}"; shift 2 ;;
    --jls-set) JLS_SETS+=("${2:-}"); shift 2 ;;
    --snap) SNAP="${2:-}"; shift 2 ;;
    --verify) VERIFY=1; shift ;;
    --keep-stripped) KEEP_STRIPPED=1; shift ;;
    --tachikaze) TACHIKAZE="${2:-}"; shift 2 ;;
    --tool-dir) TOOL_DIR="${2:-}"; shift 2 ;;
    --) shift; INPUTS+=("$@"); break ;;
    -*) die "不明なオプション: $1（--help を参照）" ;;
    *) INPUTS+=("$1"); shift ;;
  esac
done

[[ ${#INPUTS[@]} -gt 0 ]] || { usage >&2; die "入力 mp4 を指定してください"; }

if [[ ${#INPUTS[@]} -gt 1 ]]; then
  [[ -z "$OUT_PATH" ]] || die "複数入力時は -o を使えません（各入力の隣に出力します）"
  [[ -z "$CM_OUT_PATH" ]] || die "複数入力時は --cm-output を使えません"
  [[ -z "$WORK_DIR_OPT" ]] || die "複数入力時は --work-dir を使えません（--work-root を使ってください）"
fi

[[ "$SNAP" == "outward" || "$SNAP" == "inward" ]] || die "--snap は outward か inward"

if [[ "$NO_CM" -eq 1 && "$SNAP" == "inward" ]]; then
  : # ok
fi
if [[ "$NO_CM" -eq 0 && "$SNAP" == "inward" ]]; then
  die "--snap inward は --cm-output と併用不可です。--no-cm を付けるか outward にしてください"
fi

# --- resolve tools ----------------------------------------------------------

resolve_tachikaze() {
  if [[ -n "$TACHIKAZE" ]]; then
    [[ -x "$TACHIKAZE" ]] || die "tachikaze が実行できません: $TACHIKAZE"
    return
  fi
  local candidates=(
    "${REPO_ROOT}/target/release/tachikaze"
    "${REPO_ROOT}/target/debug/tachikaze"
  )
  local c
  for c in "${candidates[@]}"; do
    if [[ -x "$c" ]]; then
      TACHIKAZE="$c"
      return
    fi
  done
  if command -v tachikaze >/dev/null 2>&1; then
    TACHIKAZE="$(command -v tachikaze)"
    return
  fi
  die "tachikaze が見つかりません。cargo build --release するか --tachikaze で指定してください"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "$1 が PATH にありません"
}

resolve_tachikaze
need_cmd ffmpeg
need_cmd ffprobe
need_cmd python3

[[ -d "$TOOL_DIR" ]] || die "tool-dir がありません: $TOOL_DIR"
[[ -x "${TOOL_DIR}/dtvindex" ]] || die "dtvindex がありません: ${TOOL_DIR}/dtvindex"
[[ -x "${TOOL_DIR}/chapter_exe" ]] || die "chapter_exe がありません: ${TOOL_DIR}/chapter_exe"
[[ -x "${TOOL_DIR}/join_logo_scp" ]] || die "join_logo_scp がありません: ${TOOL_DIR}/join_logo_scp"

log "tachikaze: $TACHIKAZE"
log "tool-dir:  $TOOL_DIR"

# --- helpers ----------------------------------------------------------------

# 作業ディレクトリ名用にファイル名を安全化（拡張子なし）
safe_stem() {
  local base
  base="$(basename "$1")"
  base="${base%.*}"
  # 空白・制御文字などを _ に。日本語はそのまま残す。
  printf '%s' "$base" | python3 -c '
import re, sys
s = sys.stdin.read()
s = re.sub(r"[\s/\\:\0]+", "_", s)
s = s.strip("._") or "input"
print(s[:120])
'
}

# mp4 に elst があるか（ストリーミングで box を辿る）
has_elst() {
  python3 - "$1" <<'PY'
import struct, sys
path = sys.argv[1]
CONTAINERS = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"udta", b"mvex"}

def walk(f, end):
    while f.tell() + 8 <= end:
        start = f.tell()
        header = f.read(8)
        if len(header) < 8:
            return False
        size, typ = struct.unpack(">I4s", header)
        hdr = 8
        if size == 1:
            wide = f.read(8)
            if len(wide) < 8:
                return False
            size = struct.unpack(">Q", wide)[0]
            hdr = 16
        elif size == 0:
            size = end - start
        if size < hdr:
            return False
        box_end = start + size
        if box_end > end:
            return False
        if typ == b"elst":
            return True
        if typ in CONTAINERS:
            if walk(f, box_end):
                return True
        f.seek(box_end)
    return False

with open(path, "rb") as f:
    f.seek(0, 2)
    end = f.tell()
    f.seek(0)
    sys.exit(0 if walk(f, end) else 1)
PY
}

# elst があれば copy remux で除去したパスを返す。無ければ入力をそのまま。
prepare_input() {
  local src="$1"
  local work="$2"
  local stripped="${work}/input_noelst.mp4"

  if has_elst "$src"; then
    log "edit list (elst) を検出 → ロスレス除去: $stripped"
    ffmpeg -hide_banner -loglevel error -y \
      -i "$src" -c copy -use_editlist 0 -movflags +faststart \
      "$stripped"
    printf '%s' "$stripped"
  else
    log "edit list なし（前処理不要）"
    printf '%s' "$src"
  fi
}

default_out_path() {
  local src="$1"
  local dir base stem
  dir="$(dirname "$src")"
  base="$(basename "$src")"
  stem="${base%.*}"
  printf '%s/%s_CMcut.mp4' "$dir" "$stem"
}

default_cm_path() {
  local src="$1"
  local dir base stem
  dir="$(dirname "$src")"
  base="$(basename "$src")"
  stem="${base%.*}"
  printf '%s/%s_CM.mp4' "$dir" "$stem"
}

print_detail_summary() {
  local jls="$1"
  [[ -f "$jls" ]] || return 0
  echo
  echo "--- detail.jls（ラベル要約）---"
  # 秒数とラベルだけ抜き出して見やすくする
  python3 - "$jls" <<'PY'
import sys
from collections import Counter
path = sys.argv[1]
rows = []
with open(path, encoding="utf-8", errors="replace") as f:
    for line in f:
        parts = line.split()
        if len(parts) < 6:
            continue
        try:
            start, end, sec = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        label = parts[5]
        rows.append((start, end, sec, label))
if not rows:
    print("(空)")
    sys.exit(0)
ctr = Counter(r[3] for r in rows)
print("ラベル内訳:", ", ".join(f"{k}×{v}" for k, v in ctr.most_common()))
print()
for start, end, sec, label in rows:
    flag = ""
    if label.startswith(":CM") or label == ":Nologo":
        flag = "  ← cut 候補"
    elif "(add)" in label:
        flag = "  ← 残す方針"
    print(f"  {start:6d}-{end:<6d} {sec:4d}s  {label}{flag}")
PY
}

# work-dir 内の人手編集用 trim を解決する（優先順位つき）
resolve_trim() {
  local work="$1"
  local c
  for c in "${work}/user_trim.avs" "${work}/final_trim.avs" "${work}/trim.avs"; do
    if [[ -s "$c" ]]; then
      printf '%s' "$c"
      return 0
    fi
  done
  return 1
}

confirm_continue() {
  local trim="$1"
  [[ -s "$trim" ]] || die "trim が空、またはありません: $trim"
  echo
  echo "--- trim ($(basename "$trim")) ---"
  cat "$trim"
  echo
  if [[ "$AUTO_YES" -eq 1 ]]; then
    log "--yes 指定のため確認を省略して cut に進みます"
    return 0
  fi
  if [[ ! -t 0 ]]; then
    die "対話端末ではないため確認できません。--yes を付けるか、ターミナルから実行してください"
  fi
  echo "内容を確認してください。"
  echo "  - 直す場合: ${trim} を編集してから Enter"
  echo "  - このまま cut: Enter"
  echo "  - 中止: q + Enter"
  local ans
  read -r -p "> " ans || true
  case "${ans:-}" in
    q|Q|quit|exit) die "中止しました" ;;
  esac
}

run_analyze() {
  local src="$1"
  local work="$2"
  local trim_out="${work}/trim.avs"   # work 内の join_logo_scp 出力とは別名にしないが、
  # analyze は work_dir/trim.avs に書いてから -o へコピーする。
  # -o を work_dir/final_trim.avs にして同一パス衝突を避ける。
  local trim_final="${work}/final_trim.avs"

  mkdir -p "$work"

  local prepared
  prepared="$(prepare_input "$src" "$work")"

  local analyze_args=(
    --tool-dir "$TOOL_DIR"
    analyze "$prepared"
    -o "$trim_final"
    --work-dir "$work"
    --report
  )
  if [[ -n "$JL_FILE" ]]; then
    analyze_args+=(--jl-file "$JL_FILE")
  fi
  local s
  for s in "${JLS_SETS[@]+"${JLS_SETS[@]}"}"; do
    analyze_args+=(--jls-set "$s")
  done

  log "analyze: $prepared"
  "$TACHIKAZE" "${analyze_args[@]}"

  # 以降の cut / 人手編集用に final_trim.avs を trim.avs としても参照しやすくする
  # （work_dir 内の中間 trim.avs と同じ内容。final を正とする）
  cp -f "$trim_final" "${work}/user_trim.avs"

  # 入力元パスを work に記録（--cut-only 用）
  printf '%s\n' "$src" >"${work}/source.path"
  printf '%s\n' "$prepared" >"${work}/prepared.path"

  print_detail_summary "${work}/detail.jls"
  echo
  log "trim:     ${work}/user_trim.avs"
  log "dtvi:     ${work}/work.mp4.dtvi"
  log "detail:   ${work}/detail.jls"
  log "report は上に出力済み。見逃し候補があれば警告が出ています。"
}

run_cut() {
  local src="$1"
  local work="$2"
  local out="$3"
  local cm_out="$4"

  local prepared
  if [[ -f "${work}/prepared.path" ]]; then
    prepared="$(cat "${work}/prepared.path")"
  else
    prepared="$(prepare_input "$src" "$work")"
    printf '%s\n' "$prepared" >"${work}/prepared.path"
  fi
  [[ -f "$prepared" ]] || die "前処理済み入力がありません: $prepared"

  local trim
  trim="$(resolve_trim "$work")" || die "trim がありません。先に analyze を実行してください: $work"

  local dtvi="${work}/work.mp4.dtvi"
  [[ -f "$dtvi" ]] || die ".dtvi がありません: $dtvi（analyze を --work-dir 付きで実行してください）"

  local cut_args=(
    --tool-dir "$TOOL_DIR"
    cut "$prepared"
    --trim "$trim"
    --dtvi "$dtvi"
    -o "$out"
    --snap "$SNAP"
  )
  if [[ "$NO_CM" -eq 0 && -n "$cm_out" ]]; then
    cut_args+=(--cm-output "$cm_out")
  fi
  if [[ "$VERIFY" -eq 1 ]]; then
    cut_args+=(--verify)
  fi

  log "cut: $prepared"
  log "  trim → $trim"
  log "  out  → $out"
  if [[ "$NO_CM" -eq 0 && -n "$cm_out" ]]; then
    log "  cm   → $cm_out"
  fi

  "$TACHIKAZE" "${cut_args[@]}"

  if [[ "$KEEP_STRIPPED" -eq 0 && -f "${work}/input_noelst.mp4" ]]; then
    # 元が別パスなら中間を消してディスクを空ける
    if [[ "$prepared" == "${work}/input_noelst.mp4" ]]; then
      rm -f "${work}/input_noelst.mp4"
      log "中間 input_noelst.mp4 を削除しました（--keep-stripped で残せる）"
    fi
  fi

  echo
  log "完了: $out"
  if [[ "$NO_CM" -eq 0 && -n "$cm_out" && -f "$cm_out" ]]; then
    log "CM側: $cm_out  （本編が混ざっていないか目視推奨）"
  fi
  if command -v ffprobe >/dev/null 2>&1; then
    local dur
    dur="$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$out" 2>/dev/null || true)"
    if [[ -n "$dur" ]]; then
      python3 -c "d=float('${dur}'); print(f'  本編尺: {int(d//60)}分{d%60:04.1f}秒')"
    fi
  fi
}

process_one() {
  local src="$1"
  local out_override="${2:-}"
  local cm_override="${3:-}"
  local work_override="${4:-}"

  [[ -f "$src" ]] || die "入力がありません: $src"
  src="$(cd "$(dirname "$src")" && pwd)/$(basename "$src")"

  local work
  if [[ -n "$work_override" ]]; then
    work="$work_override"
  else
    work="${WORK_ROOT}/cmcut_$(safe_stem "$src")"
  fi
  mkdir -p "$work"

  local out cm
  out="${out_override:-$(default_out_path "$src")}"
  if [[ "$NO_CM" -eq 1 ]]; then
    cm=""
  else
    cm="${cm_override:-$(default_cm_path "$src")}"
  fi

  echo
  log "======== $(basename "$src") ========"
  log "work: $work"

  case "$MODE" in
    analyze-only)
      run_analyze "$src" "$work"
      log "analyze-only のためここで終了。cut するときは:"
      echo "  $0 --cut-only --work-dir $(printf '%q' "$work") $(printf '%q' "$src")"
      ;;
    cut-only)
      local trim
      trim="$(resolve_trim "$work")" || die "trim がありません。先に analyze を実行してください: $work"
      confirm_continue "$trim"
      run_cut "$src" "$work" "$out" "$cm"
      ;;
    all)
      run_analyze "$src" "$work"
      confirm_continue "${work}/user_trim.avs"
      run_cut "$src" "$work" "$out" "$cm"
      ;;
    *) die "内部エラー: MODE=$MODE" ;;
  esac
}

# --- main -------------------------------------------------------------------

if [[ ${#INPUTS[@]} -eq 1 ]]; then
  process_one "${INPUTS[0]}" "$OUT_PATH" "$CM_OUT_PATH" "$WORK_DIR_OPT"
else
  for src in "${INPUTS[@]}"; do
    process_one "$src" "" "" ""
  done
fi

log "すべて完了"
