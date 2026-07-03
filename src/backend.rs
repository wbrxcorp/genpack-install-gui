//! インストーラーのバックエンド抽象。
//!
//! インストール操作の大半は root 権限を必要とする。UI 開発中に毎回 root で
//! 動かすのは非効率なため、操作を trait で抽象化し `mock` feature でモック実装に
//! 差し替えられるようにする。
//!
//! root 不要な操作（システム情報取得・ディスク一覧・squashfs メタデータ読み取り）は
//! 最初から共通実装できるが、ここでは real / mock それぞれに実装を置いている。

use std::path::{Path, PathBuf};

// feature の組み合わせによって一方のバックエンドのみが使われるため、
// 未使用側の dead_code 警告は抑止する。
#[allow(dead_code)]
pub mod mock;
#[allow(dead_code)]
pub mod real;

/// バックエンド共通のエラー型。今回は文字列で十分。
pub type Result<T> = std::result::Result<T, String>;

/// インストール先候補となるブロックデバイス。
#[derive(Debug, Clone)]
pub struct DiskInfo {
    /// デバイスパス（例: `/dev/nvme0n1`）
    pub path: PathBuf,
    /// モデル名（例: `KIOXIA-EXCERIA PRO SSD`）
    pub model: String,
    /// 容量（バイト）
    pub size: u64,
    /// リムーバブルか（USB 等）
    pub removable: bool,
}

impl DiskInfo {
    /// 人間可読の容量文字列（例: `1.8 TiB`）
    pub fn size_human(&self) -> String {
        human_size(self.size)
    }
}

/// squashfs イメージのメタデータ（`.genpack/` 由来）。
#[derive(Debug, Clone)]
pub struct ImageMetadata {
    /// squashfs ファイルのパス
    pub path: PathBuf,
    /// ファイル名（表示用）
    pub filename: String,
    /// squashfs ファイルサイズ（バイト）
    pub size: u64,
    /// `.genpack/arch`（例: `x86_64`）
    pub arch: String,
    /// `.genpack/artifact`（デフォルトホスト名候補）
    pub artifact: String,
    /// `.genpack/banner`（ASCII アート、無ければ空）
    pub banner: String,
    /// `.genpack/timestamp.commit`（バージョン情報、無ければ空）
    pub version: String,
    /// 実行環境の arch と一致するか
    pub arch_match: bool,
}

impl ImageMetadata {
    pub fn size_human(&self) -> String {
        human_size(self.size)
    }

    /// スーパーフロッピーモードが選択可能か。
    /// system.img が 4GiB 未満のときのみ可（README のフォーマット仕様）。
    pub fn superfloppy_available(&self) -> bool {
        self.size < 4 * 1024 * 1024 * 1024
    }
}

/// インストール実行時のオプション。
/// 各フィールドは real バックエンドの実処理（未実装）で参照される。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InstallOptions {
    pub disk: PathBuf,
    pub image: PathBuf,
    /// スーパーフロッピーモード（パーティションを作らずディスク全体を FAT32）
    pub superfloppy: bool,
    pub timezone: String,
    pub locale: String,
    pub hostname: String,
}

/// 実行環境のシステム情報（root 不要）。
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub cpu_model: String,
    pub cpu_cores: usize,
    pub mem_total: u64,
    pub arch: String,
    pub kernel: String,
}

/// インストール進捗の 1 回分の報告。
///
/// インストールは性質の異なる操作の連なりで、進捗の見せ方を 2 層に分ける:
///   - `step` / `total`: 「Step X/N」のステップ表示（1-based）。
///   - `message`: 現在のステップの説明。
///   - `fraction`: 進捗率が読める操作（システムイメージのコピー）でのみ `Some(0.0..=1.0)`。
///     parted / mkfs のように進捗の読めない操作では `None`（UI 側は Progress バーを出さない）。
pub struct Progress<'a> {
    pub step: usize,
    pub total: usize,
    pub message: &'a str,
    pub fraction: Option<f32>,
}

/// インストール進捗コールバック。各ステップの開始時、および進捗率の分かる操作の
/// 途中で繰り返し呼ばれる。
pub type ProgressFn<'a> = dyn Fn(&Progress) + 'a;

pub trait InstallerBackend: Send + Sync {
    /// インストール先候補ディスクの一覧。
    fn list_disks(&self) -> Result<Vec<DiskInfo>>;

    /// スキャン場所（`/run/initramfs/boot/` 等）の squashfs を列挙しメタデータを返す。
    fn scan_images(&self) -> Result<Vec<ImageMetadata>>;

    /// 単一 squashfs のメタデータ読み取り。
    fn read_image_metadata(&self, path: &Path) -> Result<ImageMetadata>;

    /// システム情報（CPU・RAM 等）。
    fn system_info(&self) -> Result<SystemInfo>;

    /// インストール実行。呼び出し側の別スレッドから同期的に呼ぶ想定。
    /// 各ステップごとに `progress` を呼ぶ。
    fn install(&self, opts: &InstallOptions, progress: &ProgressFn) -> Result<()>;

    /// 再起動。実機では戻ってこない。モックではメッセージのみ。
    fn reboot(&self) -> Result<()>;

    /// 電源断。実機では戻ってこない。モックではメッセージのみ。
    fn poweroff(&self) -> Result<()>;
}

/// バイト数を人間可読な文字列に変換する。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

/// 実行環境のアーキテクチャ（`uname -m` 相当）。
/// Rust の `std::env::consts::ARCH`（`x86_64`, `aarch64`, `riscv64` 等）は
/// 概ね genpack の arch 表記と一致するのでそのまま使う。
pub fn host_arch() -> String {
    std::env::consts::ARCH.to_string()
}
