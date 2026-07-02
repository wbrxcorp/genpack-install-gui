//! 実バックエンド。
//!
//! root 不要な操作（ディスク一覧・システム情報・squashfs 検出）は実装済み。
//! root 権限が必要な実処理（パーティション作成・フォーマット・ブートローダー導入・
//! ファイルコピー・再起動）は今回の「仮実装」段階では未実装のスタブとしてある。
//!
//! TODO(要実装):
//!   - `.genpack/` メタデータ読み取りは `backhand` クレートで実装する
//!     （現状はファイル名からの推測 + arch は実行環境と同一とみなす）。
//!   - `install()` の各ステップ（parted / mkfs.vfat / mkfs.btrfs / grub-bios-setup /
//!     ファイルコピー）を nix + コマンド呼び出しで直接実装する。

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{
    host_arch, DiskInfo, ImageMetadata, InstallOptions, InstallerBackend, ProgressFn, Result,
    SystemInfo,
};

/// squashfs のマジックナンバー（ファイル先頭 4 バイト、リトルエンディアン `0x73717368`）。
const SQUASHFS_MAGIC: [u8; 4] = [0x68, 0x73, 0x71, 0x73];

/// 実機ブートした genpack アーティファクトで ESP が常にマウントされている場所。
const IMAGE_SCAN_DIR: &str = "/run/initramfs/boot";

pub struct RealBackend;

impl RealBackend {
    pub fn new() -> Self {
        RealBackend
    }
}

impl Default for RealBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InstallerBackend for RealBackend {
    fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        // lsblk は root 不要。-b: バイト表記, -P: KEY="VALUE" のペア形式。
        // 桁揃えテキスト（連続スペースで列を揃える）はパースが曖昧になるので使わない。
        // パーティションのマウント状態から起動媒体を検出する必要があるため
        // -d は付けず、パーティションも含めて全デバイスを列挙する。
        let output = std::process::Command::new("lsblk")
            .args(["-bo", "PATH,NAME,TYPE,PKNAME,RO,RM,MOUNTPOINT,SIZE,MODEL", "-P"])
            .output()
            .map_err(|e| format!("failed to run lsblk: {e}"))?;
        if !output.status.success() {
            return Err("lsblk failed".into());
        }
        Ok(installable_disks(&String::from_utf8_lossy(&output.stdout)))
    }

    fn scan_images(&self) -> Result<Vec<ImageMetadata>> {
        let dir = Path::new(IMAGE_SCAN_DIR);
        let mut images = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            // スキャン場所が無い開発環境ではエラーではなく空リストを返す。
            Err(_) => return Ok(images),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if !is_squashfs(&path) {
                continue;
            }
            if let Ok(meta) = self.read_image_metadata(&path) {
                images.push(meta);
            }
        }
        images.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(images)
    }

    fn read_image_metadata(&self, path: &Path) -> Result<ImageMetadata> {
        let size = std::fs::metadata(path)
            .map_err(|e| format!("stat failed: {e}"))?
            .len();
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        // TODO: backhand で squashfs 内の /.genpack/{arch,artifact,banner,timestamp.commit}
        // を読む。現状は arch を実行環境と同一とみなし、artifact はファイル名から推測する。
        let arch = host_arch();
        let artifact = filename
            .strip_suffix(".squashfs")
            .unwrap_or(&filename)
            .to_string();

        Ok(ImageMetadata {
            path: path.to_path_buf(),
            filename,
            size,
            arch,
            artifact,
            banner: String::new(),
            version: String::new(),
            arch_match: true,
        })
    }

    fn system_info(&self) -> Result<SystemInfo> {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo")
            .map_err(|e| format!("read /proc/cpuinfo: {e}"))?;
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| format!("read /proc/meminfo: {e}"))?;

        let cpu_model = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "(unknown)".into());
        let cpu_cores = cpuinfo
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count()
            .max(1);
        // MemTotal は kB 単位。
        let mem_total = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0);

        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "(unknown)".into());

        Ok(SystemInfo {
            cpu_model,
            cpu_cores,
            mem_total,
            arch: host_arch(),
            kernel,
        })
    }

    fn install(&self, _opts: &InstallOptions, _progress: &ProgressFn) -> Result<()> {
        // root 権限が必要な実処理は未実装。`--features mock` で MockBackend を使うこと。
        Err("real install is not implemented yet (root operations pending); \
             run with --features mock for UI development"
            .into())
    }

    fn reboot(&self) -> Result<()> {
        systemctl("reboot")
    }

    fn poweroff(&self) -> Result<()> {
        systemctl("poweroff")
    }
}

