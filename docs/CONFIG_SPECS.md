# 設定ファイル仕様書

本書では `luwiki-injector` のTOML設定ファイル形式、探索、優先順位、検証および秘密情報の扱いを定義する。

## 設定ファイルの探索

- `--config-path FILE` が指定された場合、そのパスを設定ファイルとして使用する。
- 指定されない場合の既定パスは `$XDG_CONFIG_HOME/luwiki/config.toml` とする。
- Linuxで `XDG_CONFIG_HOME` が未設定の場合は、ユーザー設定ディレクトリ配下の `luwiki/config.toml` とする。
- 明示指定したファイルを読み取れない場合、またはTOML構文が不正な場合は設定エラーとする。
- 既定パスに設定ファイルが存在しない場合は設定エラーとせず、環境変数と既定値だけで処理を継続する。

## TOML構造

設定ファイルでは次のテーブルを使用する。各キーは任意であり、省略時はCLI、環境変数、既定値などの優先順位に従う。

```toml
[server]
url = "https://localhost:8080"
# token = "<bearer-token>"
# user = "<basic-user>"
# password = "<basic-password>"

[page]
prefix = "/"
mode = "error"

[limits]
# キーを省略した場合は下表の暫定既定値を使用する。
```

上記は基本テーブルの形式例である。上限値の既定値と許容範囲は下表に示す。

| テーブル | キー | 型 | 意味 |
|:--|:--|:--|:--|
| `[server]` | `url` | 文字列 | Wiki対象サーバURL |
| `[server]` | `token` | 文字列 | Wiki Bearerトークン |
| `[server]` | `user` | 文字列 | Wiki Basic認証ユーザー名 |
| `[server]` | `password` | 文字列 | Wiki Basic認証パスワード |
| `[page]` | `prefix` | 文字列 | 投影先ページパスのプレフィクス |
| `[page]` | `mode` | 文字列 | `error`、`update`、`overwrite` のいずれか |
| `[limits]` | `max_pages` | 正整数 | root内の投入対象ページ数の上限 |
| `[limits]` | `max_page_bytes` | 正整数 | root内の対象Markdownの1ページ本文上限（bytes）。root外の本文・既存Wiki本文・JSON・エラー応答へ適用しない |
| `[limits]` | `max_zip_entries` | 正整数 | ZIPエントリ数上限 |
| `[limits]` | `max_zip_compressed_bytes` | 正整数 | ヘッダー等を含むZIPファイル全体のサイズ上限（bytes） |
| `[limits]` | `max_zip_uncompressed_bytes` | 正整数 | ZIP展開後データ量上限（bytes） |
| `[limits]` | `max_git_bytes` | 正整数 | Git取得後の専用一時領域サイズ上限（bytes）。checkout・Wiki書込前に判定し、取得中は協調停止を試みる |
| `[limits]` | `max_processing_seconds` | 正整数 | 取得からWiki操作まで共有する絶対期限の処理時間上限（秒）。再照合・ロック解放も同じ期限に従う |
| `[limits]` | `max_http_concurrency` | 正整数 | HTTPリクエスト並列数上限 |

### 初期版の暫定上限値

ページ件数/本文上限はソース最上位の `root/` 配下だけを対象にする。
root外をページ候補から除外しても、ZIP全entry数/全体/全展開量と
Git取得専用領域の上限はソース全体の資源へ適用する。

| キー | 既定値 | 設定可能範囲 |
|:--|--:|:--|
| `max_pages` | 1,000 | 1〜4,000 |
| `max_page_bytes` | 1,048,576 bytes（1 MiB） | 1〜4,194,304 bytes |
| `max_zip_entries` | 5,000 | 1〜20,000 |
| `max_zip_compressed_bytes` | 134,217,728 bytes（128 MiB） | 1〜536,870,912 bytes |
| `max_zip_uncompressed_bytes` | 268,435,456 bytes（256 MiB） | 1〜1,073,741,824 bytes |
| `max_git_bytes` | 536,870,912 bytes（512 MiB） | 1〜2,147,483,648 bytes |
| `max_processing_seconds` | 300秒 | 1〜1,200秒 |
| `max_http_concurrency` | 1 | 1〜4 |

これらは合成ZIPと開発文書の規模を根拠にした暫定値である。
次の測定証拠はroot内部限定規則の追加前の入力/候補であり、
root付き新版では元7本文を保持したZIP45,851 bytesを用い、同値上限で計画成立後
lookup503・終了1・GET1・変更要求0、上限45,850で終了2・HTTP0、
旧rootなしZIPで終了2・HTTP0を確認した。新候補で全37caseを資源再測定した結果や、
新7ページを実Wikiへ作成した証拠ではない。不変部分へ旧測定を限定再利用し、
8暫定値・範囲の維持判断を継承する。
2026-10-30の実入力再評価に基づき、8項目すべての既定値と設定可能範囲を
維持する判断を承認済みである。評価対象はWiki初期ページとAIハーネスの
二用途で共用する固定7ページ、本文合計110,833 bytesの1集合であり、
異なる二つの独立データ集合を測定したものではない。

