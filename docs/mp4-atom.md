# mp4-atom クレートの使い方

→ 入口: [overview.md](overview.md) / 選定理由: [tech-stack.md](tech-stack.md)

mp4 の読み書きコードを書くときはこの文書を読む。**検証は主に実ファイル（H.264 + Opus / AAC）で行った。**

**この文書のコード片は、実装前に書いた検証用の最小コードである**。クレートの使い方・落とし穴の記録として有用である。ただし本番の実装は `src/mp4io/`（`read.rs` / `write.rs` / `order_map.rs` / `support.rs`）にあり、性能とファイル品質のために構造が違う。差分は下記「上のコードと本実装の違い」にまとめてある。

## なぜこのクレートか

本ツールはコーデックを理解する必要がない。**`stsd` を不透明なバイト列としてコピーしたいだけ**。`mp4-atom` はこの要件に合う:

- `Codec` 列挙が映像・音声・字幕を網羅し、`#[non_exhaustive]`（0.14 時点）。映像は `Avc1` / `Hev1` / `Hvc1` / `Vp08` / `Vp09` / `Av01` / `Uncv`。音声は `Opus` / `Mp4a` / `Flac` / `Ac3` / `Eac3` / `Samr` / `Ipcm` / `Fpcm` / `Sowt` / `Twos` / `Lpcm` / `In24` / `In32` / `Fl32` / `Fl64` / `S16l`。字幕は `Tx3g` / `Wvtt`
- `Any::Unknown(FourCC, Vec<u8>)` で未知アトムを不透明バイト列として保持
- `stsz` / `stsc` / `stco` / `co64` / `stts` / `ctts` / `stss` をすべて公開

### 音声トラックとして扱う Codec（唯一の基準）

音声トラックの識別は `src/mp4io/read.rs::is_audio_codec` に集約している。カット処理は
「ソース上の DTS から最近傍パケットを引き当ててビットコピーする」だけでコーデックに
依存しないため、`mp4-atom` が `Codec` として認識する音声系すべてを音声トラックとして
受け入れる。映像・字幕・`Unknown(FourCC)` は音声に数えない。対応一覧（＝この関数の
allowlist）:

| 分類 | Codec |
|---|---|
| 圧縮 | `Opus`, `Mp4a`（AAC）, `Flac`, `Ac3`, `Eac3`, `Samr` |
| 非圧縮 / QuickTime 系 | `Ipcm`, `Fpcm`, `Sowt`, `Twos`, `Lpcm`, `In24`, `In32`, `Fl32`, `Fl64`, `S16l` |

E2E で実検証済みか、認識のみ（未検証）かの区別は [measurements.md](measurements.md)
「音声コーデック別の検証状況」を参照。`Codec` は `#[non_exhaustive]` なので、将来
追加される variant は明示的に allowlist へ足すまで音声に数えない（`_ => false`）。

## 検証済みの事実

| 項目 | 結果 |
|---|---|
| 音声 Codec の認識 | ✓ `is_audio_codec` で一括判定（Opus は `dOps`、AAC は `esds` を保持したまま clone） |
| サンプル表の復元 | ✓ 98,972 / 165,098 サンプル、同期サンプル 825 個（ffprobe と一致） |
| サンプル部分集合の書き出し | ✓ 有効な mp4 が生成され ffmpeg がエラーなくデコード |
| パケットのビット一致 | ✓ 映像 240/240、音声 402/402 で相違なし |
| 表示順の連続性 | ✓ 欠落 0 |

**注意**: `moov` を decode → encode すると**バイト一致しない**。実測では decode 前が 3,174,871。encode 後は 3,174,931（+60 bytes）。サンプルテーブルはこちらで作り直すので問題にならないが、「ラウンドトリップがバイト一致する」ことを前提にしたコードを書かないこと。

## 落とし穴

- **`mp4_atom::Result` が `std::result::Result` を隠す。** `fn main() -> Result<(), Box<dyn Error>>` はコンパイルエラーになる（型引数が 1 個の別名）。`std::result::Result<...>` と明示する
- `StszSamples` は `Identical { count, size }` と `Different { sizes }` の**構造体バリアント**（タプルではない）
- `CttsEntry::sample_offset` は **`i64`**
- `Header::size` は `Option<u64>`

## サンプル表の復元

`stbl` からデコード順の `SampleInfo` を組み立てる。
フィールドは `file_offset` / `size` / `duration` / `cts_offset` / `is_sync`。
実装は `src/mp4io/read.rs::samples(stbl, file_len) -> Result<Vec<SampleInfo>>`。

### 組み立て手順

1. `stsz` から各サンプルのサイズ列を得る（`Identical` / `Different`）
2. `stco` または `co64` からチャンク先頭オフセットを得る
3. `stts` / `ctts` を展開して duration / cts_offset 列を得る
   （`stss` は 1 始まり。無いトラックは全サンプル同期扱い）
4. `stsc` をチャンクごとに展開し、チャンク先頭オフセットに直前サンプルのサイズを積算して
   各サンプルの `file_offset` を求める（**O(n)**）。
   チャンク内で毎回 0 から積み直す O(n²) に戻さないこと。
   10 万サンプルで線形であることを確認するテストあり。

### 入力検証（破損／悪意ある MP4 向け）

サンプル表の `u32` は信頼しない。
確保・展開ループの**前**に検証し、失敗時はパニックではなく **`Err` で停止**する（OOM を起こさない）。

