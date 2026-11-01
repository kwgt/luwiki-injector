# CLI 仕様書

本書ではアプリケーションで実装するコマンドライン仕様を定義する。

## コマンドライン

```sh
luwiki-injector [OPTIONS] PAGE-SOURCE [TARGET-SERVER]
```

`PAGE-SOURCE` は必須であり、`TARGET-SERVER` は省略可能である。対象URLの優先順位は位置引数、`LUWIKI_URL`、設定ファイルの `server.url`、既定値 `https://localhost:8080` の順とする。

## オプション

| オプション | 意味 | 既定値 |
|:--|:--|:--|
| `-c`, `--config-path FILE` | 設定ファイルのパス | `$XDG_CONFIG_HOME/luwiki/config.toml` |
| `-p`, `--prefix PAGE-PATH` | Wikiページパスのプレフィクス | `/` |
| `-m`, `--mode MODE` | ページ衝突時の動作 | `error` |
| `-s`, `--source-type TYPE` | ページソース種別 | `auto` |
| `-t`, `--token TOKEN` | Wiki認証用Bearerトークン | なし |
| `-u`, `--user USER` | Wiki Basic認証のユーザー名 | なし |
| `--password PASSWORD` | Wiki Basic認証のパスワード | なし |
| `--branch BRANCH` | Gitブランチの指定 | なし |
| `--tag TAG` | Gitタグの指定 | なし |
| `--commit COMMIT` | GitコミットIDの指定 | なし |
| `--worktree` | ローカルGitの作業ツリーを入力対象とする | 無効 |
| `-h`, `--help` | ヘルプを表示 | — |
| `-v`, `--version` | プログラムのバージョンを表示 | — |

上表の `$XDG_CONFIG_HOME` はXDG Base Directory仕様の設定ファイル保存先を指す。未設定時の探索は [CONFIG_SPECS.md](CONFIG_SPECS.md) に従う。

`--target` は使用しない。対象URLは第2位置引数で指定する。

## ページソースの指定

四入力方式ともソース最上位の小文字 `root/` 配下だけをページ候補にする。
`root/a.md` は既定prefixで `/a`、`root/dir/b.md` は `/dir/b` となる。
root外のREADME等は取り込まず、root不在は入力エラーにする。
ZIPでは明示directory entryがなくてもroot内のfileから存在を認識する。
rootが存在して対象Markdownが0件なら、全安全検査成功後は正常空計画とする。
`--prefix /取込先` では `/取込先/a` のように投影し、元入力参照には `root/` を残す。
全ソースの構造安全検査とZIP/Git全量上限はroot外にも維持する。

`--source-type` の値は `auto`、`zip`、`git`、`github` とする。省略時と明示的な `auto` は同じ動作とする。

`auto` は次の順で判定する。

1. 実在するローカルパス。通常ファイルで拡張子が小文字の `.zip` と完全一致する場合はZIP、Git worktreeとして検出できるディレクトリはローカルGitとする。
2. HTTPS URLはリモートGitとする。
3. 以上に該当せず、URLスキームを含まない厳格な `owner/repo` 形式はGitHubプロジェクト指定とする。文字列全体に `/` が1個だけ存在し、ownerとrepoの両方が空でなく、空白を含まないことを構文条件とする。GitHub上での名称の有効性と存在はGitHub APIで検証する。
4. その他は入力エラーとする。

存在しないZIPパス、`.zip`以外の通常ファイル、Gitリポジトリでないディレクトリ、bare repositoryは初期版の入力対象外である。`git`はローカルGit worktreeまたはHTTPSリモートGitを表す。`github`はGitHubの `owner/repo` を表す。

HTTPSリモートGit URLはホストとリポジトリパスを許容し、userinfo・query・fragmentを拒否する。認証情報はURLへ埋め込まない。

## Git参照

`--branch`、`--tag`、`--commit` は相互排他であり、同時に複数を指定した場合は入力エラーとする。いずれも指定されない場合、mainがあればmain、なければmasterを既定参照とする。remote HEADまたはGitHub default branchは既定参照に使わない。

`--worktree` はローカルGitで未コミットの作業ツリーを入力する指定であり、`--branch`、`--tag`、`--commit`とは併用できない。対象ファイルの範囲は `REQUIREMENTS.md` に従う。

## 接続先と認証

### 対象URL

対象URLの優先順位は次のとおりとする。

1. `TARGET-SERVER` 位置引数
2. `LUWIKI_URL` 環境変数
3. 設定ファイルの `server.url`
4. `https://localhost:8080`

対象Wiki URLはHTTP/HTTPS、ホスト必須とし、userinfo・query・fragmentとルート以外のパスを拒否する。空パスと `/` は許容する。意味検証は優先順位で採用した値だけに適用する。

### Wiki認証

Wikiの認証方式はBearerまたはBasicのいずれか一方とする。両方式が同時に指定された場合は曖昧性エラーとする。

