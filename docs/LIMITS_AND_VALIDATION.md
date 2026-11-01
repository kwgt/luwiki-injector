# 上限と検証範囲

## 1. 八つの安全上限

キー・型・暫定既定値・設定可能範囲の唯一の定義元は
[設定仕様の暫定上限値](CONFIG_SPECS.md#初期版の暫定上限値) です。
この文書では数値表を複製せず、意味と適用時点を説明します。
上限はTOMLの `[limits]` で指定し、範囲外は設定エラーです。
省略時は暫定既定値を使います。上限用のCLI optionはありません。

| キー | 単位・対象・適用時点 |
| --- | --- |
| `max_pages` | 件。root内で投入対象になるページ集合の件数 |
| `max_page_bytes` | bytes。root内の対象Markdownの1ページ本文 |
| `max_zip_entries` | 件。root外も含むZIP全エントリ数 |
| `max_zip_compressed_bytes` | bytes。ヘッダー・中央directory・extra fieldを含むZIP全体 |
| `max_zip_uncompressed_bytes` | bytes。root外も含むZIP展開後データ量 |
| `max_git_bytes` | bytes。取得後の専用一時領域サイズ。checkout・Wiki書込前に超過を拒否 |
| `max_processing_seconds` | 秒。取得からWiki操作まで共有する絶対期限。再照合・lock解放も同じ期限 |
| `max_http_concurrency` | 件。HTTPリクエストの同時実行上限。ページ書込並列数ではない |

## 2. 検査と停止の境界

全件の取得・安全検証・決定的整列で計画が成立するまで、Wiki HTTPを発行しません。
root外をページ対象から除外しても、全ソースの名前/構造検査とZIP/Gitの全量上限は残ります。
ページ件数、本文UTF-8/置換文字検証、本文サイズはroot内の対象Markdownだけに適用します。
rootがない入力はエラー、安全検査済みの空rootは正常な空計画です。

入力Markdownの `max_page_bytes` を、既存Wiki本文・JSON・エラー応答へ流用しません。
Wiki応答に固定受信量上限を追加せず、逐次処理と同じ期限・取消で管理し、全量保持を避けます。
期限・取消到達後は追加HTTPを行わず、再照合や既存lock解放にも別猶予を設けません。
進行中のサーバー変更が事後に適用される可能性は [手動復旧](RECOVERY.md) のとおりです。

Git取得中は進捗監視で協調停止を試みますが、通信量/一時領域が一時的に上限を超える
可能性を許容します。上限はwire転送量の厳密な上限ではありません。
同期I/Oやworkerの終了に時間がかかり、所有者を先に回収しないため、
停止通知と実際のI/O終了・領域回収は同時とは限りません。
sourceと検証済みpayloadを一時的に二重保有する容量も必要です。
HTTP上限を複数に設定しても、ページ処理と変更HTTPは直列です。

## 3. 採用した検証と測定

### 通常構成の最終製品検証

2026-10-31のroot対応候補について、Linux aarch64、Rust/Cargo 1.92.0、
明示target `aarch64-unknown-linux-gnu`、lock固定、jobs 1で確認しました。
通常回帰は412 passed/0 failed/64 ignored（12 summaries、measured/filteredとも0）です。
通常とmeasurement両構成のcheck、通常test、通常release buildが期待結果で警告0でした。
通常releaseは既定feature、opt-level 3、debuginfo 0、debug assertions無効です。
通常releaseのSHA256は
`53e1200e0a11ac4b0a255c91a4858656cddec4ce5ad4b39dbacf085af3552afe`、
サイズ24,391,704 bytesで、同じSHAのコピーからhelp/versionとroot loopbackを確認しました。

四adapterのroot回帰と追加9回帰、loopbackの次の3代表を旧証拠と組み合わせています。

| 入力と条件 | 観測した結果 |
| --- | --- |
| root付きZIP45,851 bytes、圧縮後上限45,851 | 元7本文の計画成立後lookup503、終了1、GET1、変更要求0 |
| 同じZIP、上限45,850 | 入力拒否、終了2、Wiki HTTP0 |
| 旧rootなしZIP | 入力拒否、終了2、Wiki HTTP0 |

これは新7ページを実WikiへCreatedした試験ではありません。
旧四入力×三modeの基本12組合せ、追加リモート8代表、異常系実代表の変更しない部分を、
新root回帰と影響評価に基づき限定再利用しています。旧通常403成功/134fileと
root対応412成功/135fileの候補を区別し、旧成功を新実行と呼び替えていません。

最終検証runの全体状態は、検証補助の終端server比較の誤判定によりFailedのまま保存しています。
成功した12製品検証stage、受領後のsource/binary静的補足、補助修正とmock3case、
作成不参加の担当による正式独立更新レビューを別出自で結合しました。
欠けた元完了時刻を創作せず、補助修正後に製品12stageを再実行したとも扱いません。
製品阻害/要修正動作0、補助問題の処置を確認し、2026-11-01にこの製品検証を受け入れています。
利用文書を含む最終差分確認と初期リリース判断は別です。

### root規則追加前の限定測定

Wiki初期ページとAIハーネスの二用途で共用する小規模7本文1集合
（合計110,833 bytes、最大37,768 bytes）をZIP/LocalGitで評価しました。
独立した二つの入力集合ではありません。旧ZIPは45,781 bytesで、root付き新版とは異なります。
旧観測feature `measurement` のreleaseで33測定＋設定拒否4＝37期待caseを確認しました。
親成功11参照＋新26を採用し、原親runのFailedは保持しています。
root対応候補で全37caseを再測定した結果ではありません。

| 旧測定の観測値 | 意味と限界 |
| --- | --- |
| wall最大19.300129970秒 | 当該case/環境の経過時間。全入力やリモート取得の性能保証ではない |
| accounting RSS最大50,806,784 bytes | 直接childのwait4 lifetime。exec前やwaited descendantsの寄与を含み得る |
| 外部allocated最大180,224 bytes | 外部samplingで見えた値。絶対容量peakではない |
| 内部同時snapshot245,760 bytes | 内部観測の同時点値。独立分類の最大値を足してpeakにしない |
| sampling9,439、hard欠測0、FD消失3、walk消失10、最大間隔57.441204 ms | サンプル間の未観測と終端消失がある。連続完全観測ではない |

時刻・durationは観測器/clockの違いとsampling間隔を含めて解釈し、異なるclockの値を
無条件に直接比較しません。accounting RSSをRust処理だけの専有peakと解釈しません。
旧測定は暫定既定値/範囲の最大まで負荷をかけたものではありません。
現行値を変更する実測根拠がないという限定的判断により、八上限の値と範囲を維持しています。
この維持判断は承認済みであり、文書追加を理由に全測定/再承認へ戻す必要はありません。

実期限では製品の期限後追加HTTP0を確認しましたが、Reportの3 Created/1 Failed/3未実行と
別主体の事後4本文適用を分けています。取消/future dropは制御/模擬試験での証拠であり、
実CLIのOS signal取消を確認したものではありません。
通常回帰、ignoredの別実行、内部subcase、制御試験、測定caseは混算しません。

## 4. 未保証の条件と環境

- Linux/Rust 1.92が初期検証基準です。正式MSRV、他OS、別環境の再現buildは未保証です。
  全Cargo設定/flags/linkerの由来を直接同定した証拠はありません。
- 過去の稼働serverの起動物/埋込revision/build由来は未直接証明です。
  ユーザーによる再起動のみ・同binary申告と、観測した現processの前後一致を分けて
  限定再利用しました。現在状態から過去processを直接同定したとは扱いません。
- 必要な規約是正と移管判断は行いましたが、既存field doc不足362件と追加35fileの
  未処置部分は別作業へ移管しています。全repo規約適合や新規コードへの一般免除ではありません。
  過去の未移管step/comment等の処置済み項目と、この残件を混同しません。
- ZIP64は小規模の拡張entry/終端構造と拒否境界の確認です。巨大・特殊構造の全実証ではありません。
- remote取得資源、全最大規模、Git取得中の一時超過防止、同期停止の即時性は保証しません。
- 自然な内部500/ディスク故障、全wire位置の遮断、細分scopeの全実拒否、
  全補助/cleanup実失敗、実CLIのOS signal取消は未実証です。
  relayで上流適用と下流500/応答喪失を分けた観測を自然障害の再現と扱いません。
- readonly File closeの直接観測、worker/join/permit/領域終端の同時性、即時回収、
  絶対RSS/一時容量peakは保証しません。非原子的overwriteや期限後の適用可能性も残ります。
- public/private、Basic/Bearer、main/master、固定ref等は選定した実代表の証拠です。
  全ref/mode/認証の直積や実行中ref移動の完全防止を保証しません。

## 5. 変更時の再確認

文書のみの変更はsource/docのhash、リンク、契約、保証境界を静的照合します。
製品source/依存/生成helpが変わらなければ、全回帰・実Wiki操作・全資源測定を
機械的に再実行する必要はありません。文書を含む最終差分は作成者と別担当が確認します。

Rust docからclap表示が変わる場合は、コメント変更だけでも文書のみとは扱わず、
新sourceとhelp/build・関係境界を確認します。
認証/取得/root/本文/計画、mode/HTTP/再照合/cleanup/Report、期限/取消/所有、
固定入力/上限、fixture/観測helperの変更は新候補として影響範囲を評価し、
変わった境界に必要なcheck/回帰/build/help・実代表/差分測定を選びます。
過去結果を無条件流用せず、原履歴と補足の出自を保存します。

[README](../README.md) → [利用手順](USAGE.md) → [手動復旧](RECOVERY.md) が利用の導線です。
詳細契約は [要求仕様](REQUIREMENTS.md)、[設定仕様](CONFIG_SPECS.md)、
[CLI仕様](CLI_SPECS.md)、[基本設計](BASE_DESIGN.md)、
[Wikiクライアント設計](WIKI_CLIENT_DESIGN.md) を参照してください。