| 対象 | 検証内容 |
|---|---|
| `stsz` | 件数は `file_len` 以下（`size=0` でも巨大 `count` を許さない）。`Identical` は `count * size` を `checked_mul`。`Different` はサイズ総和を `checked_add`。いずれも `file_len` 超でエラー。これで `write.rs` の `copy_buf.resize(s.size)` に届く `size` もファイル長以下に束縛される |
| `stts` / `ctts` | 展開前に `sample_count` を `checked_add` で合計し、`stsz` の総数と一致することを確認する。一致後だけ、既に束縛済みの件数を capacity / ループ上限に使う（`take(total)` で矛盾を隠さない） |

`file_len` は入力ファイルの実バイト長（それ以上に厳しい検証済み mdat 範囲でも可）。
マジックなメモリ上限は持ち込まず、「mdat に収まらない件数・サイズは定義上あり得ない」として束縛する。

`stsc` の `first_chunk` 検証（wrap / 静かな誤オフセット防止）は別途扱う。

**やらないこと**: `mp4-atom` のボックスパーサ自体の改修（検証は `samples()` 側で足りる）。

## トップレベルから moov を取り出す

```rust
let mut r = BufReader::new(File::open(&path)?);
let moov = loop {
    let h = Header::read_from(&mut r)?;
    if h.kind == Moov::KIND { break Moov::read_atom(&h, &mut r)?; }
    r.seek(SeekFrom::Current(h.size.expect("no size") as i64))?;
};
```

`mdat` を読み飛ばすので巨大ファイルでもメモリを食わない。

## 書き出し

**`stsd` には一切触らない。`moov` を `clone()` してサンプルテーブルだけ差し替える。**

レイアウトは `ftyp` → `mdat` → `moov` の順にする。`moov` を最後に置けば `stco` のオフセットが確定してから書けるので、サイズの先読み（鶏と卵）を回避できる。faststart が必要なら後段で `ffmpeg -movflags +faststart` を通す。

```rust
// 1. ftyp を書く
Ftyp { major_brand: b"isom".into(), minor_version: 512,
       compatible_brands: vec![b"isom".into(), b"iso2".into(),
                               b"avc1".into(), b"mp41".into()] }
    .encode(&mut buf)?;

// 2. mdat 本体を組み立てつつ新しいオフセットを記録
//    samples() は file_len で件数・サイズを検証し Result を返す
let mdat_body_start = buf.len() as u64 + 8;   // mdat ヘッダ 8 バイト
for (ti, trak) in moov.trak.iter().enumerate() {
    let s = samples(&trak.mdia.minf.stbl, file_len)?;
    for &i in &keep[ti] {
        offs.push(mdat_body_start + mdat.len() as u64);
        // 元ファイルの s[i].file_offset から s[i].size バイト読んで mdat に追記
    }
}

// 3. moov を clone してテーブルだけ差し替え
let mut nmoov = moov.clone();
for (ti, trak) in nmoov.trak.iter_mut().enumerate() {
    let s = samples(&moov.trak[ti].mdia.minf.stbl, file_len)?;
    let k = &keep[ti];
    let stbl = &mut trak.mdia.minf.stbl;
    stbl.stsz = Stsz { samples: StszSamples::Different {
        sizes: k.iter().map(|&i| s[i].size).collect() } };
    stbl.stts = Stts { entries: k.iter()
        .map(|&i| SttsEntry { sample_count: 1, sample_delta: s[i].duration }).collect() };
    stbl.ctts = moov.trak[ti].mdia.minf.stbl.ctts.as_ref().map(|_| Ctts {
        entries: k.iter()
            .map(|&i| CttsEntry { sample_count: 1, sample_offset: s[i].cts_offset }).collect() });
    stbl.stss = moov.trak[ti].mdia.minf.stbl.stss.as_ref().map(|_| Stss {
        entries: k.iter().enumerate()
            .filter(|&(_, &i)| s[i].is_sync).map(|(n, _)| n as u32 + 1).collect() });
    stbl.stsc = Stsc { entries: vec![StscEntry {
        first_chunk: 1, samples_per_chunk: k.len() as u32,
        sample_description_index: 1 }] };
    stbl.stco = Some(Stco { entries: vec![new_off[ti][0] as u32] });
    stbl.co64 = None;
    // duration を更新
    let dur: u64 = k.iter().map(|&i| s[i].2 as u64).sum();
    trak.mdia.mdhd.duration = dur;
    trak.tkhd.duration = dur * nmoov.mvhd.timescale as u64
                             / trak.mdia.mdhd.timescale as u64;
}
nmoov.mvhd.duration = /* trak の最大値 */;
```

### 上のコードと本実装の違い（すべて対応済み）

**上のコードは検証用の最小実装で、そのままでは本番品質にならない。** 現在の `src/mp4io/write.rs` は以下の点で異なるので、書き出しを直すときは上のコードではなく実装を読むこと。

| 検証コードの単純化 | 本実装 |
|---|---|
| `stts` / `ctts` を 1 サンプル 1 エントリで書く（10 万サンプルで `stts` が 800 KB） | ランレングス圧縮する（`run_length_encode_stts` / `run_length_encode_ctts`） |
| トラックごとに 1 チャンク（`mdat` が「映像全部 → 音声全部」の順。プレイヤが大きくシークする） | 1 秒程度でチャンクを切り、開始時刻順にトラック間でインターリーブする（`chunk_track` + `write_mp4` 内のマージ） |
| `stco` 固定（4 GB 超で破綻） | `mdat` の終端オフセットを見て `stco` / `co64` を選ぶ（`plan_mdat_layout`） |

`elst`（edit list）と `stsd` 複数エントリは**検証の上で意図的に非対応**にしてある（`moov.clone()` で引き継ぐと `media_time` が正当なフレームをスキップすることを実験で確認済み）。`src/mp4io/support.rs` が入力の時点で明示エラーにするので、`write.rs` 側で考慮する必要はない。理由と回避策は [architecture.md](architecture.md) の「未対応の入力」。
