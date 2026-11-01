# luwiki-injector

## 目的と対象

`kwgt/luwiki` のREST APIへMarkdownページを投入するCLIです。
Wikiの初期ページやAIエージェント向けハーネスの準備に使用します。
通常ZIP/ZIP64、ローカルGit、HTTPSリモートGit、GitHubの `owner/repo` を入力にでき、
ソース最上位の小文字 `root/` 配下だけをページとして取り込みます。

既存ページには `error`、`update`、`overwrite` のいずれかを選びます。
`overwrite` は削除後の再作成であり、失敗時に元ページを自動復元しません。
SSH・credential helperによるGit認証、Markdown以外のアセット投入、
自動ロールバック、原子的なoverwrite、front matterによるバージョン比較は対象外です。

## 使い方

```text
luwiki-injector [OPTIONS] PAGE-SOURCE [TARGET-SERVER]
```

`PAGE-SOURCE` は必須の入力、`TARGET-SERVER` は省略可能なWikiのURLです。
以下では、ビルド済みの `luwiki-injector` を実行できる前提で説明します。
ソースから用意する場合は、後述の [ビルドとヘルプ表示](#ビルドとヘルプ表示) を参照してください。

### 入力の準備と実行例

入力の最上位に小文字の `root/` を置き、その配下にMarkdownファイルを用意します。
`root/a.md` は `/a`、`--prefix /取込先` を指定すると `/取込先/a` になります。
root外のREADME等はページ対象外です。root不在は入力エラー、必要な安全検査を通った
空rootは正常な空計画となり、root自体やMarkdownのない親ページは自動生成しません。

Wiki認証を準備したうえで、入力方式に応じて実行します。
以下のURL・リポジトリ・prefixは説明用です。実際の対象へ置き換えてください。

```sh
luwiki-injector --prefix /取込先 pages.zip https://wiki.example
luwiki-injector --mode update --branch main ./pages-repo https://wiki.example
luwiki-injector --tag v1 https://github.com/example/pages.git https://wiki.example
luwiki-injector --source-type github example/pages https://wiki.example
```

入力種別の既定値 `auto` は、実在ローカルパス、HTTPS URL、`owner/repo` の順に判定します。
ZIPは小文字 `.zip` の通常ファイル、ローカルGitはGit worktreeを指定します。
明示する場合は `--source-type zip|git|github` を使います。
Gitの既定参照はmainがあればmain、なければmasterです。remote HEADやGitHubの
default branchを優先しません。ローカルGitの既定はコミット済みツリーで、
未コミットの変更を含める場合は `--worktree` を指定します。

### 接続先・認証・設定

接続先は `TARGET-SERVER` → `LUWIKI_URL` → TOMLの `[server].url` →
`https://localhost:8080` の順に解決します。Wiki URLはHTTP/HTTPSのホストを指定し、
userinfo・query・fragment・ルート以外のパスは付けません。

- Wiki Bearer認証：実行環境で `LUWIKI_TOKEN` を用意します。
- Wiki Basic認証：`LUWIKI_USER` と `LUWIKI_PASSWORD` を用意します。
  TTYでユーザー名だけが確定し、パスワードが不足する場合に限り対話入力できます。
  非TTYでは入力を求めずエラーになります。認証の完全未指定やパスワードだけの指定もエラーです。
- GitHub private取得：Wiki認証と別に、`GITHUB_TOKEN` 環境変数を用意します。
  GitHubトークンをCLI引数や設定ファイルでは受け付けません。

Wiki認証は項目ごとにCLI → 環境変数 → TOML → 条件付きTTYの順で解決します。
BearerとBasicの同時指定はエラーです。上位指定でも下位の別方式は消えないため、
認証方式を切り替える際は環境変数とTOMLに残る別方式の項目も確認してください。
秘密情報のCLI直接指定にはプロセス一覧・シェル履歴への露出リスクがあります。

設定は `--config-path FILE`、省略時は `$XDG_CONFIG_HOME/luwiki/config.toml` を使います。
LinuxでXDG未設定時はユーザー設定ディレクトリ配下を探索し、既定ファイルがなければ
環境変数・既定値で継続します。明示したファイルの読取不能やTOMLの構文・型・未知キーはエラーです。
prefixとmodeはCLI → TOML → 既定値の順です。入力上限はTOMLの `[limits]` で指定します。
キー・優先順位・権限警告の詳細は [設定仕様](docs/CONFIG_SPECS.md) を参照してください。

### 主要オプション

| オプション | 用途・既定値 |
| --- | --- |
| `-c`, `--config-path FILE` | TOML設定ファイルを指定 |
| `-p`, `--prefix PAGE-PATH` | 投影先prefix。既定 `/` |
| `-m`, `--mode MODE` | `error`（既定）、`update`、`overwrite` |
| `-s`, `--source-type TYPE` | `auto`（既定）、`zip`、`git`、`github` |
| `-t`, `--token TOKEN` | Wiki Bearer認証 |
| `-u`, `--user USER`、`--password PASSWORD` | Wiki Basic認証 |
| `--branch BRANCH`、`--tag TAG`、`--commit COMMIT` | Git参照。3指定は相互排他 |
| `--worktree` | ローカルGitの未コミット変更とignore対象外の未追跡ファイルを含める。明示refと併用不可 |
| `-h`, `--help` | ヘルプ表示 |
| `-v`, `--version` | バージョン表示 |

### 既存ページがある場合

不存在ページは全modeで新規作成します。既存ページは次のように処理します。
同一判定は本文のUTF-8バイト列の完全一致で、BOM・改行・末尾改行を変換しません。

| mode | 既存本文が同一 | 既存本文が相違 |
| --- | --- | --- |
| `error`（既定） | エラーで停止 | エラーで停止 |
| `update` | 書込なしskip | ロック後に再比較して更新 |
| `overwrite` | 書込なしskip | 削除直前に再照合し、削除して再作成 |

1件の失敗や安全停止で後続は未実行となり、先行成功は残ります。
応答不明時に適用済みの場合もあるため、同じコマンドの再実行前に
[手動復旧](docs/RECOVERY.md) で対象・modeと現在状態の確認方法を参照してください。

### 結果と終了コード

stdoutにページ別結果・集計・確定した復旧情報、stderrに警告・診断・補助ログを表示します。
終了コードだけでなく、成功・skip・警告付き適用・失敗・未実行と警告を併せて確認します。

| コード | 意味 |
| --- | --- |
| 0 | 警告なし正常完了、正常なhelp/version表示 |
| 1 | 実行中のAPI失敗・競合・停止・未実行・安全停止・表示I/O失敗等 |
| 2 | 起動・設定・入力不正 |
| 3 | 失敗/停止なく完了したが、起動/操作警告あり |

終了コードの優先順位は1→2→3→0です。正常空計画はWiki HTTP0で0、起動警告付きなら3です。
適用を確認できても警告や独立した安全停止が残る場合は非0になります。
詳しい入力規則と実行手順は [利用手順](docs/USAGE.md)、厳密な構文は
[CLI仕様](docs/CLI_SPECS.md) にあります。

## 実行環境

初期検証基準はLinuxとRust 1.92です。受け入れ検証はLinux aarch64、
Rust/Cargo 1.92.0、`aarch64-unknown-linux-gnu` で行っています。
正式なMSRVと他OSでの動作は保証していません。
このリポジトリのソースからビルドする方法を以下に示します。
検証の範囲と保証外の条件は [上限と検証範囲](docs/LIMITS_AND_VALIDATION.md) を参照してください。

## ビルドとヘルプ表示

リポジトリの最上位で実行します。通常構成では追加featureを指定しません。
`measurement` は資源観測用で、既定では無効です。

```sh
cargo build --locked --release --jobs 1
cargo run --locked --release --jobs 1 -- --help
cargo run --locked --release --jobs 1 -- --version
```

Cargoの `target-dir` やtarget設定によって出力先は変わります。
ビルド済みbinaryを直接実行する場合は、Cargoのartifact情報または実際の出力先を使ってください。
検証環境と同じtargetを明示する場合はCargoに
`--target aarch64-unknown-linux-gnu` を指定します。
別環境で同じbinaryや再現ビルドが得られることは保証していません。

help/versionは設定読込・入力取得より先に表示されます。

通常の検証コマンドは `cargo check --locked --all-targets --jobs 1` と
`cargo test --locked --jobs 1` です。実Wikiを操作する個別試験や
ignored試験の実行は別途対象・認証・副作用を確認して行います。
通常回帰と実接続・測定の実績は [検証範囲](docs/LIMITS_AND_VALIDATION.md) に分けて記載しています。

## 利用を始める

1. [利用手順](docs/USAGE.md) でroot付き入力を準備し、設定・認証・modeを確認します。
2. 実行後は終了コードだけでなく、ページ別結果と警告を読みます。
3. 失敗・警告・適用不明がある場合は [手動復旧](docs/RECOVERY.md) に進みます。
4. 上限、検証済み環境、未実証事項は [上限と検証範囲](docs/LIMITS_AND_VALIDATION.md) で確認します。

厳密な契約は [要求仕様](docs/REQUIREMENTS.md)、
[CLI仕様](docs/CLI_SPECS.md)、[設定仕様](docs/CONFIG_SPECS.md)、
[基本設計](docs/BASE_DESIGN.md) に定義しています。