認証情報が完全に未指定の場合と、Basicパスワードだけが指定された場合は起動エラーとする。

認証情報の値は、CLI引数、環境変数、設定ファイル、条件付きTTY対話入力の順に解決する。Bearerトークンは `-t/--token` または `LUWIKI_TOKEN`、Basicユーザー名・パスワードは `-u/--user`・`--password` または `LUWIKI_USER`・`LUWIKI_PASSWORD`、設定ファイルの `server` 項目から取得する。

対話入力を使うのは、TTY上でBasicユーザー名が確定し、パスワードだけが不足している場合に限る。非TTYでは対話入力を行わない。CLIに秘密情報を直接指定した場合、プロセス一覧やシェル履歴に残る可能性を警告する。

GitHubトークンは `GITHUB_TOKEN` 環境変数だけで指定する。CLI引数・設定ファイルでは受け付けない。

認証情報を含む設定ファイルがgroupまたはotherから読み取り可能な場合は警告する。秘密情報、Authorization情報、Git資格情報、ページ本文、ロックトークンをログ・Debug出力・エラーに含めない。

LuWiki 0.9.32のBearer scopeは操作ごとに判定される。全modeの通常遷移を
許可する集合はread/create/update/delete、またはこれらを包含するwriteである。
既存同一skipやerror拒否にdeleteまで常時要求するという意味ではない。
詳細はBASE_DESIGN.mdの単発操作表に従い、token値からscopeを推測/自動付与しない。
Bearerのpath prefix制約とロックtoken認証も独立して適用される。

## ページプレフィクスと衝突モード

`--prefix PAGE-PATH` は入力ページの投影先パスの前に付加する。設定ファイルの `page.prefix` と既定値 `/` が使われ、優先順位はCLI、設定ファイル、既定値の順とする。専用の環境変数は設けない。パスとプレフィクスの検証条件は `REQUIREMENTS.md` に従う。

`--mode MODE` の値は次のとおりとする。優先順位はCLI、設定ファイル、既定値の順であり、専用の環境変数は設けない。

- `error`: 対象ページが既に存在すれば、本文が同一でも無条件にエラー。
- `update`: 既存本文と入力本文が同一なら無視し、異なる場合は更新。
- `overwrite`: 既存本文と入力本文が同一なら無視し、異なる場合は既存ページを削除して再作成。

ロック、競合、500、不確定応答、部分成功、警告の扱いは `REQUIREMENTS.md` に従う。

期限・取消到達後は、再照合や自分の既存ロック解放を含め追加HTTPを行わない。別の猶予は設けず、確定情報と不確定状態を報告して停止する。Wiki HTTPはGETを含めリダイレクトを自動追従せず、3xxを単発操作失敗として扱う。

## 入力上限

入力上限は設定ファイルの `limits` 項目で指定できる。各上限の意味、暫定既定値・許容範囲と実入力再評価の結果・制約は [CONFIG_SPECS.md](CONFIG_SPECS.md) に定義する。

## 結果と終了状態

取得→全件検証→決定的整列の成立計画をExecutorへ渡し、計画順にWikiへ投入する。
同じ絶対期限と取消境界を共有し、計画成立前のWiki HTTPは発行しない。
初期化失敗でも成立計画の全未実行を保持する。最初の失敗または規定の安全停止で
後続を未実行とし、先行成功は自動的に取り消さない。

- stdout：help/version、ページ別結果・件数・終了コード・確定復旧情報。
- stderr：起動/操作警告、主/補助/cleanup診断、env_logger補助ログ。
- ページ別に成功・同一skip・警告付き適用・失敗・未実行を報告する。
- 操作警告は最終結果と独立した発生順の列とし、後続失敗にも先行警告を保持する。
  警告件数は起動警告＋操作警告レコード数。成功ページを重複加算しない。
- 原因段階と最後の試行段階、旧新ID/revision/入力参照と観測済み復旧事実を区別する。
  本文/秘密/raw errorを出力せず、未確認のID/削除/lock状態を補完しない。

| 最終状態 | 終了コード |
| --- | ---: |
| 警告なし正常完了・正常なhelp/version表示 | 0 |
| 実行中の認証/API失敗・競合・停止・未実行・安全停止・表示I/O失敗 | 1 |
| 起動・設定・入力不正（起動時の認証不足等を含む） | 2 |
| 失敗/停止なく完了し、起動/操作警告がある | 3 |

優先順位は1→2→3→0。再作成PUTの目的状態を確認しても、独立した安全停止理由が
ある場合は最終ページ/failed件数0でも1とする。起動警告は後続失敗でも一度だけ保持する。
正常空計画はHTTP0で0、起動警告付きなら3。help/versionは設定/取得/loggerより先に表示し、
これらのwrite/flush失敗も1とする。通常Reportの表示失敗も1で、ログを唯一の結果保存先にしない。

