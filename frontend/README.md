# AzooKey Windows frontend

React / Vite / Tauri の設定画面です。Node.js 20.19 以降（CI は Node 24.18）を使います。

## UI 開発と確認

Storybook は Tauri バックエンドを起動せずに、コンポーネントと設定画面を状態別に確認するための環境です。`@tauri-apps/api/core` は Storybook 専用モックに置き換わるため、ネイティブAPIを呼び出しません。

```sh
npm ci
npm run storybook
```

`http://localhost:6006` で、Controls・Docs・Accessibility・Testing パネルを使えます。ツールバーからライト／ダークテーマも切り替えられます。

## 変換候補 UI の開発

変換候補ポップアップは設定画面とは別のネイティブ WebView UI ですが、見た目と候補更新の実装は [`../crates/ui/assets/candidate.html`](../crates/ui/assets/candidate.html) が唯一の編集元です。Rust 側はこの文書を `include_str!` で読み込むため、Storybook で見える候補一覧は本番と同じ HTML / CSS / JavaScript です。

Storybook の `IME / Candidate Window` で、通常状態・長い日本語候補・多数候補のスクロール・選択状態を確認できます。候補 UI を変更したら、次を実行してください。

```sh
npm run storybook
npm run test-storybook
```

Storybook が確認するのは WebView 文書内の候補描画です。キャレット追従、ウィンドウのサイズと画面端補正、最前面・非フォーカス表示、実 Windows の DPI／フォントはネイティブ側の責務なので、リリース前には Windows 上でも確認してください。候補文書のダークモードはブラウザ／OS の `prefers-color-scheme` に従い、Storybook のテーマツールバーとは独立しています。

## 自動チェック

```sh
npm run test-storybook        # 全ストーリーの描画・play・a11y テスト
npm run test-storybook:watch  # UI実装中の再実行
npm run build-storybook       # 静的 Storybook のビルド確認
```

新しい UI には実装ファイルの近くに `*.stories.tsx` を追加し、通常状態だけでなく、無効状態・長い日本語・重要な操作をカバーしてください。アクセシビリティ違反はローカルと CI の両方で失敗になります。視覚差分テストは、Chromatic プロジェクトとトークンを用意した段階で任意追加します。