ZIP/LocalGitの正常・同一/相違、六容量上限の正常/境界/拒否、実期限、
HTTP上限設定を33測定＋設定拒否4の37caseとして確認した。
これは上表の既定値・設定可能範囲の最大まで負荷をかけた評価ではなく、
現行値を変更する実測根拠がないという限定的な維持判断である。
Git容量境界はLocalGit snapshotの評価であり、リモート取得資源、巨大入力、
全wire位置の遮断、実CLIの外部取消は未実測の範囲として残る。

HTTP上限2～4を設定しても、ページ処理・変更HTTPは直列である。
既定値1と同様、設定値をページ書込の並列数や性能保証と解釈しない。
実測の時間/RSS/容量は当該入力・環境・観測方法に限られ、
絶対RSS上限、同期I/Oの即時中断、領域の即時回収を保証しない。
測定には観測feature `measurement` を有効にしたrelease binaryを用い、
既定featureの通常binaryとは区別する。外部samplingには欠測と間隔の揺らぎがあり、
accounting RSSは直接childのwait4 lifetimeで、exec前やwaited descendantsの
寄与を含み得る。Rust処理だけの専有peakとせず、独立した分類最大値の和を
同時刻の容量peakとしない。source/payloadの一時二重保有も考慮する。
停止と観測の契約は [BASE_DESIGN.md](BASE_DESIGN.md) 第6節と
[CLI_SPECS.md](CLI_SPECS.md) の確認範囲に従う。
新旧証拠と保証境界の説明は [LIMITS_AND_VALIDATION.md](LIMITS_AND_VALIDATION.md) を参照する。

Wiki応答へ固定受信量上限を追加する設定は設けない。本文・JSON・エラー応答は逐次処理し、同じ処理期限・取消で管理する。期限・取消到達後は再照合・既存ロック解放を含む追加HTTPを行わず、別の後処理猶予を設けない。

GitHub認証トークンはこのファイルに設定しない。`GITHUB_TOKEN` 環境変数だけで指定する。

## 値の優先順位

### 対象URL

1. CLI位置引数 `TARGET-SERVER`
2. `LUWIKI_URL` 環境変数
3. `[server].url`
4. `https://localhost:8080`

### Wiki認証

認証情報は項目ごとに次の優先順位とする。

1. CLI引数：`--token`、`--user`、`--password`
2. 環境変数：`LUWIKI_TOKEN`、`LUWIKI_USER`、`LUWIKI_PASSWORD`
3. 設定ファイル：`[server].token`、`[server].user`、`[server].password`
4. TTY対話入力：Basicユーザー名が確定し、パスワードだけが不足している場合

BearerとBasicが同時に指定された場合は曖昧性エラーとする。GitHubトークン `GITHUB_TOKEN` はWiki認証とは別の資格情報であり、この優先順位に含めない。

### ページ設定

- `[page].prefix` の優先順位は `--prefix`、設定ファイル、既定値 `/` の順とする。
- `[page].mode` の優先順位は `--mode`、設定ファイル、既定値 `error` の順とする。
- ページプレフィクスと衝突モードに専用の環境変数は設けない。

### 入力上限

- 上限は `[limits]` の各キーで設定可能とする。
- 上限を省略した場合は「初期版の暫定上限値」の既定値を適用する。
- 指定値が同表の設定可能範囲外であれば設定エラーとする。

## 設定の検証とエラー

- 候補値は優先順位で採用した値だけを意味検証する。TOML構文・型・未知テーブル/キーの検証とファイル読込は、値の採用有無によらず行う。
- 対象Wiki URLはHTTP/HTTPSでホストを必須とし、userinfo・query・fragmentとルート以外のパスを拒否する。空パスと `/` は許容する。
- Wiki認証の完全未指定、およびBasicパスワードだけの指定は起動エラーとする。

- 未知のテーブルまたはキーは設定エラーとする。
- TOML構文エラー、型の不一致、URL・mode・prefix・上限値の検証失敗は設定エラーとする。
- 採用された `server.token`、`server.user`、`server.password` が空文字列の場合は設定エラーとする。認証情報を指定しない場合は対応するキーを省略する。
- 指定された設定ファイルを読み取れない場合は設定エラーとする。
- 設定ファイルにない値は、上記の優先順位に従って環境変数、CLIまたは既定値から解決する。
- 既定の設定ファイルが存在しない場合は「設定ファイルの探索」に従い、設定ファイルを使用せず処理を継続する。

## 認証情報とファイル権限

- BearerトークンおよびBasicパスワードのTOMLへの平文保存を許容する。
- 認証情報を含む設定ファイルがgroupまたはotherから読み取り可能な場合は警告する。
- 設定ファイルに認証情報がない場合、group/other読み取り権限だけを理由とした認証情報警告は行わない。
- 警告は設定読込を拒否する条件ではない。
- 認証情報、Authorization情報、GitHubトークン、Git資格情報、ロックトークンをDebug出力、ログ、エラーに含めない。
- CLIへ秘密情報を直接指定した場合の露出リスク警告はCLI仕様に従う。
