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

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{
    host_arch, DiskInfo, ImageMetadata, InstallOptions, InstallerBackend, Progress, ProgressFn,
    Result, SystemInfo,
};

/// 1 GiB のバイト数。
const GIB: u64 = 1024 * 1024 * 1024;

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
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            // スキャン場所が無い開発環境ではエラーではなく空リストを返す。
            Err(_) => return Ok(Vec::new()),
        };
        // squashfs マジックを持つファイルを (ファイル名, パス) で集める。
        let mut squashfs: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || !is_squashfs(&path) {
                continue;
            }
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            squashfs.push((name, path));
        }
        // system.*（インストーラ自身/更新残骸）を除外し、選ばれたものだけ読む。
        let mut images: Vec<ImageMetadata> = filter_installable_images(squashfs)
            .iter()
            .filter_map(|p| self.read_image_metadata(p).ok())
            .collect();
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

        // squashfs 内の /.genpack/ メタデータを backhand で直接読む。
        let meta = read_genpack_metadata(path);
        let arch = meta.arch.unwrap_or_default();
        // arch が読めない場合は host と同じ扱い（警告を出さない）。読めて違えば
        // arch_match=false → UI で警告を出すが、選択・インストールは可能にする
        // （MicroSD を PC に挿して別アーキ向けに入れる用途を塞がないため）。
        let arch_match = arch.is_empty() || arch == host_arch();
        // artifact はデフォルトホスト名の候補。読めなければファイル名から推測する。
        let artifact = meta
            .artifact
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                filename
                    .strip_suffix(".squashfs")
                    .unwrap_or(&filename)
                    .to_string()
            });

        Ok(ImageMetadata {
            path: path.to_path_buf(),
            filename,
            size,
            arch,
            artifact,
            banner: meta.banner.unwrap_or_default(),
            version: meta.version.unwrap_or_default(),
            arch_match,
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

    fn install(&self, opts: &InstallOptions, progress: &ProgressFn) -> Result<()> {
        install_to_disk(opts, progress)
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

// ===========================================================================
// インストール実処理
//
// オリジナル genpack-install.cpp の install_to_disk() に忠実な移植。
// ライブラリ呼び出し（libmount / libblkid）は依存を増やさず mount(8) / blkid(8)
// のサブプロセスで代替する（ゲスト内 root で動く。README が挙げる nix::mount は
// 将来 PTY 実装で nix を導入する際に合わせて検討）。
//
// 未対応（今回は x86 検証を優先してスコープ外）:
//   - ラズパイ用 boot ファイル（イメージ内 boot/bootcode.bin があるとき boot/ 以下を
//     ブートパーティションへ展開する処理）。install_bootloaders() に差し込み口を残す。
// ===========================================================================

/// system.img コピーステップの表示文言（開始報告と進捗報告で共有）。
const COPY_MSG: &str = "Copying system image";

/// 「Step X/N」表示のためのステップ進行管理。
struct Stepper<'a> {
    step: usize,
    total: usize,
    progress: &'a ProgressFn<'a>,
}

impl<'a> Stepper<'a> {
    /// 次のステップを開始する（進捗率なし＝ Progress バー非表示）。
    fn begin(&mut self, message: &str) {
        self.step += 1;
        (self.progress)(&Progress {
            step: self.step,
            total: self.total,
            message,
            fraction: None,
        });
    }

    /// 現ステップの進捗率を更新する（コピー等、進捗の読める操作用）。
    fn fraction(&self, message: &str, fraction: f32) {
        (self.progress)(&Progress {
            step: self.step,
            total: self.total,
            message,
            fraction: Some(fraction),
        });
    }
}

/// ディスクへのインストール本体。
fn install_to_disk(opts: &InstallOptions, progress: &ProgressFn) -> Result<()> {
    let disk = opts.disk.as_path();
    let image = opts.image.as_path();

    let image_size = std::fs::metadata(image)
        .map_err(|e| format!("stat {}: {e}", image.display()))?
        .len();
    // 4GiB 未満ならブートパーティション(FAT32)に system.img として置ける。
    let fits_boot = image_size < 4 * GIB;

    if opts.superfloppy && !fits_boot {
        return Err("superfloppy mode requires a system image smaller than 4 GiB".into());
    }

    // ステップ総数。superfloppy はパーティション作成・data フォーマットが無い分 2 少ない。
    let total = if opts.superfloppy { 6 } else { 8 };
    let mut st = Stepper {
        step: 0,
        total,
        progress,
    };

    // --- 1. システムイメージの検証 ---
    st.begin("Checking system image");
    if !is_squashfs(image) {
        return Err(format!("{} is not a valid squashfs image", image.display()));
    }

    // イメージを RO ループマウントする。SBC 種別判定・ブートローダ探索・SBC 固有ブート
    // ファイルの取得に使う。関数終了まで維持する（Drop で umount）。マウントできない
    // 環境でも致命ではない（ホスト側ブートローダで補える）ので Option で扱う。
    let image_mnt = TempMount::mount_image_ro(image).ok();
    let image_root = image_mnt.as_ref().map(|m| m.path());

    // イメージ内のマーカーファイルから SBC 種別を判定する（対応表は SBC_PROFILES）。
    // ESP フラグの出し分けに使うためパーティション作成より前で判定しておく。
    let sbc = image_root.and_then(detect_sbc);
    if let Some(p) = sbc {
        eprintln!("[genpack-install-gui] detected SBC profile: {}", p.name);
    }

    // --- 2. パーティション作成 / ディスクフォーマット ---
    let boot_dev: PathBuf;
    let data_dev: Option<PathBuf>;
    let bios_compatible: bool;

    if opts.superfloppy {
        st.begin("Formatting disk (FAT32)");
        // -I: パーティションではなくディスク全体を対象にする（誤操作防止の明示フラグ）。
        run(
            "mkfs.vfat",
            [
                OsStr::new("-F"),
                OsStr::new("32"),
                OsStr::new("-s"),
                OsStr::new("32"),
                OsStr::new("-I"),
                OsStr::new("-n"),
                OsStr::new("BOOT"),
                disk.as_os_str(),
            ],
        )?;
        boot_dev = disk.to_path_buf();
        data_dev = None;
        // superfloppy は EFI 起動専用（BIOS ブートローダは導入しない）。
        bios_compatible = false;
    } else {
        // BIOS 互換（msdos ラベル + grub-bios-setup）にできる条件:
        // 2TiB 以下かつ論理セクタ 512B。GPT 選択オプションは GUI に無いので gpt=false 固定。
        let (disk_size, log_sec) = disk_geometry(disk);
        bios_compatible = disk_size <= 2 * 1024 * GIB && log_sec == 512;

        st.begin("Creating partitions");
        // ブートパーティションのサイズ(GiB)。イメージがブートに収まるなら必要容量、
        // 収まらない（>=4GiB、data パーティション行き）なら 1GiB で足りる。
        let boot_gib = if fits_boot {
            least_capacity_gib(image_size)
        } else {
            1
        };
        let boot_end = format!("{boot_gib}GiB");
        let label = if bios_compatible { "msdos" } else { "gpt" };
        // parted は各コマンドを 1 トークン（スペース区切り文字列）で渡す必要がある。
        // トークンに分割すると getopt が "-1"（ディスク末尾指定）をオプションと誤認し
        // 「invalid option -- '1'」で失敗する（デバイスパスだけは独立トークン）。
        let mut args: Vec<String> = vec![
            "--script".to_string(),
            disk.to_string_lossy().into_owned(),
            format!("mklabel {label}"),
            format!("mkpart primary fat32 1MiB {boot_end}"),
            format!("mkpart primary btrfs {boot_end} -1"),
            "set 1 boot on".to_string(),
        ];
        // parted の boot/esp フラグはパーティションテーブル種別で意味が変わる（実測で確認済み）:
        //
        //   ┌────────┬─────────────────────────┬──────────────────────────────┐
        //   │        │ boot on                 │ esp on                       │
        //   ├────────┼─────────────────────────┼──────────────────────────────┤
        //   │ msdos  │ MBR アクティブフラグ(0x80,  │ タイプ 0xEF(=ESP, UEFI 用)。   │
        //   │ (MBR)  │ BIOS 用)。タイプは 0x0c     │ boot とは別物                 │
        //   │        │ (FAT32-LBA)のまま         │                              │
        //   │ gpt    │ タイプGUIDを ESP           │ boot のエイリアス（冗長）      │
        //   │        │ (C12A7328-…)にする ＝ ESP  │                              │
        //   └────────┴─────────────────────────┴──────────────────────────────┘
        //
        // ⇒ msdos では BIOS 起動(active)と UEFI 認識(0xEF)が別フラグなので boot+esp 両方必要。
        //    GPT では boot on だけで ESP になり、esp on は冗長。
        //    よって esp on は msdos 経路(bios_compatible)でのみ立てる。これで両経路とも
        //    正しく ESP になる（GPT 経路で ESP が抜ける、といった穴は無い）。
        //
        // ただし SBC によっては ESP タイプ(0xEF)を嫌う。Raspberry Pi のブートファーム
        // ウェアはブートパーティションが FAT32-LBA(0x0c)のままであることを要求する
        // （オリジナル genpack-install の `--no-esp` "some bootloaders dislike it" がこれ）。
        // そこで ESP を立てるかは SBC プロファイルの mark_esp に従う（未検出＝通常は true）。
        let mark_esp = sbc.is_none_or(|p| p.mark_esp);
        if bios_compatible && mark_esp {
            args.push("set 1 esp on".to_string());
        }
        run("parted", &args)?;
        // パーティションデバイスノードの出現を待つ（udev 反映のタイムラグ対策）。
        let _ = run("udevadm", [OsStr::new("settle")]);
        let bootp = partition_path(disk, 1);
        let datap = partition_path(disk, 2);
        wait_for_path(&bootp, 25)?;
        wait_for_path(&datap, 25)?;

        st.begin("Formatting boot partition (FAT32)");
        run(
            "mkfs.vfat",
            [
                OsStr::new("-F"),
                OsStr::new("32"),
                OsStr::new("-s"),
                OsStr::new("32"),
                OsStr::new("-n"),
                OsStr::new("BOOT"),
                bootp.as_os_str(),
            ],
        )?;
        // data パーティションのラベルは boot の UUID を後置する（オリジナルに倣う）。
        let boot_uuid = blkid_uuid(&bootp)?;

        st.begin("Formatting data partition (BTRFS)");
        let data_label = format!("data-{boot_uuid}");
        run(
            "mkfs.btrfs",
            [
                OsStr::new("-q"),
                OsStr::new("-L"),
                OsStr::new(&data_label),
                OsStr::new("-f"),
                datap.as_os_str(),
            ],
        )?;

        boot_dev = bootp;
        data_dev = Some(datap);
    }

    // boot パーティション（superfloppy 時はディスク自体）をマウント。
    // コピー・設定書き込みが終わるまで維持する（Drop で umount）。
    let boot_mnt = TempMount::mount(&boot_dev, Some("vfat"), &["fmask=177", "dmask=077"])?;

    // --- 3. ブートローダ導入 ---
    st.begin("Installing bootloader");
    // SBC 固有のブートファイル（例: RPi の boot/ 一式）があれば先に導入する。
    if let (Some(profile), Some(root)) = (sbc, image_root)
        && let Some(handler) = profile.install_boot_files
    {
        handler(root, boot_mnt.path())?;
    }
    // 汎用 EFI/BIOS ブートローダ。SBC 未検出時は無ければ起動不能なので必須。
    // SBC 検出時は SBC 側のブートファイルで起動できるため、無くてもエラーにしない。
    // また SBC は BIOS 起動しない（RPi 等）ので grub-bios-setup は行わない。
    let bios_disk = if sbc.is_some() {
        None
    } else {
        bios_compatible.then_some(disk)
    };
    install_efi_bios_bootloaders(image_root, boot_mnt.path(), bios_disk, sbc.is_none())?;

    // --- 4. システムイメージのコピー（進捗率あり）---
    st.begin(COPY_MSG);
    if fits_boot {
        let dest = boot_mnt.path().join("system.img");
        copy_with_progress(image, &dest, image_size, |f| st.fraction(COPY_MSG, f))?;
    } else {
        // 4GiB 以上は data パーティション(btrfs)に /system として置く。
        let data_mnt = TempMount::mount(data_dev.as_deref().unwrap(), Some("btrfs"), &[])?;
        let dest = data_mnt.path().join("system");
        copy_with_progress(image, &dest, image_size, |f| st.fraction(COPY_MSG, f))?;
    }

    // --- 5. システム設定（system.ini）を boot パーティションに書き込む ---
    st.begin("Writing configuration");
    write_system_ini(boot_mnt.path(), opts)?;

    // --- 6. sync してからマウント解除（Drop で umount）---
    st.begin("Syncing");
    let _ = run("sync", std::iter::empty::<&OsStr>());

    Ok(())
}

/// system.img を格納するのに最低限必要なディスク容量(GiB)。
/// オリジナル同様 `max(4, imageSize*3 / GiB + 1)`（作業余地込みの見積り）。
fn least_capacity_gib(image_size: u64) -> u64 {
    std::cmp::max(4, image_size.saturating_mul(3) / GIB + 1)
}

/// 汎用 EFI/BIOS ブートローダを導入する。
///
/// ブートローダは「イメージ内蔵 `/usr/lib/genpack-install` → ホスト側 `/usr/local/lib`
/// → `/usr/lib/genpack-install`」の順で探す。`required` が true で見つからない場合は
/// エラーにする（起動不能になるため）。SBC のように別経路で起動できる場合は false を渡す。
fn install_efi_bios_bootloaders(
    image_root: Option<&Path>,
    boot_mnt: &Path,
    bios_disk: Option<&Path>,
    required: bool,
) -> Result<()> {
    let bl = image_root
        .and_then(|root| {
            let p = root.join("usr/lib/genpack-install");
            p.is_dir().then_some(p)
        })
        .or_else(host_bootloader_path);

    let Some(bl) = bl else {
        if required {
            return Err(
                "no bootloader files found (neither in image nor /usr/lib/genpack-install)".into(),
            );
        }
        return Ok(());
    };

    // EFI: bl 直下の boot*.efi を efi/boot/ へコピー（FAT は大小無視なので小文字で可）。
    let efi_boot = boot_mnt.join("efi/boot");
    std::fs::create_dir_all(&efi_boot).map_err(|e| format!("create {}: {e}", efi_boot.display()))?;
    let mut installed_any = false;
    for entry in std::fs::read_dir(&bl)
        .map_err(|e| format!("read {}: {e}", bl.display()))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("boot") && name.ends_with(".efi") {
            std::fs::copy(&path, efi_boot.join(&name))
                .map_err(|e| format!("copy {name}: {e}"))?;
            installed_any = true;
        }
    }
    if required && !installed_any {
        return Err(format!("no boot*.efi found under {}", bl.display()));
    }

    // BIOS: msdos/512B の互換ディスクで、boot.img/core.img/grub.cfg が揃い
    // grub-bios-setup が使えるときのみ導入する。
    if let Some(disk) = bios_disk {
        let boot_img = bl.join("boot.img");
        let core_img = bl.join("core.img");
        let grub_cfg = bl.join("grub.cfg");
        if boot_img.exists()
            && core_img.exists()
            && grub_cfg.exists()
            && grub_bios_setup_available()
        {
            install_bios_bootloader(&boot_img, &core_img, &grub_cfg, boot_mnt, disk)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SBC（シングルボードコンピュータ）別の追加処理
//
// 「イメージ内にこのファイルがあればこの種類の SBC」という判定でプロファイルを選び、
// ESP フラグの扱いや固有のブートファイル導入を切り替える。将来 SBC が増えたら
// SBC_PROFILES に (マーカー, mark_esp, ハンドラ) を 1 行足すだけで済むようにしてある。
// ---------------------------------------------------------------------------

/// SBC ごとの差分をまとめたプロファイル。
struct SbcProfile {
    /// ログ表示用の名前。
    name: &'static str,
    /// イメージルートからの相対パス。これが存在すればこの SBC と判定する。
    marker: &'static str,
    /// ブートパーティションを ESP(0xEF)としてマークしてよいか。
    /// RPi のファームウェアは ESP 型を嫌い FAT32-LBA(0x0c)を要求するため false。
    mark_esp: bool,
    /// SBC 固有のブートファイル導入処理 `(image_root, boot_mnt)`。不要なら None。
    install_boot_files: Option<fn(&Path, &Path) -> Result<()>>,
}

/// 対応 SBC 一覧。上から順に最初にマーカーが一致したものを採用する。
const SBC_PROFILES: &[SbcProfile] = &[SbcProfile {
    name: "Raspberry Pi",
    marker: "boot/bootcode.bin",
    mark_esp: false,
    install_boot_files: Some(install_raspi_boot_files),
}];

/// イメージルート内のマーカーファイルから SBC 種別を判定する。
fn detect_sbc(image_root: &Path) -> Option<&'static SbcProfile> {
    SBC_PROFILES
        .iter()
        .find(|p| image_root.join(p.marker).exists())
}

/// Raspberry Pi 用ブートファイルを導入する（オリジナル install_boot_files の raspi 部分）。
///
/// イメージの `boot/` 以下の全ファイルをブートパーティション直下へ再帰コピーする。
///   - `cmdline.txt`: ブート先に既にあれば触らない。無ければ先頭行の `root=…` を
///     `root=systemimg:auto` に置換し、`rootfstype=…` を除去して書き出す。
///   - `config.txt`: ブート先に既にあれば上書きしない。
///   - それ以外: 上書きコピー。
fn install_raspi_boot_files(image_root: &Path, boot_mnt: &Path) -> Result<()> {
    let src_boot = image_root.join("boot");
    for src in walk_files(&src_boot)? {
        let rel = src.strip_prefix(&src_boot).unwrap_or(&src);
        let dest = boot_mnt.join(rel);

        if rel == Path::new("cmdline.txt") {
            if dest.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&src)
                .map_err(|e| format!("read cmdline.txt: {e}"))?;
            let fixed = fix_raspi_cmdline(content.lines().next().unwrap_or(""));
            std::fs::write(&dest, format!("{fixed}\n"))
                .map_err(|e| format!("write cmdline.txt: {e}"))?;
            continue;
        }
        if rel == Path::new("config.txt") && dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::copy(&src, &dest).map_err(|e| format!("copy {}: {e}", rel.display()))?;
    }
    Ok(())
}

/// RPi の cmdline.txt を書き換える: `root=…` → `root=systemimg:auto`、`rootfstype=…` を除去。
fn fix_raspi_cmdline(line: &str) -> String {
    line.split_whitespace()
        .filter_map(|tok| {
            if tok.starts_with("root=") {
                Some("root=systemimg:auto".to_string())
            } else if tok.starts_with("rootfstype=") {
                None
            } else {
                Some(tok.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// ディレクトリ以下の通常ファイルを再帰的に集める（シンボリックリンクは辿らない）。
fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_files_into(dir, &mut out)?;
    Ok(out)
}

fn walk_files_into(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk_files_into(&path, out)?,
            Ok(ft) if ft.is_file() => out.push(path),
            _ => {} // シンボリックリンク・特殊ファイルは無視
        }
    }
    Ok(())
}

/// BIOS 用 grub を導入する（boot.img/core.img/grub.cfg を配置し grub-bios-setup 実行）。
fn install_bios_bootloader(
    boot_img: &Path,
    core_img: &Path,
    grub_cfg: &Path,
    boot_mnt: &Path,
    disk: &Path,
) -> Result<()> {
    let grub_dir = boot_mnt.join("boot/grub");
    std::fs::create_dir_all(&grub_dir).map_err(|e| format!("create {}: {e}", grub_dir.display()))?;
    for (src, name) in [
        (boot_img, "boot.img"),
        (core_img, "core.img"),
        (grub_cfg, "grub.cfg"),
    ] {
        std::fs::copy(src, grub_dir.join(name)).map_err(|e| format!("copy {name}: {e}"))?;
    }
    run(
        "grub-bios-setup",
        [OsStr::new("-d"), grub_dir.as_os_str(), disk.as_os_str()],
    )?;
    // grub-bios-setup が MBR/gap に埋め込んだ後は boot.img/core.img は不要（オリジナルに倣い削除）。
    let _ = std::fs::remove_file(grub_dir.join("boot.img"));
    let _ = std::fs::remove_file(grub_dir.join("core.img"));
    Ok(())
}

/// システム設定を `system.ini` として boot パーティションに書き出す。
///
/// genpack-init は `/run/initramfs/boot/system.ini` を Python の configparser で読む。
/// 読み込み時に先頭へ暗黙の `[_default]` セクションが付くため、キーはセクション見出し
/// 無しで並べる。キー名は genpack-init の設定モジュールで確認済み:
///   hostname / timezone(zoneinfo パス) / locale(LANG 値)。空欄のキーは書かない。
fn write_system_ini(boot_mnt: &Path, opts: &InstallOptions) -> Result<()> {
    let mut body = String::new();
    for (key, value) in [
        ("hostname", opts.hostname.trim()),
        ("timezone", opts.timezone.trim()),
        ("locale", opts.locale.trim()),
    ] {
        if !value.is_empty() {
            body.push_str(&format!("{key}={value}\n"));
        }
    }
    if body.is_empty() {
        return Ok(());
    }
    let path = boot_mnt.join("system.ini");
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

/// ファイルをチャンクコピーし、進捗率を報告する。
/// UI スレッドへの通知を間引くため 1% 刻みでのみ `report` を呼ぶ。
fn copy_with_progress(
    src: &Path,
    dst: &Path,
    total_size: u64,
    report: impl Fn(f32),
) -> Result<()> {
    let mut reader = File::open(src).map_err(|e| format!("open {}: {e}", src.display()))?;
    let mut writer = File::create(dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut copied: u64 = 0;
    let mut last_pct: i64 = -1;
    report(0.0);
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", src.display()))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("write {}: {e}", dst.display()))?;
        copied += n as u64;
        // total_size==0 なら読む前に n==0 で break 済みなのでここには来ない。max(1) で除算は安全。
        let pct = (copied * 100 / total_size.max(1)) as i64;
        if pct != last_pct {
            last_pct = pct;
            report(copied as f32 / total_size.max(1) as f32);
        }
    }
    writer
        .flush()
        .map_err(|e| format!("flush {}: {e}", dst.display()))?;
    report(1.0);
    Ok(())
}

/// ホスト側のブートローダ配置パスを探す（イメージ内蔵が無い場合のフォールバック）。
fn host_bootloader_path() -> Option<PathBuf> {
    for p in ["/usr/local/lib/genpack-install", "/usr/lib/genpack-install"] {
        let path = Path::new(p);
        if path.is_dir() {
            return Some(path.to_path_buf());
        }
    }
    None
}

/// grub-bios-setup が実行可能か（`--version` の成否）。
fn grub_bios_setup_available() -> bool {
    Command::new("grub-bios-setup")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// ディスクの (総バイト数, 論理セクタ長) を `/sys/block/<name>` から読む。
/// `size` は常に 512B セクタ数（カーネル慣習）なので 512 倍でバイト数になる。
fn disk_geometry(disk: &Path) -> (u64, u64) {
    let name = disk
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = Path::new("/sys/block").join(&name);
    let sectors = std::fs::read_to_string(base.join("size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let log_sec = std::fs::read_to_string(base.join("queue/logical_block_size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(512);
    (sectors.saturating_mul(512), log_sec)
}

/// ディスクパスと番号から n 番目のパーティションのデバイスパスを組む。
/// 末尾が数字で終わる名前（nvme0n1, mmcblk0, loop0 等）は `p` を挟む。
fn partition_path(disk: &Path, n: u32) -> PathBuf {
    let s = disk.to_string_lossy();
    let sep = if s.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    PathBuf::from(format!("{s}{sep}{n}"))
}

/// パーティションデバイスノードが現れるまで最大 `tries`×200ms 待つ。
fn wait_for_path(path: &Path, tries: u32) -> Result<()> {
    for _ in 0..tries {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(format!("partition device {} did not appear", path.display()))
}

/// `blkid -s UUID -o value <dev>` で UUID を取得する。
fn blkid_uuid(dev: &Path) -> Result<String> {
    let output = Command::new("blkid")
        .args([
            OsStr::new("-s"),
            OsStr::new("UUID"),
            OsStr::new("-o"),
            OsStr::new("value"),
            dev.as_os_str(),
        ])
        .output()
        .map_err(|e| format!("failed to run blkid: {e}"))?;
    if !output.status.success() {
        return Err(format!("blkid failed for {}", dev.display()));
    }
    let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uuid.is_empty() {
        Err(format!("no UUID for {}", dev.display()))
    } else {
        Ok(uuid)
    }
}

/// 一時マウント。Drop で umount とマウントポイント削除を行い、解除漏れを防ぐ。
struct TempMount {
    dir: PathBuf,
    mounted: bool,
}

impl TempMount {
    /// デバイス/イメージを一時ディレクトリにマウントする。
    /// `fstype` が None なら auto。`options` は `-o` に渡す追加オプション。
    fn mount(source: &Path, fstype: Option<&str>, options: &[&str]) -> Result<Self> {
        Self::mount_inner(source.as_os_str(), fstype, options, false)
    }

    /// squashfs 等のイメージファイルを RO・ループでマウントする。
    fn mount_image_ro(image: &Path) -> Result<Self> {
        Self::mount_inner(image.as_os_str(), None, &["ro"], true)
    }

    fn mount_inner(
        source: &OsStr,
        fstype: Option<&str>,
        options: &[&str],
        add_loop: bool,
    ) -> Result<Self> {
        let dir = make_temp_dir()?;
        let mut opts: Vec<&str> = options.to_vec();
        if add_loop {
            opts.push("loop");
        }
        let opt_str = opts.join(",");

        let mut args: Vec<&OsStr> = Vec::new();
        if let Some(t) = fstype {
            args.push(OsStr::new("-t"));
            args.push(OsStr::new(t));
        }
        if !opt_str.is_empty() {
            args.push(OsStr::new("-o"));
            args.push(OsStr::new(&opt_str));
        }
        args.push(source);
        args.push(dir.as_os_str());

        match run("mount", args) {
            Ok(()) => Ok(TempMount { dir, mounted: true }),
            Err(e) => {
                let _ = std::fs::remove_dir(&dir);
                Err(e)
            }
        }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempMount {
    fn drop(&mut self) {
        if self.mounted {
            let _ = run("umount", [self.dir.as_os_str()]);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// `/tmp` 配下に一意な一時ディレクトリを作る（PID + 起動からの nanoseconds）。
fn make_temp_dir() -> Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("genpack-install-{}-{}", std::process::id(), nanos));
    std::fs::create_dir(&dir).map_err(|e| format!("create temp dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// サブプロセスを実行し、非ゼロ終了なら stderr を含むエラーを返す。
fn run<I, S>(cmd: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {cmd}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let msg = stderr.trim();
    if msg.is_empty() {
        Err(format!("{cmd} exited with {}", output.status))
    } else {
        Err(format!("{cmd}: {msg}"))
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

/// スキャンで見つかった squashfs `(ファイル名, 値)` を、一覧に出すものだけに絞る。
///
/// `system.*`（`system.img` / `system.old` 等 = インストーラ自身の OS や更新の残骸）は
/// 基本的に除外する。ただし他に何もインストールできるものが無いときだけ、救済として
/// `system.img` のみ残す（`system.old` 等は救済対象にしない）。
fn filter_installable_images<T>(items: Vec<(String, T)>) -> Vec<T> {
    let (system, others): (Vec<_>, Vec<_>) = items
        .into_iter()
        .partition(|(name, _)| name.starts_with("system."));
    if !others.is_empty() {
        others.into_iter().map(|(_, v)| v).collect()
    } else {
        system
            .into_iter()
            .filter(|(name, _)| name == "system.img")
            .map(|(_, v)| v)
            .collect()
    }
}

/// squashfs 内 `/.genpack/` のメタデータ。読めなかったフィールドは `None`。
#[derive(Default)]
struct GenpackMeta {
    arch: Option<String>,
    artifact: Option<String>,
    banner: Option<String>,
    version: Option<String>,
}

/// squashfs から `/.genpack/{arch,artifact,banner,timestamp.commit}` を読む。
///
/// 当初は `backhand`（pure Rust）で読む想定だったが、backhand 0.25.1 は拡張シンボリック
/// リンク inode（squashfs inode type 10、実装上 TODO のまま）を扱えず、これを含む genpack
/// イメージを開けなかった。そこでカーネルの squashfs 対応でループマウントし、`.genpack/*` を
/// 通常ファイルとして読む。インストーラは root で動くので mount 可能（ブートローダ導入でも
/// 同様にマウントしている）。マウント不可・ファイル不在の場合は各フィールド `None`（＝呼び出し
/// 側でファイル名からの推測・arch 不明扱いにフォールバック）。
fn read_genpack_metadata(path: &Path) -> GenpackMeta {
    let mut meta = GenpackMeta::default();
    let Ok(mnt) = TempMount::mount_image_ro(path) else {
        return meta;
    };
    let dir = mnt.path().join(".genpack");
    meta.arch = read_meta_line(&dir.join("arch"));
    meta.artifact = read_meta_line(&dir.join("artifact"));
    meta.version = read_meta_line(&dir.join("timestamp.commit"));
    // banner は ASCII アートなので末尾改行だけ落とす（前後空白は保つ）。
    meta.banner = std::fs::read_to_string(dir.join("banner"))
        .ok()
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty());
    meta
}

/// メタデータの 1 行値を読み、前後空白を除いて返す（読めない/空なら `None`）。
fn read_meta_line(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_ignores_system_images_but_rescues_system_img() {
        // 他に通常イメージがあれば system.* は全部無視する。
        let items = vec![
            ("genpack.squashfs".to_string(), 1),
            ("system.img".to_string(), 2),
            ("system.old".to_string(), 3),
        ];
        assert_eq!(filter_installable_images(items), vec![1]);

        // 他に何も無ければ system.img だけ救済（system.old 等は出さない）。
        let items = vec![
            ("system.img".to_string(), 2),
            ("system.old".to_string(), 3),
        ];
        assert_eq!(filter_installable_images(items), vec![2]);

        // system.img すら無ければ空（system.old だけでは救済しない）。
        let items: Vec<(String, i32)> = vec![("system.old".to_string(), 3)];
        assert!(filter_installable_images(items).is_empty());

        // 複数の通常イメージはそのまま残る。
        let items = vec![("a.squashfs".to_string(), 1), ("b.squashfs".to_string(), 2)];
        assert_eq!(filter_installable_images(items), vec![1, 2]);
    }

    #[test]
    fn fix_raspi_cmdline_replaces_root_and_strips_rootfstype() {
        // root= は systemimg:auto に置換、rootfstype= は除去、他は順序を保って残す。
        assert_eq!(
            fix_raspi_cmdline("console=serial0,115200 root=/dev/mmcblk0p2 rootfstype=ext4 rootwait"),
            "console=serial0,115200 root=systemimg:auto rootwait"
        );
        // root= が無ければ足さない（rootwait は root= 接頭辞ではないので残る）。
        assert_eq!(
            fix_raspi_cmdline("console=tty1 rootwait quiet"),
            "console=tty1 rootwait quiet"
        );
        // root= だけでも置換される。
        assert_eq!(fix_raspi_cmdline("root=PARTUUID=abcd-02"), "root=systemimg:auto");
    }

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
