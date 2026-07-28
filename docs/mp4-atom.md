# mp4-atom クレートの使い方

→ 入口: [overview.md](overview.md) / 選定理由: [tech-stack.md](tech-stack.md)

mp4 の読み書きコードを書くときはこの文書を読む。**すべて実ファイル（H.264 + Opus）で検証済み。**

## なぜこのクレートか

本ツールはコーデックを理解する必要がない。**`stsd` を不透明なバイト列としてコピーしたいだけ**。`mp4-atom` はこの要件に合う:

- `Codec` 列挙が `Avc1` / `Hev1` / `Hvc1` / `Vp08` / `Vp09` / `Av01` / `Mp4a` / `Tx3g` / **`Opus`** / `Uncv` / `Flac` / `Ac3` / `Eac3` / `Ipcm` / `Fpcm` / `Sowt` / `Twos` / `Lpcm` / `In24` … を網羅し、`#[non_exhaustive]`
- `Any::Unknown(FourCC, Vec<u8>)` で未知アトムを不透明バイト列として保持
- `stsz` / `stsc` / `stco` / `co64` / `stts` / `ctts` / `stss` をすべて公開

## 検証済みの事実

| 項目 | 結果 |
|---|---|
| Opus の認識 | ✓ `Codec::Opus`（`dOps`） |
| サンプル表の復元 | ✓ 98,972 / 165,098 サンプル、同期サンプル 825 個（ffprobe と一致） |
| サンプル部分集合の書き出し | ✓ 有効な mp4 が生成され ffmpeg がエラーなくデコード |
| パケットのビット一致 | ✓ 映像 240/240、音声 402/402 で相違なし |
| 表示順の連続性 | ✓ 欠落 0 |

**注意**: `moov` を decode → encode すると**バイト一致しない**（実測 3,174,871 → 3,174,931、+60 bytes）。サンプルテーブルはこちらで作り直すので問題にならないが、「ラウンドトリップがバイト一致する」ことを前提にしたコードを書かないこと。

## 落とし穴

- **`mp4_atom::Result` が `std::result::Result` を隠す。** `fn main() -> Result<(), Box<dyn Error>>` はコンパイルエラーになる（型引数が 1 個の別名）。`std::result::Result<...>` と明示する
- `StszSamples` は `Identical { count, size }` と `Different { sizes }` の**構造体バリアント**（タプルではない）
- `CttsEntry::sample_offset` は **`i64`**
- `Header::size` は `Option<u64>`

## サンプル表の復元

`stbl` から `(offset, size, duration, cts_offset, is_sync)` を組み立てる。検証済みコード:

```rust
fn samples(stbl: &Stbl) -> Vec<(u64, u32, u32, i64, bool)> {
    let sizes: Vec<u32> = match &stbl.stsz.samples {
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
        StszSamples::Different { sizes } => sizes.clone(),
    };
    let chunk_offsets: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (Some(stco), _) => stco.entries.iter().map(|&o| o as u64).collect(),
        (_, Some(co64)) => co64.entries.clone(),
        _ => vec![],
    };
    // stsc を展開して sample -> chunk を得る
    let mut samp_chunk = vec![];
    {
        let e = &stbl.stsc.entries;
        let mut si = 0usize;
        for (i, ent) in e.iter().enumerate() {
            let last = if i + 1 < e.len() { e[i+1].first_chunk - 1 }
                       else { chunk_offsets.len() as u32 };
            for c in ent.first_chunk..=last {
                for _ in 0..ent.samples_per_chunk {
                    if si < sizes.len() { samp_chunk.push(c as usize - 1); si += 1; }
                }
            }
        }
    }
    let mut durs = vec![];
    for e in &stbl.stts.entries {
        for _ in 0..e.sample_count { durs.push(e.sample_delta); }
    }
    let mut ctss = vec![0i64; sizes.len()];
    if let Some(ctts) = &stbl.ctts {
        let mut i = 0usize;
        for e in &ctts.entries {
            for _ in 0..e.sample_count {
                if i < ctss.len() { ctss[i] = e.sample_offset; i += 1; }
            }
        }
    }
    // stss は 1 始まり。存在しない場合は全サンプルが同期扱い（音声など）
    let sync: std::collections::HashSet<u32> = stbl.stss.as_ref()
        .map(|s| s.entries.iter().cloned().collect()).unwrap_or_default();

    let mut out = vec![];
    for i in 0..sizes.len() {
        let c = samp_chunk[i];
        let mut off = chunk_offsets[c];
        for j in 0..i { if samp_chunk[j] == c { off += sizes[j] as u64; } }
        out.push((off, sizes[i], *durs.get(i).unwrap_or(&0), ctss[i],
                  sync.is_empty() || sync.contains(&(i as u32 + 1))));
    }
    out
}
```

**性能上の注意**: 上の内側ループ（`for j in 0..i`）は O(n²)。10 万サンプルでは実測で許容範囲だったが、**チャンクごとに累積を持つ形に直すべき**。

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
let mdat_body_start = buf.len() as u64 + 8;   // mdat ヘッダ 8 バイト
for (ti, trak) in moov.trak.iter().enumerate() {
    let s = samples(&trak.mdia.minf.stbl);
    for &i in &keep[ti] {
        offs.push(mdat_body_start + mdat.len() as u64);
        // 元ファイルの s[i].0 から s[i].1 バイト読んで mdat に追記
    }
}

// 3. moov を clone してテーブルだけ差し替え
let mut nmoov = moov.clone();
for (ti, trak) in nmoov.trak.iter_mut().enumerate() {
    let s = samples(&moov.trak[ti].mdia.minf.stbl);
    let k = &keep[ti];
    let stbl = &mut trak.mdia.minf.stbl;
    stbl.stsz = Stsz { samples: StszSamples::Different {
        sizes: k.iter().map(|&i| s[i].1).collect() } };
    stbl.stts = Stts { entries: k.iter()
        .map(|&i| SttsEntry { sample_count: 1, sample_delta: s[i].2 }).collect() };
    stbl.ctts = moov.trak[ti].mdia.minf.stbl.ctts.as_ref().map(|_| Ctts {
        entries: k.iter()
            .map(|&i| CttsEntry { sample_count: 1, sample_offset: s[i].3 }).collect() });
    stbl.stss = moov.trak[ti].mdia.minf.stbl.stss.as_ref().map(|_| Stss {
        entries: k.iter().enumerate()
            .filter(|&(_, &i)| s[i].4).map(|(n, _)| n as u32 + 1).collect() });
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

### 本実装で直すべき点

上のコードは検証用の最小実装。本番では:

1. **`stts` / `ctts` をランレングス圧縮する。** 1 サンプル 1 エントリだと 10 万サンプルで `stts` が 800 KB になる。同じ値が連続する区間をまとめる
2. **チャンクを分割する。** 上のコードはトラックごとに 1 チャンク（`mdat` が「映像全部 → 音声全部」の順）。プレイヤが再生時に大きくシークするので、**1 秒程度でインターリーブする**
3. **`stco` / `co64` を選択する。** 4 GB を超える場合は `co64` が必要。上のコードは `stco` 固定
4. **`elst`（edit list）の扱いを決める。** 元ファイルに edit list があるとタイムラインが変わる。現状は `moov.clone()` でそのまま引き継がれるが、サンプルを削った後に整合するとは限らない。**未検証**
5. **`stsd` に複数エントリがある場合の `sample_description_index`。** 上のコードは 1 固定。**未検証**