### 採用した検証範囲と限界

以下の実接続代表と資源測定はroot規則追加前の証拠である。
2026-10-31のroot対応候補は通常412成功/0失敗/64 ignored、両check、通常release、
同SHAコピーのhelp/versionとroot loopback3caseを確認した。
四adapterのroot回帰と独立影響評価を組み合わせ、不変部分へ旧証拠を限定再利用した。
新候補で全実接続・全37測定を再実行したとは扱わない。
証拠の出自と保証境界は [LIMITS_AND_VALIDATION.md](LIMITS_AND_VALIDATION.md) にまとめる。

四入力方式と三つのmodeの実CLIから実Wikiへの基本12組合せ、
public/private、Basic/Bearer、部分成功と明示再実行の代表を確認した。
追加のリモート参照試験では、mainと異なるremote HEAD/default branch、
main不在時のmaster、注釈tag、非広告の到達可能commitを確認した。
privateはGitHub指定の注釈tagを追加代表とし、全refと全modeの直積は保証しない。
ZIP64は小規模の拡張entry/終端構造と拒否境界の証拠を持つが、巨大・特殊構造の
全ZIP64アーカイブを実証したものではない。

| 証拠 | 確認した範囲と限界 |
| --- | --- |
| 通常回帰 | 旧測定前候補403成功/0失敗/64 ignored、root対応候補412成功/0失敗/64 ignored。通常回帰、個別の実接続試験、制御試験、測定caseを混算しない |
| 追加リモート8代表 | 固定ref/広告条件、実CLI結果と独立したID/本文照合を確認。private全ref/modeや実行中ref移動の完全防止は未保証 |
| 異常系 | 実上流の適用とrelay下流の500/応答喪失を分けて観測し、警告/非0/再照合/停止を確認。内部自然500やディスク障害の実再現ではない |
| 認可・補助失敗 | 実Lookup401/403、既知IDの範囲外metadata403、実lock代表と、操作別scope・主/補助/cleanupの模擬試験を併用。細分scopeの変更段階実拒否や実上流後の全補助失敗は未実証 |
| 実入力と安全上限 | 二用途共用の固定7ページ集合をZIP/LocalGitで再評価。33測定と設定拒否4の37case。対象と限界は [CONFIG_SPECS.md](CONFIG_SPECS.md) を参照 |
| 期限・取消・所有 | 実期限の停止と期限後追加HTTP0を確認。取消/future dropの所有契約は制御/模擬試験と区別し、実CLIのOS signal取消を保証しない |
| 現行help | root対応の通常releaseと同SHAコピーでhelp/version表示を確認。旧dev buildのhelp/exit0は過去の証拠。位置引数と全optionは本書の構文に一致 |

実期限でReportが3 Created/1 Failed/3未実行でも、別主体の事後観測では
4ページの本文適用が確認された。期限後に製品が追加HTTPをしないことは、
期限時点で進行中だった変更の未適用やサーバーの即時停止を保証しない。
復旧時はReportの確定情報と事後状態を分け、未知の適用/lock状態を補完しない。

実試験で生じたページ・履歴・旧soft-delete・新draft等は自動回収しない。
metadata照会にも期限切れlock/draftのcleanup作用があり得るため、
手動復旧では対象ID/path、現在状態と観測による作用を確認する。
自動再送・再試行・旧ページ復元や、期限後の解除猶予を追加しない。

上記は候補ごとの検証証拠である。通常/measurement両check、通常releaseと証拠の結合、
正式独立更新レビューを確認し、2026-11-01に受け入れた。最終検証runの補助誤判定による
Failedは保持し、製品成功stageと受領後の静的補足・補助修正を別出自で結合している。
過去serverの起動物/埋込revisionや全build設定は未直接証明である。
利用文書を含む最終差分と初期リリース判断は別に行い、全故障・全認証条件の網羅としない。
利用手順は [USAGE.md](USAGE.md)、手動復旧は [RECOVERY.md](RECOVERY.md) に記す。

### OP4接続前の暫定動作（MS3.3・履歴）

以下は2026-10-03承認・MS3.3グループ5で実装した接続前の履歴である。
MS4.3の実行CLI接続により、非空計画の未提供表示は上記の現行動作へ置き換わった。

- 取得・全件検証・決定的整列後の計画が非空なら、全ページを未実行
  としてstdoutへ表示する。投入処理が未提供であることをstderrへ
  報告し、終了状態1とする。計画成立を適用成功として報告しない。
- 全入力の必要な安全検査に成功した空計画は終了状態0とする。
  起動警告がある場合はその警告を保持してstderrへ表示し、
  終了状態3とする。
- 計画成立前の入力・設定エラーは終了状態2、I/O・期限・取消などの
  実行失敗は終了状態1とする。失敗時も起動警告を保持する。
- Wikiの存在確認・読み書きとExecutorによる投入はOP4の対象とする。
