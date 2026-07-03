//! モックバックエンド。root 権限なしで UI 開発するためのダミー実装。
//!
//! `install()` は各ステップごとに数百 ms のスリープを挟んで進捗コールバックを呼ぶ。
//! 即時完了させないのは、中間状態（「処理中」表示・プログレスバー）が実際に UI へ
//! 描画される機会を確保し、メッセージポンプが回ることで状態遷移が正しく描画される
//! ことを保証するため（README「モック設計」参照）。

use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use super::{
    host_arch, DiskInfo, ImageMetadata, InstallOptions, InstallerBackend, Progress, ProgressFn,
    Result, SystemInfo,
};

pub struct MockBackend;

impl MockBackend {
    pub fn new() -> Self {
        MockBackend
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InstallerBackend for MockBackend {
    fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        // 実機で lsblk がもたつくケースの再現（busy スピナーの動作確認用）。
        sleep(Duration::from_millis(800));
        Ok(vec![
            DiskInfo {
                path: PathBuf::from("/dev/sda"),
                model: "SanDisk Ultra USB 3.0".into(),
                size: 32 * 1024 * 1024 * 1024,
                removable: true,
            },
            DiskInfo {
                path: PathBuf::from("/dev/nvme0n1"),
                model: "KIOXIA-EXCERIA PRO SSD".into(),
                size: 2_000_398_934_016,
                removable: false,
            },
            DiskInfo {
                path: PathBuf::from("/dev/nvme1n1"),
                model: "KIOXIA-EXCERIA PRO SSD".into(),
                size: 2_000_398_934_016,
                removable: false,
            },
        ])
    }

    fn scan_images(&self) -> Result<Vec<ImageMetadata>> {
        // squashfs メタデータ読み取りの遅延を再現。起動時は list_disks と同時に
        // 走るので busy カウンタの重なり（bool では破綻するケース）の検証にもなる。
        sleep(Duration::from_millis(1200));
        let arch = host_arch();
        Ok(vec![
            ImageMetadata {
                path: PathBuf::from("/run/initramfs/boot/genpack.squashfs"),
                filename: "genpack.squashfs".into(),
                size: 1_200 * 1024 * 1024, // 1.2GiB → superfloppy 可
                arch: arch.clone(),
                artifact: "genpack".into(),
                banner: "  __ _  ___ _ __  _ __   __ _  ___| | __\n / _` |/ _ \\ '_ \\| '_ \\ / _` |/ __| |/ /\n| (_| |  __/ | | | |_) | (_| | (__|   < \n \\__, |\\___|_| |_| .__/ \\__,_|\\___|_|\\_\\\n |___/           |_|".into(),
                version: "commit a1b2c3d 2026-06-20 12:34:56".into(),
                arch_match: true,
            },
            ImageMetadata {
                path: PathBuf::from("/run/initramfs/boot/walbrix.squashfs"),
                filename: "walbrix.squashfs".into(),
                size: 5 * 1024 * 1024 * 1024, // 5GiB → superfloppy 不可
                arch: arch.clone(),
                artifact: "walbrix".into(),
                banner: String::new(),
                version: "commit deadbee 2026-06-25 09:00:00".into(),
                arch_match: true,
            },
            ImageMetadata {
                path: PathBuf::from("/run/initramfs/boot/other-arch.squashfs"),
                filename: "other-arch.squashfs".into(),
                size: 800 * 1024 * 1024,
                arch: if arch == "x86_64" { "aarch64".into() } else { "x86_64".into() },
                artifact: "genpack".into(),
                banner: String::new(),
                version: "commit 0000000 2026-01-01 00:00:00".into(),
                arch_match: false,
            },
        ])
    }

    fn read_image_metadata(&self, path: &Path) -> Result<ImageMetadata> {
        self.scan_images()?
            .into_iter()
            .find(|m| m.path == path)
            .ok_or_else(|| format!("no such image: {}", path.display()))
    }

    fn system_info(&self) -> Result<SystemInfo> {
        // 実機情報が読める部分は読み、読めない環境ではダミー値。
        super::real::RealBackend::new()
            .system_info()
            .or_else(|_| {
                Ok(SystemInfo {
                    cpu_model: "Mock CPU @ 3.00GHz".into(),
                    cpu_cores: 8,
                    mem_total: 16 * 1024 * 1024 * 1024,
                    arch: host_arch(),
                    kernel: "mock".into(),
                })
            })
    }

    fn install(&self, opts: &InstallOptions, progress: &ProgressFn) -> Result<()> {
        // real バックエンドと同じステップ構成にして UI の見え方を揃える。
        // superfloppy はパーティションを作らずディスク全体を FAT32 にするので
        // 「Creating partitions」「Formatting data partition」が無い（6 ステップ）。
        // (説明, 所要ms, コピーステップか) の並び。
        let mut steps: Vec<(&str, u64, bool)> = vec![("Checking system image", 300, false)];
        if opts.superfloppy {
            steps.push(("Formatting disk (FAT32)", 700, false));
        } else {
            steps.push(("Creating partitions", 700, false));
            steps.push(("Formatting boot partition (FAT32)", 500, false));
            steps.push(("Formatting data partition (BTRFS)", 500, false));
        }
        steps.push(("Installing bootloader", 600, false));
        steps.push(("Copying system image", 1500, true));
        steps.push(("Writing configuration", 300, false));
        steps.push(("Syncing", 400, false));

        let total = steps.len();
        for (i, (msg, delay, is_copy)) in steps.iter().enumerate() {
            let step = i + 1;
            if *is_copy {
                // 進捗率の分かる操作。fraction を刻んで Progress バーを動かす。
                let ticks = 20u64;
                for t in 0..=ticks {
                    progress(&Progress {
                        step,
                        total,
                        message: msg,
                        fraction: Some(t as f32 / ticks as f32),
                    });
                    sleep(Duration::from_millis(delay / ticks));
                }
            } else {
                // 進捗率の読めない操作。ステップ開始を報告してから作業（スリープ）する。
                progress(&Progress {
                    step,
                    total,
                    message: msg,
                    fraction: None,
                });
                sleep(Duration::from_millis(*delay));
            }
        }
        Ok(())
    }

    fn reboot(&self) -> Result<()> {
        // モックでは実際には再起動せず、何もしない（呼び出し側が完了画面を表示する）。
        eprintln!("[mock] reboot() called — doing nothing");
        Ok(())
    }

    fn poweroff(&self) -> Result<()> {
        eprintln!("[mock] poweroff() called — doing nothing");
        Ok(())
    }
}
