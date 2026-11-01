# 利用手順

## 1. 実行前の確認

この文書は [README](../README.md) のビルド後に使用します。
以下のコマンドは説明例です。`example`、`https://wiki.example`、入力名、prefixを
実際に使う対象へ置き換え、対象Wiki・認証権限・既存ページへの影響を確認して実行してください。
例のWiki認証は、後述の環境変数または条件付きTTY入力で準備します。
秘密情報をコマンドの引数へ直接書く必要はありません。

製品の構文は `luwiki-injector [OPTIONS] PAGE-SOURCE [TARGET-SERVER]` です。
ここではCargoの出力先を仮定せず、`cargo run` から起動します。
`--` より後が製品への引数です。全optionは [CLI仕様](CLI_SPECS.md) にあります。

## 2. 入力ツリーとページへの投影

四入力方式の全てで、ソース最上位の小文字 `root/` ディレクトリが必須です。
ZIPの最上位、または選択したGit commit/worktreeの最上位を基準にします。

```text
入力の最上位/
├── README.md                 ページ対象外
└── root/
    ├── 開始.md               → /取込先/開始
    └── 開始/
        └── 手順.md           → /取込先/開始/手順
```

この例は `--prefix /取込先` の場合です。既定prefix `/` なら `/開始` と `/開始/手順` です。
先頭の `root/` 一段だけを除去し、小文字 `.md` の拡張子を除いた名前を投影します。
復旧用の入力参照は `root/開始.md` のように元のroot付き相対名を保持します。
`root/root/a.md` は `/root/a` に投影されます。

- root不在、またはrootが通常ファイル等でdirectoryとして扱えない場合は入力エラーです。
  ソース最上位へのfallbackはありません。
- ZIPは明示directory entryがなくても `root/a.md` 等からrootの存在を認識します。
  worktreeの実在する空rootも認識します。
- rootが存在し、必要な安全検査を全て通り、root内の小文字 `.md` 通常ファイルが0件なら
  正常な空計画です。空入力を理由にrootや親Wikiページを作成しません。
- `root/.md` や `root/dir/.md` は拡張子を除いたファイル名が空になるため拒否します。
  ディレクトリだけを理由に親ページを生成することもありません。
- 投入順は親ページから子ページ、同じ深さではページパスのUTF-8文字列の辞書順です。
- 本文は厳格なUTF-8で、置換文字U+FFFDを許しません。BOM、LF/CRLF、末尾改行を
  自動変換せず、同一判定もUTF-8 bytesの完全一致です。
- パス・prefixに空白文字、空セグメント、`.`、`..`、バックスラッシュ、制御文字、NUL、
  連続スラッシュ等を許しません。重複する投影先、symlink、Gitのgitlinkも拒否します。
- ZIP名は生bytesを厳格なUTF-8で検証し、非ASCII名にはUTF-8 flag（bit 11）が必要です。