/// systemctl のサブコマンド（reboot / poweroff）を実行する。
/// genpack アーティファクトは systemd ベースなので実機には常に存在する。
/// root でない場合（開発環境など）は polkit に拒否されて Err が返るだけで無害。
fn systemctl(verb: &str) -> super::Result<()> {
    let status = std::process::Command::new("systemctl")
        .arg(verb)
        .status()
        .map_err(|e| format!("failed to run systemctl {verb}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl {verb} exited with {status}"))
    }
}

/// lsblk -P の `KEY="VALUE"` 並びから key の値を取り出す（入力行のスライスを返す）。
fn pair_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=\"");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

/// `lsblk -bo PATH,NAME,TYPE,PKNAME,RO,RM,MOUNTPOINT,SIZE,MODEL -P` の出力から
/// インストール可能なディスクを抽出する。
///
/// 除外規則はオリジナル genpack-install の print_installable_disks() 相当:
///   ① マウント中のデバイスは「使用中」とみなし、親（PKNAME）を物理ディスクまで
///      再帰的に辿って除外する。起動媒体（ESP が /run/initramfs/boot にマウント）や
///      稼働中の squashfs ループデバイスはこれで消える（＝自分自身へのインストール防止）。
///      オリジナルは親1段のみだったが、パーティション→RAID/LUKS→マウントのような
///      多段スタックでも最下層まで届くよう改良してある。
///      なお②と違い①だけを伝播させる。②を伝播させると「パーティションを持つ
///      ディスク」がすべて消えてしまう。
///   ② read-only（eMMC の boot0/boot1 等）・4GiB 未満・disk/loop 以外は対象外。
///      loop を残すのはループデバイス上のイメージファイルへのテストインストール用。
fn installable_disks(lsblk_output: &str) -> Vec<DiskInfo> {
    // ① マウント中のデバイス名を集め、除外集合が安定するまで親へ伝播する。
    let mut in_use = std::collections::HashSet::new();
    for line in lsblk_output.lines() {
        if !pair_value(line, "MOUNTPOINT").unwrap_or("").is_empty() {
            if let Some(name) = pair_value(line, "NAME") {
                in_use.insert(name.to_string());
            }
        }
    }
    let parents: Vec<(&str, &str)> = lsblk_output
        .lines()
        .filter_map(|l| Some((pair_value(l, "NAME")?, pair_value(l, "PKNAME")?)))
        .filter(|(_, pk)| !pk.is_empty())
        .collect();
    loop {
        let mut changed = false;
        for (name, pk) in &parents {
            if in_use.contains(*name) && !in_use.contains(*pk) {
                in_use.insert(pk.to_string());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // ② 型・RO・サイズの適格性はここで判定する（伝播させない）。
    let mut disks = Vec::new();
    for line in lsblk_output.lines() {
        let (Some(name), Some(path), Some(ty), Some(size)) = (
            pair_value(line, "NAME"),
            pair_value(line, "PATH"),
            pair_value(line, "TYPE"),
            pair_value(line, "SIZE"),
        ) else {
            continue;
        };
        let size: u64 = size.parse().unwrap_or(0);
        if (ty != "disk" && ty != "loop")
            || pair_value(line, "RO") == Some("1")
            || size < 4 * 1024 * 1024 * 1024
            || in_use.contains(name)
        {
            continue;
        }
        let model = pair_value(line, "MODEL").unwrap_or("").trim().to_string();
        disks.push(DiskInfo {
            path: PathBuf::from(path),
            model: if model.is_empty() { "(unknown)".into() } else { model },
            size,
            removable: pair_value(line, "RM") == Some("1"),
        });
    }
    disks
}

/// 先頭 4 バイトを読んで squashfs か判定する。
fn is_squashfs(path: &Path) -> bool {
    // 拡張子が .squashfs でなくてもマジックで判定するが、まず拡張子で軽く絞る。
    let mut buf = [0u8; 4];
    match File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf == SQUASHFS_MAGIC,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_value_extracts_fields() {
        let line = r#"PATH="/dev/sda" SIZE="15376000000" MODEL="Ultra Fit""#;
        assert_eq!(pair_value(line, "PATH"), Some("/dev/sda"));
        assert_eq!(pair_value(line, "SIZE"), Some("15376000000"));
        assert_eq!(pair_value(line, "MODEL"), Some("Ultra Fit"));
        assert_eq!(pair_value(line, "TYPE"), None);
    }

    /// Atom スティックPC（USB起動、内蔵eMMCがインストール先）の実機出力を再現。
    /// 期待: 起動媒体 sda はESPマウントにより親ごと除外、loop0 はマウント中＋RO、
    /// eMMC の boot0/boot1 は RO＋4GiB未満で除外され、mmcblk1 だけが残る。
    #[test]
    fn installable_disks_excludes_boot_media_and_self() {
        let out = concat!(
            r#"PATH="/dev/loop0" NAME="loop0" TYPE="loop" PKNAME="" RO="1" RM="0" MOUNTPOINT="/run/initramfs/ro" SIZE="1876492288" MODEL="""#, "\n",
            r#"PATH="/dev/sda" NAME="sda" TYPE="disk" PKNAME="" RO="0" RM="1" MOUNTPOINT="" SIZE="15376000000" MODEL="Ultra""#, "\n",
            r#"PATH="/dev/sda1" NAME="sda1" TYPE="part" PKNAME="sda" RO="0" RM="1" MOUNTPOINT="/run/initramfs/boot" SIZE="15374000000" MODEL="""#, "\n",
            r#"PATH="/dev/mmcblk1" NAME="mmcblk1" TYPE="disk" PKNAME="" RO="0" RM="0" MOUNTPOINT="" SIZE="31268536320" MODEL="""#, "\n",
            r#"PATH="/dev/mmcblk1boot0" NAME="mmcblk1boot0" TYPE="disk" PKNAME="" RO="1" RM="0" MOUNTPOINT="" SIZE="4194304" MODEL="""#, "\n",
            r#"PATH="/dev/mmcblk1boot1" NAME="mmcblk1boot1" TYPE="disk" PKNAME="" RO="1" RM="0" MOUNTPOINT="" SIZE="4194304" MODEL="""#, "\n",
        );
        let disks = installable_disks(out);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].path, PathBuf::from("/dev/mmcblk1"));
        assert_eq!(disks[0].model, "(unknown)");
        assert!(!disks[0].removable);
    }

    /// 未使用（未マウント・4GiB以上）のループデバイスはテストインストール先として残す。
    #[test]
    fn installable_disks_keeps_unmounted_loop() {
        let out = concat!(
            r#"PATH="/dev/loop1" NAME="loop1" TYPE="loop" PKNAME="" RO="0" RM="0" MOUNTPOINT="" SIZE="8589934592" MODEL="""#, "\n",
        );
        let disks = installable_disks(out);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].path, PathBuf::from("/dev/loop1"));
    }

    /// パーティション→RAID→マウントの多段スタックでも物理ディスクまで除外が届く。
    /// （開発機の実構成: md127 = nvme0n1p3 + nvme1n1p3 の RAID0 が / にマウント。
    ///  オリジナルの1段伝播では nvme0n1 が候補に残ってしまうケース）
    #[test]
    fn installable_disks_excludes_raid_member_disks_recursively() {
        let out = concat!(
            r#"PATH="/dev/nvme0n1" NAME="nvme0n1" TYPE="disk" PKNAME="" RO="0" RM="0" MOUNTPOINT="" SIZE="2000398934016" MODEL="KIOXIA-EXCERIA PRO SSD""#, "\n",
            r#"PATH="/dev/nvme0n1p3" NAME="nvme0n1p3" TYPE="part" PKNAME="nvme0n1" RO="0" RM="0" MOUNTPOINT="" SIZE="1957449170944" MODEL="""#, "\n",
            r#"PATH="/dev/md127" NAME="md127" TYPE="raid0" PKNAME="nvme0n1p3" RO="0" RM="0" MOUNTPOINT="/" SIZE="3914627809280" MODEL="""#, "\n",
            r#"PATH="/dev/nvme1n1" NAME="nvme1n1" TYPE="disk" PKNAME="" RO="0" RM="0" MOUNTPOINT="" SIZE="2000398934016" MODEL="KIOXIA-EXCERIA PRO SSD""#, "\n",
        );
        let disks = installable_disks(out);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].path, PathBuf::from("/dev/nvme1n1"));
    }

    /// マウント中のパーティションを持つディスクは親ごと除外される。
    #[test]
    fn installable_disks_excludes_disk_with_mounted_partition() {
        let out = concat!(
            r#"PATH="/dev/nvme0n1" NAME="nvme0n1" TYPE="disk" PKNAME="" RO="0" RM="0" MOUNTPOINT="" SIZE="2000398934016" MODEL="KIOXIA-EXCERIA PRO SSD""#, "\n",
            r#"PATH="/dev/nvme0n1p1" NAME="nvme0n1p1" TYPE="part" PKNAME="nvme0n1" RO="0" RM="0" MOUNTPOINT="/" SIZE="2000000000000" MODEL="""#, "\n",
            r#"PATH="/dev/nvme1n1" NAME="nvme1n1" TYPE="disk" PKNAME="" RO="0" RM="0" MOUNTPOINT="" SIZE="2000398934016" MODEL="KIOXIA-EXCERIA PRO SSD""#, "\n",
        );
        let disks = installable_disks(out);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].path, PathBuf::from("/dev/nvme1n1"));
        assert_eq!(disks[0].model, "KIOXIA-EXCERIA PRO SSD");
    }
}
