# AzooKey Windows frontend

React / Vite / Tauri の設定画面です。Node.js 20.19 以降（CI は Node 24.18）を使います。

## UI 開発と確認

Storybook は Tauri バックエンドを起動せずに、コンポーネントと設定画面を状態別に確認するための環境です。`@tauri-apps/api/core` は Storybook 専用モックに置き換わるため、ネイティブAPIを呼び出しません。

```sh
npm ci
npm run storybook
```

`http://localhost:6006` で、Controls・Docs・Accessibility・Testing パネルを使えます。ツールバーからライト／ダークテーマも切り替えられます。

## 自動チェック

```sh
npm run test-storybook        # 全ストーリーの描画・play・a11y テスト
npm run test-storybook:watch  # UI実装中の再実行
npm run build-storybook       # 静的 Storybook のビルド確認
```

新しい UI には実装ファイルの近くに `*.stories.tsx` を追加し、通常状態だけでなく、無効状態・長い日本語・重要な操作をカバーしてください。アクセシビリティ違反はローカルと CI の両方で失敗になります。視覚差分テストは、Chromatic プロジェクトとトークンを用意した段階で任意追加します。