root外のREADME等はページとして投入しませんが、名前・構造の安全検査とZIP/Gitの全量上限は
root外も含めて適用します。ページ件数・本文の検証と上限はroot内の対象Markdownだけです。
厳密な規則は [要求仕様のページソース](REQUIREMENTS.md#2-ページソース)、
適用時点は [上限と検証範囲](LIMITS_AND_VALIDATION.md) を参照してください。

## 3. 四入力方式とGit参照

`--source-type` は `auto`（既定）、`zip`、`git`、`github` です。
`auto` は、(1)実在するローカルパス、(2)HTTPS URL、(3)厳格な `owner/repo` の順で判定します。
ローカル通常ファイルは拡張子が小文字 `.zip` と完全一致する場合だけZIP、
ディレクトリはGit worktreeとして検出できる場合だけローカルGitです。
存在しないZIP、その他の通常ファイル、Gitでないディレクトリ、bare repositoryは対象外です。
GitHub形式はURLスキームなし、`/` が1個、owner/repoとも非空・空白なしが構文条件で、
名称と存在はGitHub APIで検証します。

| 方式 | 例の入力 | 明示種別 | 認証と注意 |
| --- | --- | --- | --- |
| ZIP/ZIP64 | `pages.zip` | `zip` | rootをZIPの最上位へ置く |
| ローカルGit | `./pages-repo` | `git` | 既定はコミット済みツリー。未コミット変更は含まない |
| HTTPSリモートGit | `https://github.com/example/pages.git` | `git` | ホストとrepo pathを持つHTTPS URL |
| GitHub指定 | `example/pages` | `github` | privateの取得には `GITHUB_TOKEN` を使用 |

HTTPSリモートGit URLはuserinfo・query・fragmentを拒否します。
認証情報をURLへ埋め込まないでください。初期版のGitHub private取得はHTTPSトークンを使い、
SSHとcredential helperによるGit認証は対象外です。

Git/GitHubの既定参照はmainがあればmain、なければmasterです。
remote HEADやGitHubのdefault branchを優先しません。
`--branch`、`--tag`、`--commit` で固定参照を選べますが、相互排他です。
ローカルGitの `--worktree` は、追跡済みファイルのステージ済み・未ステージ変更と、
`.gitignore` 対象外の未追跡ファイルを含みます。
`--worktree` はローカルGit限定で、明示branch/tag/commitとは併用できません。
選択対象にあるsymlinkやsubmodule（gitlink）は展開せず拒否します。

## 4. 接続先・設定・認証

### 設定の探索と優先順位

`--config-path FILE` 指定時はそのファイルを読み、読取不能ならエラーです。
省略時は `$XDG_CONFIG_HOME/luwiki/config.toml` を使い、LinuxでXDG未設定なら
ユーザー設定ディレクトリ配下の `luwiki/config.toml` を探索します。
既定ファイルが存在しない場合は環境変数・既定値で継続します。
TOML構文・型・未知テーブル/キー・読込は、上位の値で上書きされる場合も検証します。
採用値の意味検証は優先順位で選んだ値だけに適用します。

| 項目 | 優先順位（左が優先） |
| --- | --- |
| Wiki URL | `TARGET-SERVER` → `LUWIKI_URL` → `[server].url` → `https://localhost:8080` |
| Wiki Bearer token | `--token` → `LUWIKI_TOKEN` → `[server].token` |
| Basic user | `--user` → `LUWIKI_USER` → `[server].user` |
| Basic password | `--password` → `LUWIKI_PASSWORD` → `[server].password` → 条件付きTTY |
| prefix | `--prefix` → `[page].prefix` → `/` |
| mode | `--mode` → `[page].mode` → `error` |
| 入力上限 | `[limits]` の指定値 → 暫定既定値 |

Wiki URLはHTTP/HTTPSでホスト必須です。userinfo・query・fragment・ルート以外のpathは
拒否し、空pathと `/` は同じ接続先として扱います。prefix/mode用の環境変数はありません。
上限にはCLI optionを設けていません。形式・キー・数値は [設定仕様](CONFIG_SPECS.md) に従います。
秘密を含まない設定例は次のとおりです。

```toml
[server]
url = "https://wiki.example"

[page]
prefix = "/取込先"
mode = "error"
```

### Wiki認証とGitHub認証

WikiはBearerかBasicの一方を使います。値は方式単位でなく上表の項目ごとに解決するため、
上位指定が下位の別方式を消すわけではありません。
例えば設定ファイルにBasic userがある状態でCLIの `--token` を指定すると、
BearerとBasicが共存して曖昧性エラーになります。
方式を切り替える際は、環境変数と設定ファイルに別方式の項目が残っていないか確認します。

- Wiki Bearerは `LUWIKI_TOKEN`、Basicは `LUWIKI_USER` と `LUWIKI_PASSWORD` を
  実行環境で準備できます。この文書に実値を貼ったり、ログへ出したりしないでください。
- Basic userだけが確定しpasswordが不足する場合に限り、TTYで対話入力します。
  非TTYでは入力を求めず起動エラーです。完全未認証、passwordだけの指定も起動エラーです。
  採用されたtoken/user/passwordが空文字列の場合もエラーです。未指定のキーは省略します。
- GitHubトークンは `GITHUB_TOKEN` 環境変数だけで受け付け、Wiki認証とは別です。
  Wiki URLやWiki認証の優先順位に含めず、CLI・TOMLへ保存しません。
- 秘密情報をCLIへ直接指定するとプロセス一覧やシェル履歴への露出リスクを警告します。
  認証情報を含むTOMLがgroup/otherから読める場合も警告します。警告は読込拒否ではありません。
  TOMLへの平文保存は許容されますが、必要な利用者だけが読める権限で管理してください。

### 必要な権限

LuWiki 0.9.32のBearerではlookup/meta/sourceのread、draft作成のcreate、本文PUTと
unlockのupdate、ページDELETEのdeleteが操作ごとに判定されます。
lock取得/延長はcreateまたはupdateです。create/update/deleteはreadを暗黙包含しません。
全modeの通常遷移にはread/create/update/delete、または包含するwriteの集合が使えます。
既存同一skipやerror拒否にdeleteまで常時要求する意味ではありません。
Bearerのpath prefix認可とlock token認証、BasicのReadOnly属性は独立です。
製品はtoken値からscopeを推測・付与しません。
詳しくは [基本設計の単発操作契約](BASE_DESIGN.md#81-単発操作契約) を参照してください。

## 5. 衝突modeを選ぶ

| 対象の状態 | `error`（既定） | `update` | `overwrite` |
| --- | --- | --- | --- |
| 不存在 | 新規作成 | 新規作成 | 新規作成 |
| 既存・同一bytes | エラー | 書込なしskip | 書込なしskip |
| 既存・相違bytes | エラー | ロック後再比較して更新 | 削除直前再照合後、削除して再作成 |

`update` は事前確認後の他者変更を競合として停止します。
`overwrite` は非原子的で、削除後に失敗すると旧ページの自動復元はありません。
PUT/DELETEの500や応答状態不確定時は、同じ期限・取消境界が許す範囲で単回再照合します。
目的状態を確認しても警告を保持し、非0で終了します。
DELETE完全500後の再作成は旧削除確定の場合に限り、POST以降の不確定時は停止します。
自動再送・再試行・ロールバックは行いません。

1件の失敗や安全停止で後続ページは未実行となり、先行成功を取り消しません。
取得からWiki操作まで同じ絶対期限・取消を使い、到達後は再照合や自分のlock解除も
追加HTTPを発行しません。GETを含めリダイレクトを自動追従せず、3xxを失敗として扱います。

## 6. 結果と終了コード

stdoutにはhelp/version、ページ別結果、集計、終了コード、確定した復旧情報を表示します。
stderrには起動/操作警告、主・補助・cleanup診断、補助ログを出します。
本文、認証情報、Authorization、Git資格情報、lock token、外部のraw errorを出しません。
ログだけを結果保存先にせず、stdout/stderrの両方を確認してください。

| コード | 意味 |
| --- | --- |
| 0 | 警告なし正常完了、正常なhelp/version表示 |
| 1 | 実行中の認証/API失敗、競合、停止、未実行、安全停止、表示I/O失敗 |
| 2 | 起動・設定・入力不正（起動時の認証不足を含む） |
| 3 | 失敗/停止なく完了したが起動/操作警告あり |

優先順位は1→2→3→0です。操作警告は最終ページ結果と独立した発生順の列で、
後続失敗でも先行警告を保持します。警告件数は起動警告と操作警告レコード数であり、
成功ページを重複加算しません。目的状態を確認してfailed件数が0でも、
独立した安全停止理由があれば1です。

空計画はWiki HTTP0で終了0、起動警告があれば3です。
help/versionのwrite/flushや通常Reportの表示に失敗した場合は1です。
「空計画」「表示失敗」「未実行」「未適用」を同じ意味として扱わないでください。
Reportの成功・skip・警告付き適用・失敗・未実行と既知IDを読み、
不確定や部分成功がある場合は [手動復旧](RECOVERY.md) へ進みます。

## 7. 実行例

下記はroot付き入力と認証が準備済みの場合の構文例です。
`--config-path` を使う場合は準備した非秘密設定ファイルを指定できます。
第2例以降は `TARGET-SERVER` を省略しており、`LUWIKI_URL` またはTOMLの
`server.url` を準備した前提です。いずれも未指定なら `https://localhost:8080` へ接続します。

```sh
cargo run --locked --release --jobs 1 -- pages.zip https://wiki.example
cargo run --locked --release --jobs 1 -- --source-type zip --prefix /取込先 pages.zip
cargo run --locked --release --jobs 1 -- --source-type git --branch main ./pages-repo
cargo run --locked --release --jobs 1 -- --source-type git --worktree ./pages-repo
cargo run --locked --release --jobs 1 -- --source-type git --tag v1 https://github.com/example/pages.git
cargo run --locked --release --jobs 1 -- --source-type github --branch main example/pages
cargo run --locked --release --jobs 1 -- --config-path ./injector-example.toml --mode update pages.zip
```

固定commitを使う場合は `--branch` や `--tag` の代わりに
`--commit COMMIT` を使い、`COMMIT` を対象リポジトリの実際のcommit IDへ置き換えます。
Git参照選択時点を固定しても、実行中のリモート側の変更を全条件で防げる保証ではありません。
`overwrite` を選ぶ前には削除後失敗と手動復旧の影響を確認してください。
同じコマンドの再実行も、現在状態とmodeによって新規・skip・更新・削除再作成が変わります。

原典は [要求仕様](REQUIREMENTS.md)、[CLI仕様](CLI_SPECS.md)、
[設定仕様](CONFIG_SPECS.md) です。環境と測定上の限界は
[上限と検証範囲](LIMITS_AND_VALIDATION.md) にまとめています。
