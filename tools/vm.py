#!/usr/bin/env python3
"""genpack-install-gui を実機なしで検証するための QEMU ツール。

サブコマンド:
  run      インストーラ(superfloppy)を起動する。ビルドしたてのバイナリは起動用 superfloppy に
           同梱するのでゲスト側でのマウントは不要（中身は .vm/ にキャッシュし、バイナリだけ毎回差替）。
           --image を付けると、任意アーティファクトの squashfs を積んだ大容量 superfloppy を
           生成・キャッシュして起動する（そのイメージをインストール対象にできる）。
  boot     run でインストール済みの target ディスクを起動し、実際にブートするか確認する。
           UEFI(既定) と BIOS(--bios, grub-bios-setup 検証) の両経路。
  mkimage  任意アーティファクトを積んだ superfloppy を単体でビルドする。

すべてユーザー権限で完結する（ホスト root 不要。KVM は kvm グループ所属で使う）。
実インストールの root 処理は VM ゲスト内 root が行う。追加の Python 依存は無し(stdlib のみ)。

スクラッチ類は .vm/ (gitignore 済み) に置く。
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# --- パス設定 -------------------------------------------------------------
GUI_DIR = Path(__file__).resolve().parent.parent
RUN_DIR = GUI_DIR / ".vm"
ART_DIR = Path(
    os.environ.get(
        "GENPACK_INSTALL_ARTIFACT",
        os.path.expanduser("~/projects/genpack-artifacts/genpack-install"),
    )
)
BASE_IMG = ART_DIR / "genpack-install-x86_64.img"
BIN = GUI_DIR / "target" / "release" / "genpack-install-gui"

OVMF_CODE = Path("/usr/share/edk2-ovmf/OVMF_CODE.fd")
OVMF_VARS_TEMPLATE = Path("/usr/share/edk2-ovmf/OVMF_VARS.fd")

# FAT32 の単一ファイル上限(4GiB)。superfloppy に積むターゲットはこれ未満でなければならない。
FAT32_MAX = 4 * 1024**3


# --- 共通ヘルパ -----------------------------------------------------------
def die(msg: str):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def warn(msg: str) -> None:
    print(f"warning: {msg}", file=sys.stderr)


def need_tool(name: str) -> None:
    if shutil.which(name) is None:
        die(f"{name} not found")


def require_file(path: Path, what: str, hint: str | None = None) -> None:
    if not Path(path).is_file():
        msg = f"{what} not found: {path}"
        if hint:
            msg += f"\n  {hint}"
        die(msg)


def run(cmd, quiet: bool = False, **kw) -> subprocess.CompletedProcess:
    """サブプロセスを実行し、失敗したら die する。"""
    try:
        return subprocess.run(
            [str(c) for c in cmd],
            check=True,
            stdout=subprocess.DEVNULL if quiet else None,
            **kw,
        )
    except FileNotFoundError:
        die(f"command not found: {cmd[0]}")
    except subprocess.CalledProcessError as e:
        die(f"command failed ({e.returncode}): {' '.join(map(str, cmd))}")


def have_kvm() -> bool:
    return os.access("/dev/kvm", os.W_OK)


def human(n: float) -> str:
    n = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024:
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} PiB"


# --- QEMU 起動（run/boot 共通）--------------------------------------------
def vga_device(vga: str) -> list[str]:
    # Slint linuxkms は DRM+GBM+EGL を要求する。ゲストの DRM ドライバ次第で
    # virtio-gpu が使えなければ std(bochs-drm)/qxl を試す。
    return {
        "virtio": ["-device", "virtio-gpu-pci"],
        "std": ["-vga", "std"],
        "qxl": ["-vga", "qxl"],
    }.get(vga) or die(f"unknown --vga: {vga} (virtio|std|qxl)")


def firmware_args(firmware: str, vars_path: Path | None) -> list[str]:
    if firmware == "bios":
        # pflash を付けない＝ SeaBIOS が使われる。
        return []
    # UEFI: OVMF。NVRAM(vars) は用途ごとに分けて混線を防ぐ。
    require_file(OVMF_CODE, "OVMF firmware", "sys-firmware/edk2-ovmf ?")
    assert vars_path is not None
    if not vars_path.is_file():
        shutil.copy(OVMF_VARS_TEMPLATE, vars_path)
    return [
        "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}",
        "-drive", f"if=pflash,format=raw,file={vars_path}",
    ]


def launch_qemu(disks, firmware, vga, kvm, vars_path=None):
    """disks を virtio で繋いで qemu を exec する（このプロセスを置き換える）。

    disks: [{'file': path, 'format': 'qcow2'|'raw'}] のリスト。先頭が起動ディスク。
    """
    need_tool("qemu-system-x86_64")
    accel = []
    if kvm and have_kvm():
        accel = ["-enable-kvm", "-cpu", "host"]
    elif kvm:
        warn("/dev/kvm not writable — falling back to TCG (slow)")

    args = ["qemu-system-x86_64", *accel, "-machine", "q35", "-m", "2048", "-smp", "2"]
    args += firmware_args(firmware, vars_path)
    for d in disks:
        args += ["-drive", f"file={d['file']},format={d.get('format', 'qcow2')},if=virtio"]
    args += vga_device(vga)
    args += [
        "-device", "virtio-keyboard-pci",
        "-device", "virtio-tablet-pci",
        "-display", "gtk,gl=on",
        "-serial", "mon:stdio",
    ]
    print(f"launching qemu (firmware={firmware}, vga={vga})...")
    os.execvp(args[0], args)


# --- ディスクイメージ生成ヘルパ -------------------------------------------
def make_overlay(base: Path, out: Path) -> None:
    """base(raw) の上に使い捨て qcow2 オーバーレイを作る（毎回作り直す）。"""
    out.unlink(missing_ok=True)
    run(["qemu-img", "create", "-f", "qcow2", "-b", base, "-F", "raw", out], quiet=True)


# --- 大容量 superfloppy のビルド（mtools、root 不要）----------------------
def _stream_file(src_img: Path, src_path: str, dst_img: Path, dst_path: str) -> None:
    """src_img 内の 1 ファイルを dst_img へ一時ファイルなしでストリームコピーする。

    `mcopy -i src ::/f -` (stdout) → `mcopy -i dst - ::/f` (stdin) のパイプ。
    数GB級の system.img を一時ファイルに落とさず移すため。
    """
    p1 = subprocess.Popen(
        ["mcopy", "-n", "-i", str(src_img), f"::/{src_path}", "-"],
        stdout=subprocess.PIPE,
    )
    p2 = subprocess.Popen(
        ["mcopy", "-n", "-i", str(dst_img), "-", f"::/{dst_path}"],
        stdin=p1.stdout,
    )
    p1.stdout.close()  # p2 が EOF を受け取れるように親側は閉じる
    rc2 = p2.wait()
    rc1 = p1.wait()
    if rc1 != 0 or rc2 != 0:
        die(f"streaming {src_path} failed (mcopy rc {rc1}/{rc2})")


def _copy_efi_tree(src_img: Path, dst_img: Path) -> None:
    """src_img の /EFI ツリーを一時ステージ経由で dst_img のルートへ複製する（~12MBと軽い）。"""
    with tempfile.TemporaryDirectory(prefix="genpack-vm-") as tmp:
        run(["mcopy", "-s", "-n", "-i", src_img, "::/EFI", tmp])
        run(["mcopy", "-s", "-i", dst_img, Path(tmp) / "EFI", "::/"])


def build_superfloppy(artifact: Path | None, out: Path, force: bool = False) -> None:
    """ベースのインストーラ内容(+任意アーティファクト squashfs)を積んだ superfloppy を作る。

    構成: 新規 FAT32 に、ベースから EFI/ と system.img(インストーラOS) を移送し、
    artifact を指定すればそれをルートへ置く。インストーラは /run/initramfs/boot を
    スキャンするので、ターゲットは同じブートメディア(FAT)に物理的に載せる必要がある。
    """
    require_file(BASE_IMG, "base superfloppy")
    for t in ("mkfs.vfat", "mcopy"):
        need_tool(t)

    art_size = 0
    if artifact is not None:
        require_file(artifact, "artifact image")
        art_size = artifact.stat().st_size
        if art_size >= FAT32_MAX:
            die(
                f"{artifact.name} is {human(art_size)} — FAT32 の単一ファイル上限は 4GiB。"
                "FAT ブートメディアには載せられない（>=4GiB はインストーラのスコープ外）。"
            )
    if out.exists() and not force:
        die(f"{out} already exists (use --force / --rebuild-image)")

    slack = 256 * 1024**2
    size = BASE_IMG.stat().st_size + art_size + slack
    what = f" with {artifact.name} ({human(art_size)})" if artifact else ""
    print(f"building superfloppy {out} ({human(size)}){what}...")
    out.parent.mkdir(exist_ok=True)
    out.unlink(missing_ok=True)
    run(["truncate", "-s", str(size), out])
    run(["mkfs.vfat", "-F", "32", "-n", "GENPACK", out], quiet=True)

    # ベースからインストーラ内容を移送。
    _copy_efi_tree(BASE_IMG, out)
    _stream_file(BASE_IMG, "system.img", out, "system.img")

    # ターゲット squashfs をルートへ（インストーラはマジック判定でも拾うが名前は保つ）。
    if artifact is not None:
        run(["mcopy", "-i", out, artifact, f"::/{artifact.name}"])
    print("done.")


def _content_signature(image_opt: str | None) -> str:
    """superfloppy の中身(ベース + 任意ターゲット)の鮮度署名。バイナリは含めない。"""
    parts = [f"base:{BASE_IMG.stat().st_mtime_ns}"]
    if image_opt:
        p = Path(image_opt)
        parts.append(f"image:{p}:{p.stat().st_mtime_ns}")
    return "|".join(parts)


def ensure_boot_superfloppy(image_opt: str | None, rebuild: bool) -> Path:
    """起動用 superfloppy(.vm/superfloppy.img)を用意し、ビルドしたてのバイナリを埋め込む。

    重い中身のビルド(EFI + 1.9GB system.img [+ ターゲット])は、ベース/ターゲットが
    変わったとき(または --rebuild-image)だけ行う。判定は stamp ファイルで持つ
    （バイナリの mcopy で superfloppy の mtime が変わるため mtime 比較は使えない）。
    バイナリ(32MB)は毎回 mcopy で入れ替える（軽い）ので開発ループが速い。
    """
    out = RUN_DIR / "superfloppy.img"
    stamp = RUN_DIR / "superfloppy.stamp"
    sig = _content_signature(image_opt)
    cached = stamp.read_text() if stamp.exists() else ""
    if rebuild or not out.exists() or cached != sig:
        build_superfloppy(Path(image_opt) if image_opt else None, out, force=True)
        stamp.write_text(sig)
    else:
        print(f"superfloppy  : {out} (中身はキャッシュ再利用)")
    # ビルドしたてのバイナリを毎回埋め込む(-o: 既存を上書き)。ゲストの
    # /run/initramfs/boot に現れるのでマウント不要で実行できる。
    run(["mcopy", "-o", "-i", out, BIN, "::/genpack-install-gui"])
    print(f"binary       : embedded ({BIN.name}, {human(BIN.stat().st_size)})")
    return out


# --- サブコマンド ---------------------------------------------------------
def cmd_run(a: argparse.Namespace) -> None:
    require_file(BASE_IMG, "superfloppy image")
    require_file(BIN, "binary", hint="先に `cargo build --release` を実行してください。")
    RUN_DIR.mkdir(exist_ok=True)

    # 起動用 superfloppy（中身はキャッシュ、バイナリは毎回埋め込み）。
    base = ensure_boot_superfloppy(a.image, rebuild=a.rebuild_image)

    boot_ovl = RUN_DIR / "boot.qcow2"
    make_overlay(base, boot_ovl)
    print(f"boot overlay : {boot_ovl} (backing {base})")

    target = RUN_DIR / "target.qcow2"
    if a.fresh or not target.exists():
        target.unlink(missing_ok=True)
        run(["qemu-img", "create", "-f", "qcow2", target, "20G"], quiet=True)
        print(f"target disk  : {target} (fresh 20G)")
    else:
        print(f"target disk  : {target} (reused; --fresh で作り直し)")

    print()
    print("ゲスト内での起動手順（バイナリは起動メディアに同梱済み。マウント不要）:")
    print("  LIBSEAT_BACKEND=builtin SLINT_BACKEND=linuxkms /run/initramfs/boot/genpack-install-gui")
    print("  (noexec で弾かれる場合: install -m755 /run/initramfs/boot/genpack-install-gui /tmp/gi &&")
    print("   LIBSEAT_BACKEND=builtin SLINT_BACKEND=linuxkms /tmp/gi)")
    if a.image:
        print(f"  → インストーラの一覧から {Path(a.image).name} を選んでインストール")
    print()

    disks = [
        {"file": boot_ovl},  # vda: 起動メディア(superfloppy, バイナリ同梱)
        {"file": target},    # vdb: インストール先
    ]
    launch_qemu(disks, "uefi", a.vga, not a.no_kvm, vars_path=RUN_DIR / "OVMF_VARS.fd")


def cmd_boot(a: argparse.Namespace) -> None:
    target = RUN_DIR / "target.qcow2"
    require_file(target, "target disk", hint="先に `tools/vm.py run` でインストールしてください。")
    RUN_DIR.mkdir(exist_ok=True)

    if a.overlay:
        bootdisk = RUN_DIR / "target-run.qcow2"
        bootdisk.unlink(missing_ok=True)
        run(["qemu-img", "create", "-f", "qcow2", "-b", target, "-F", "qcow2", bootdisk], quiet=True)
        print(f"boot disk : {bootdisk} (throwaway overlay of {target})")
    else:
        bootdisk = target
        print(f"boot disk : {target} (本体を直接起動。書き込みは永続化される)")

    firmware = "bios" if a.bios else "uefi"
    disks = [{"file": bootdisk}]
    launch_qemu(disks, firmware, a.vga, not a.no_kvm, vars_path=RUN_DIR / "OVMF_VARS.target.fd")


def cmd_mkimage(a: argparse.Namespace) -> None:
    out = Path(a.out) if a.out else RUN_DIR / "superfloppy.img"
    build_superfloppy(Path(a.artifact), out, force=a.force)
    print(f"superfloppy ready: {out}")
    print(f"起動: tools/vm.py run --image {a.artifact}  (同じ内容をキャッシュから使う)")


# --- 引数パース -----------------------------------------------------------
def main() -> None:
    p = argparse.ArgumentParser(
        prog="vm.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = p.add_subparsers(dest="cmd", required=True)

    def add_common(sp):
        sp.add_argument("--vga", choices=["virtio", "std", "qxl"], default="virtio",
                        help="ディスプレイデバイス (既定: virtio)")
        sp.add_argument("--no-kvm", action="store_true", help="KVM を使わない(TCG、遅い)")

    sp_run = sub.add_parser("run", help="インストーラ(superfloppy)を起動")
    sp_run.add_argument("--image", metavar="SQUASHFS",
                        help="インストール対象アーティファクトの squashfs（大容量superfloppyを生成）")
    sp_run.add_argument("--rebuild-image", action="store_true",
                        help="--image のsuperfloppyをキャッシュ無視で作り直す")
    sp_run.add_argument("--fresh", action="store_true", help="target ディスクを作り直す")
    add_common(sp_run)
    sp_run.set_defaults(func=cmd_run)

    sp_boot = sub.add_parser("boot", help="インストール済み target を起動")
    sp_boot.add_argument("--bios", action="store_true",
                         help="SeaBIOS(BIOS)で起動（grub-bios-setup 検証。既定は UEFI）")
    sp_boot.add_argument("--overlay", action="store_true",
                         help="target を汚さず使い捨てオーバーレイで起動")
    add_common(sp_boot)
    sp_boot.set_defaults(func=cmd_boot)

    sp_mk = sub.add_parser("mkimage", help="任意アーティファクトを積んだ superfloppy を単体ビルド")
    sp_mk.add_argument("artifact", help="積む squashfs のパス")
    sp_mk.add_argument("--out", help="出力先 (既定: .vm/superfloppy.img)")
    sp_mk.add_argument("--force", action="store_true", help="既存の出力を上書きする")
    sp_mk.set_defaults(func=cmd_mkimage)

    a = p.parse_args()
    a.func(a)


if __name__ == "__main__":
    main()
