# genpack-install-gui

genpack アーティファクトをディスクにインストールするための GUI フロントエンド。Slint + Rust で実装しています。

Wayland コンポジタのない環境（KMS/DRM 直接描画）で動作することを前提に設計されています。インストーラは通常、グラフィカルなデスクトップ環境を持たないブートメディアから起動されるため、Wayland/X11 クライアントである GTK/Qt ではなく、KMS に直接描画できる Slint（`backend-linuxkms`）を採用しています。開発時は Wayland/X 上の `backend-winit` に自動的にフォールバックします。

## 特徴

- **コンポジタ不要の全画面 GUI** — 生 KMS/DRM に直接描画し、libseat 経由で DRM マスターと入力デバイスを取得します。
- **ウィザード形式** — ディスク選択 → イメージ選択 → オプション設定 → インストール → 再起動。
- **squashfs イメージの自動検出** — マジックナンバー判定と `.genpack/` メタデータ（arch / artifact / banner / ビルド情報）の読み取り。
- **アーキテクチャの警告** — 実行環境と異なる arch のイメージには警告を表示します。ただし選択・インストールは可能で、別アーキテクチャ向けにインストールする用途（例: PC に挿した MicroSD へ SBC 用イメージを書き込む）を妨げません。
- **SBC 対応** — Raspberry Pi など、イメージ内のマーカーファイルで種別を判定して固有のブート処理を行うプロファイル方式。
- **多言語対応** — 日本語 / 英語。Slint のバンドル翻訳で単一バイナリに埋め込みます。
- **高 DPI 対応** — 接続ディスプレイの物理解像度に応じてスケールファクターを自動決定します。
- **root 不要の開発** — root 権限が必要な処理を trait で抽象化し、`mock` feature でダミー実装に差し替えられます。

## 仕組み

インストーラは genpack のブートメディア（ESP が `/run/initramfs/boot` にマウントされる）から起動し、そこにある squashfs イメージをスキャンしてインストール対象の一覧を作ります。

実インストール処理（パーティション作成・フォーマット・ブートローダ導入・システムイメージのコピー）は、外部の `genpack-install` CLI を呼び出すのではなく **Rust で直接実装**しています（`parted` / `mkfs.vfat` / `mkfs.btrfs` / `grub-bios-setup` などのコマンド呼び出しと、squashfs のループマウントを利用）。

ディスクのフォーマットは 2 通りです。

- **通常モード** — ブートパーティション（FAT32）+ データパーティション（BTRFS）の 2 パーティション構成。
- **スーパーフロッピーモード** — パーティションを作らずディスク全体を FAT32 とする（システムイメージが 4GiB 未満のときのみ選択可）。

## ビルドと実行

Rust ツールチェーン（Cargo）が必要です。Slint および Rust の依存クレートは Cargo が crates.io から解決します。

```sh
# 実バックエンド（実機向け・インストール処理には root が必要）
cargo build --release

# 開発用モック（root 不要。ディスク列挙・スキャン・インストールをダミー化）
cargo run --features mock

# テスト
cargo test
```

実機では release ビルドのバイナリを実行します。

```sh
LIBSEAT_BACKEND=builtin SLINT_BACKEND=linuxkms ./genpack-install-gui
```

- `LIBSEAT_BACKEND=builtin` — root 実行時に libseat が DRM/入力デバイスを直接オープンします（seatd/logind 不要）。非 root で動かす場合は seatd を常駐させて `LIBSEAT_BACKEND=seatd` を指定します。
- `SLINT_BACKEND=linuxkms` — KMS バックエンドを明示します（Wayland/X セッションが無ければ省略しても自動選択されます）。

## 実行時の依存

Slint および Rust クレートは Cargo が解決しますが、以下のネイティブライブラリ／コマンドはアーティファクト（実行環境）に含まれている必要があります。genpack ベース（Gentoo）でのパッケージ名を併記します。

**Slint（KMS）の実行時共有ライブラリ:**

| 用途 | パッケージ |
|---|---|
| KMS/DRM 描画 | `x11-libs/libdrm` |
| GBM + EGL/GLES（FemtoVG レンダラ） | `media-libs/mesa`（`gbm egl gles2`。`VIDEO_CARDS` は対象 GPU に合わせる） |
| 入力（キーボード/マウス/タッチ） | `dev-libs/libinput` |
| フォント解決 | `media-libs/fontconfig` |
| DRM マスター/入力の権限取得 | `sys-auth/seatd` |
| キーマップ変換 | `x11-libs/libxkbcommon` |
| 日本語グリフ | `media-fonts/noto-cjk` |
| UI 装飾の絵文字 | `media-fonts/noto-emoji` |

