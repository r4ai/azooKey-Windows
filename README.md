# azooKey for Windows

[AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter)を利用したWindows版IMEです。

> [!WARNING]
> 現在開発中であるため、安定性や機能に関しては保証できません。使用する際は自己責任でお願いします。

# インストール方法
[Release](https://github.com/fkunn1326/azooKey-Windows/releases)から`azookey-setup.exe`をダウンロードし、インストーラーを実行してください。

# 機能

- [x] ライブ変換
- [x] Zenzaiを使用したニューラルかな漢字変換

- [ ] 学習機能
- [ ] 辞書登録機能
- [ ] テーマ変更機能
- [ ] 辞書のインポート/エクスポート機能
- [ ] いい感じ変換
- [ ] 個人最適化システム
- [ ] 予測変換

# 設定

## Zenzai

### 変換プロファイル
設定で変換プロファイルを指定すると、プロファイルに応じた変換候補が表示されます。

### バックエンド
以下の3種類のバックエンドをサポートしています。

- **CPU**: 動作が非常に遅いため、非推奨です。
- **CUDA**: NvidiaのGPU専用。[CUDA Toolkit 12系](https://developer.nvidia.com/cuda-downloads)をインストールする必要があります。
- **Vulkan**: GPUのドライバーに標準で含まれているため、追加のインストールは不要です。

# コミュニティ

## 開発を支援する
- [GitHub Sponsors (Miwa)](https://github.com/sponsors/ensan-hcl): 変換エンジンの開発者
- [Patreon (fkunn1326)](https://www.patreon.com/c/fkunn1326): Windowsに移植した人

## 開発に参加する

### 開発環境のセットアップ

- [Visual Studio 2022 Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)（「C++によるデスクトップ開発」とWindows SDK）
- [Rust](https://www.rust-lang.org/tools/install)（MSVC toolchain）
- [Swift for Windows](https://www.swift.org/install/windows/)（Swift 6.1以上）
- [cargo-make](https://github.com/sagiegurari/cargo-make)

Swiftは標準のインストール先から自動検出し、MSVC環境もVisual Studioから自動で読み込みます。Node.js 24.18.0、protoc 29.6、Inno Setup 6.7.3は、初回ビルド時にSHA-256などを検証して`.build-tools`へportable配置します。

AzooKeyのUIAccess起動には管理者権限が必要です。インストールと実行は管理者アカウントで行ってください。標準ユーザーから別の管理者資格情報を入力してインストールする構成は、現在サポートしていません。

### ビルド

#### リポジトリのクローン
```
git clone https://github.com/fkunn1326/azookey-Windows --recursive
```
`--recursive`オプションを付けて、サブモジュールも一緒にクローンしてください。

#### 初回セットアップ

```powershell
cargo install cargo-make --version 0.37.24 --locked
```

Rustのi686 target、llama.cpp b4846のCPU/CUDA/Vulkanバイナリ、Zenzモデル、npm依存もビルドフロー内で検証・準備されるため、2回目以降は次のコマンドだけでビルドできます。初回のみ各資材のダウンロードが発生します。

#### リリースビルド

```powershell
cargo make build --release
```

`build`フォルダーに実行ファイル一式が作成され、配布用インストーラーは`build\azookey-setup.exe`に出力されます。デバッグビルドでは引数を付けずに`cargo make build`を使用してください。

#### インストール

1. 管理者アカウントで`build\azookey-setup.exe`を実行し、UACの確認を許可します。既存のリリース版はアンインストール不要で、そのまま上書き実行できます。更新時は既存のAzooKeyランタイムを自動停止し、`Program Files\Azookey`のファイルと起動タスクを更新します。
2. セットアップ完了後、Windowsを必ず再起動します。IME DLLはメモ帳やブラウザーなどに読み込まれたままになるため、再起動するまで新しいDLLへ完全には切り替わりません。
3. 再起動後、言語バーからAzooKeyを選択します。
4. Zenzaiを使う場合は、AzooKeyの設定画面でZenzaiを有効にしてバックエンドを選択します。バックエンド変更後も、設定画面の案内どおりWindowsを再起動してください。

標準ユーザーのセッションからUACで別の管理者資格情報を入力するインストール方法は、現在サポートしていません。AzooKeyを使用する管理者アカウント自身でインストーラーを実行してください。

入力モード切替を診断するリリースログは`%LOCALAPPDATA%\Azookey\logs\client`に保存されます。通常の文字、変換前テキスト、候補は記録せず、入力モード用仮想キー、修飾キー、TSF compartmentの数値と処理結果だけを記録します。各ホストプロセスのログは最大1 MiBで、終了済みプロセスの古いログは最大64件まで保持します。直近のログは次のコマンドで確認できます。

```powershell
$log = Get-ChildItem "$env:LOCALAPPDATA\Azookey\logs\client\client-*.log" |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
Get-Content $log.FullName -Tail 200
```

launcher、変換server、候補UIの起動・終了と標準出力は`%LOCALAPPDATA%\Azookey\logs\runtime.log`に保存され、1 MiBを超える前に`runtime.log.1`へローテーションします。起動直後にserver/UIが終了する場合は次のコマンドで確認できます。

```powershell
Get-Content "$env:LOCALAPPDATA\Azookey\logs\runtime.log" -Tail 200
```

インストーラーを使う方法を推奨します。開発中にDLLだけ手動登録する場合は、管理者権限のターミナルで次を実行してください。

```powershell
& "$env:WINDIR\System32\regsvr32.exe" "$PWD\build\azookey_windows.dll" /s
& "$env:WINDIR\SysWOW64\regsvr32.exe" "$PWD\build\x86\azookey_windows.dll" /s
```

登録後、管理者権限のターミナルから`Start-Process "$PWD\build\launcher.exe"`を実行します。登録解除時は、同じ2つの`regsvr32`コマンドに`/u`を付けます。

#### 開発時のヒント
- 開発は仮想マシンまたは専用のPCで行うことを推奨します。IMEがクラッシュするとWindowsがフリーズする可能性があります。
- IMEを解除する際、IMEを使用中のアプリケーション（メモ帳など）を終了しないと、解除できないことがあります。

# 関連

- [azooKey/azooKey](https://github.com/azooKey/azooKey): iOS / iPadOS向けの日本語キーボードアプリ
- [7ka-Hiira/fcitx5-hazkey](https://github.com/7ka-Hiira/fcitx5-hazkey): fcitx5向けのLinux版azooKey
- [azooKey/AzookeyKanakanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter): azooKeyの変換エンジン

# 参考
本プロジェクトの開発にあたり、以下のリソースを参考にしました。ありがとうございます！
- [OMAMA-Taioan/khiin-rs](https://github.com/OMAMA-Taioan/khiin-rs/tree/master/windows)
- [google/mozc](https://github.com/google/mozc/tree/master/src/win32/tip)
- [microsoft/Windows-classic-samples](https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/Win7Samples/winui/input/tsf/textservice)
- [dec32/ajemi](https://github.com/dec32/ajemi)
- https://zenn.dev/mkpoli/scraps/6dc57fcd0335cf