**実インストール処理が呼び出すコマンド:**

`sys-block/parted`、`sys-fs/dosfstools`（`mkfs.vfat`）、`sys-fs/btrfs-progs`（`mkfs.btrfs`）、`sys-boot/grub`（`grub-bios-setup`）。加えて、ブートローダのバイナリ（`bootx64.efi` など）と BIOS 用 `boot.img`/`core.img` が `/usr/lib/genpack-install` または `/usr/local/lib/genpack-install`（あるいはインストール対象のイメージ内の同パス）に配置されている前提です。

## アーキテクチャ

- **バックエンド抽象**（`src/backend.rs`） — `InstallerBackend` trait で root 操作を隠蔽し、`real`（実装）と `mock`（ダミー）を `mock` feature で切り替えます。
- **UI**（`ui/main.slint`） — 全画面遷移・メニューバー・各ダイアログ。`@tr(...)` で翻訳対象文字列をマークします。
- **非同期実行**（`src/main.rs`） — 時間のかかる処理（ディスク列挙・squashfs スキャン・イメージコピー）はワーカースレッドで実行し、`slint::invoke_from_event_loop` で結果を UI スレッドへ戻します。実行中は全画面オーバーレイとスピナーで操作を遮断します。
- **スケールファクターの自動設定** — 接続コネクタの物理解像度から目標論理高さ（720px）を基準にスケールを算出し、公式 API（`WindowEvent::ScaleFactorChanged`）で適用します。Wayland/X ではコンポジタに委ねます。
- **翻訳** — `slint-build` の `with_bundled_translations()` で `.po` をビルド時にバイナリへ埋め込むため、ランタイムに `.mo` を配置する必要はありません。

## VM での検証

`tools/vm.py`（Python 標準ライブラリのみ）を使うと、実機に転送せず QEMU 上でインストール処理を検証できます。すべてユーザー権限で完結します（KVM は kvm グループ所属で利用）。

```sh
tools/vm.py run [--image <artifact.squashfs>]   # インストーラを起動（--image で任意イメージを対象に）
tools/vm.py boot [--bios]                        # インストール済みディスクを起動（UEFI / BIOS）
tools/vm.py mkimage <artifact.squashfs>          # 任意イメージを積んだブートメディアを生成
```

`qemu-system-x86_64`、OVMF（`edk2-ovmf`）、`mtools`、`dosfstools` が必要です。ベースとなる検証用アーティファクトの場所は環境変数 `GENPACK_INSTALL_ARTIFACT` で指定します。

## 実装状況

| 項目 | 状態 |
|---|---|
| ディスク / イメージ / オプション / 進捗 / 完了・再起動 画面 | 実装済み |
| 実インストール処理（parted / mkfs / grub / コピー / 設定書き込み） | 実装済み（x86 の UEFI / BIOS で検証） |
| squashfs 検出 + `.genpack` メタデータ読み取り + arch 警告 | 実装済み |
| Raspberry Pi / SBC 対応 | 実装済み |
| システム情報画面 | 実装済み |
| 多言語対応（日本語 / 英語） | 実装済み |
| ターミナル画面 | 未実装（プレースホルダ） |

### 既知の制限

- **ターミナル画面**は未実装です（PTY とターミナルエミュレータを Slint 上に描画する構成を予定）。
- **KMS 起動時の物理ディスプレイ接続待ち** — DRM デバイスの出現後、ディスプレイ（HDMI EDID）の認識完了前に起動すると終了することがあります。systemd の `Restart=on-failure` による再起動で拾う想定です。
- **squashfs メタデータの読み取り**は現状ループマウント（要 root）で行います。pure-Rust リーダは genpack イメージが用いる拡張シンボリックリンク inode を扱えないためです。

## ライセンス

本リポジトリは MIT License（© 2026 Walbrix Corporation）です。詳細は [LICENSE](LICENSE) を参照してください。

UI フレームワークに [Slint](https://slint.dev/) を使用しており、About ダイアログに "Made with Slint" の帰属表示を含みます。Slint 自体のライセンス条項は Slint プロジェクトの規定に従います。
