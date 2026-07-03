//! genpack-install の GUI フロントエンド（Slint + Rust）。
//!
//! 画面遷移とバックエンド呼び出しの結線を行う。root 権限が必要な実処理は
//! `InstallerBackend` trait の背後に隠し、`--features mock` でモックに差し替える。

mod backend;

use std::sync::{Arc, Mutex};

use backend::{InstallOptions, InstallerBackend, SystemInfo};

slint::include_modules!();

/// Rust 側で保持する一覧データ。UI の選択インデックスから実体（パス等）を引くのに使う。
/// 非同期取得の完了時に更新するので、refresh 後も UI とここが常に同期する。
///
/// 読み書きは UI スレッドのみで行うが、更新クロージャがワーカースレッドを
/// *経由して*運ばれる（`Send` が必要になる）ため `Rc<RefCell>` は使えず、
/// `Arc<Mutex>` で包む。実際のロック競合は起きない。
#[derive(Default)]
struct AppState {
    disks: Mutex<Vec<backend::DiskInfo>>,
    images: Mutex<Vec<backend::ImageMetadata>>,
}

fn make_backend() -> Arc<dyn InstallerBackend> {
    #[cfg(feature = "mock")]
    {
        Arc::new(backend::mock::MockBackend::new())
    }
    #[cfg(not(feature = "mock"))]
    {
        Arc::new(backend::real::RealBackend::new())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ロケールが ja_* なら日本語、それ以外は英語（バンドル翻訳の既定＝英語ソース）。
    // システムロケールからの自動選択は Slint が起動時に行うため通常は不要だが、
    // KMS 環境などで確実に効かせるため $LANG を見て明示指定する。
    if std::env::var("LANG")
        .map(|l| l.starts_with("ja"))
        .unwrap_or(false)
    {
        let _ = slint::select_bundled_translation("ja");
    }

    let backend = make_backend();
    let state = Arc::new(AppState::default());
    let window = MainWindow::new()?;

    // 生KMS表示時は物理解像度に応じてスケールファクターを設定する。
    // バックエンドが初期化時に自前の初期スケール（EDID由来など）を設定した後に
    // 上書きしたいので、invoke_from_event_loop でループ開始後にディスパッチする。
    if let Some(scale) = desired_kms_scale() {
        let w = window.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = w.upgrade() {
                app.window()
                    .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                        scale_factor: scale,
                    });
                // ScaleFactorChanged は set_scale_factor するだけでルートWindowの
                // 論理サイズを更新しない。物理サイズは変わらないので、旧スケールで組まれた
                // ジオメトリのままだと画面をはみ出す（下部ボタンが切れる）。
                // 本物のウィンドウシステムがスケール変更時にResizedも送るのに倣い、
                // 物理サイズ÷新スケール＝論理サイズで Resized を発行して組み直す。
                let logical = app.window().size().to_logical(scale);
                app.window()
                    .dispatch_event(slint::platform::WindowEvent::Resized { size: logical });
            }
        });
    }

    // 実行環境の既定値をプリセット。
    window.set_timezone(default_timezone().into());
    window.set_locale(default_locale().into());
    window.set_debug_build(cfg!(debug_assertions));
    window.set_app_version(env!("CARGO_PKG_VERSION").into());

    // 初期データ投入（非同期。完了までは busy オーバーレイが出る）。
    load_disks(&window, &backend, &state);
    load_images(&window, &backend, &state);

    // ---- コールバック結線 ----
    {
        let w = window.as_weak();
        let b = backend.clone();
        let s = state.clone();
        window.on_refresh_disks(move || {
            if let Some(w) = w.upgrade() {
                load_disks(&w, &b, &s);
            }
        });
    }
    {
        let w = window.as_weak();
        let b = backend.clone();
        let s = state.clone();
        window.on_refresh_images(move || {
            if let Some(w) = w.upgrade() {
                load_images(&w, &b, &s);
            }
        });
    }
    {
        let w = window.as_weak();
        let b = backend.clone();
        window.on_load_sysinfo(move || {
            if let Some(w) = w.upgrade() {
                let b = b.clone();
                run_busy(
                    &w,
                    move || b.system_info(),
                    |w, info| {
                        let text = match info {
                            Ok(info) => format_sysinfo(&info),
                            Err(e) => format!("failed to read system info: {e}"),
                        };
                        w.set_sysinfo_text(text.into());
                    },
                );
            }
        });
    }
    {
        let w = window.as_weak();
        let b = backend.clone();
        window.on_do_reboot(move || {
            if let Err(e) = b.reboot() {
                if let Some(w) = w.upgrade() {
                    // 再起動できない環境（開発中など）はエラー表示に留める。
                    w.set_install_error(format!("reboot failed: {e}").into());
                }
            }
        });
    }
    {
        let w = window.as_weak();
        let b = backend.clone();
        window.on_do_poweroff(move || {
            if let Err(e) = b.poweroff() {
                if let Some(w) = w.upgrade() {
                    w.set_install_error(format!("poweroff failed: {e}").into());
                }
            }
        });
    }
    // debug ビルド専用メニューから。イベントループを抜けて run() を正常リターンさせる。
    window.on_do_exit(|| {
        let _ = slint::quit_event_loop();
    });
    wire_install(&window, backend.clone(), state.clone());

    window.run()?;
    Ok(())
}

/// 「ちょっと時間のかかる操作」の汎用ランナー。
/// busy カウンタを増やしてワーカースレッドで `work` を実行し、完了したら
/// UI スレッドへ戻って `done` に結果を渡し、カウンタを戻す。
/// busy カウンタが 0 より大きい間、UI はオーバーレイ＋スピナーで操作不能になる。
///
/// `done` は UI スレッドでしか*呼ばれない*が、ワーカースレッドを経由して
/// *運ばれる*ため `Send` 境界が必要（Send は「どこで実行するか」ではなく
/// 「スレッド境界を越えて移動できるか」の性質）。
fn run_busy<T: Send + 'static>(
    window: &MainWindow,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&MainWindow, T) + Send + 'static,
) {
    window.set_busy_count(window.get_busy_count() + 1);
    let w = window.as_weak();
    std::thread::spawn(move || {
        let result = work();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(win) = w.upgrade() {
                win.set_busy_count(win.get_busy_count() - 1);
                done(&win, result);
            }
        });
    });
}

/// インストール開始ボタンの結線。処理は別スレッドで実行し、進捗を UI スレッドへ戻す。
fn wire_install(window: &MainWindow, backend: Arc<dyn InstallerBackend>, state: Arc<AppState>) {
    let w = window.as_weak();
    window.on_start_install(move || {
        let Some(win) = w.upgrade() else { return };

        let disk_idx = win.get_selected_disk();
        let image_idx = win.get_selected_image();
        let disks = state.disks.lock().unwrap();
        let images = state.images.lock().unwrap();
        let (Some(disk), Some(image)) =
            (disks.get(disk_idx as usize), images.get(image_idx as usize))
        else {
            win.set_install_error("no disk/image selected".into());
            win.set_page(Page::Done);
            return;
        };

        let opts = InstallOptions {
            disk: disk.path.clone(),
            image: image.path.clone(),
            superfloppy: win.get_superfloppy(),
            timezone: win.get_timezone().to_string(),
            locale: win.get_locale().to_string(),
            hostname: win.get_hostname().to_string(),
        };
        drop(images);
        drop(disks);

        // インストール中画面へ。
        win.set_install_error("".into());
        win.set_progress(0.0);
        win.set_install_step(0);
        win.set_install_total_steps(0);
        win.set_install_step_message("".into());
        win.set_install_has_progress(false);
        win.set_page(Page::Installing);

        let backend = backend.clone();
        let w2 = w.clone();
        std::thread::spawn(move || {
            // 進捗は UI スレッドへ戻して反映する。
            let w_prog = w2.clone();
            let progress = move |p: &backend::Progress| {
                let step = p.step as i32;
                let total = p.total as i32;
                let msg = p.message.to_string();
                let fraction = p.fraction;
                // ステップインジケータ（済み ■ / 未 □）を組む。■/□ は noto-cjk にグリフあり。
                let indicator: String =
                    "■".repeat(p.step) + &"□".repeat(p.total.saturating_sub(p.step));
                let w_prog = w_prog.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w_prog.upgrade() {
                        win.set_install_step(step);
                        win.set_install_total_steps(total);
                        win.set_install_step_indicator(indicator.into());
                        win.set_install_step_message(msg.into());
                        // 進捗率が読める操作のときだけ Progress バーを出す。
                        match fraction {
                            Some(f) => {
                                win.set_install_has_progress(true);
                                win.set_progress(f);
                            }
                            None => win.set_install_has_progress(false),
                        }
                    }
                });
            };

            let result = backend.install(&opts, &progress);

            // 完了 or 失敗を UI へ。
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(win) = w2.upgrade() {
                    match result {
                        Ok(()) => {
                            win.set_progress(1.0);
                            win.set_install_error("".into());
                        }
                        Err(e) => win.set_install_error(e.into()),
                    }
                    win.set_page(Page::Done);
                }
            });
        });
    });
}

fn load_disks(window: &MainWindow, backend: &Arc<dyn InstallerBackend>, state: &Arc<AppState>) {
    let b = backend.clone();
    let s = state.clone();
    run_busy(
        window,
        move || b.list_disks().unwrap_or_default(),
        move |win, disks| {
            let rows: Vec<DiskInfo> = disks
                .iter()
                .map(|d| DiskInfo {
                    path: d.path.to_string_lossy().into_owned().into(),
                    model: d.model.clone().into(),
                    size: d.size_human().into(),
                    removable: d.removable,
                })
                .collect();
            win.set_disks(slint::ModelRc::new(slint::VecModel::from(rows)));
            // 一覧が入れ替わった可能性があるので選択は解除する。
            win.set_selected_disk(-1);
            *s.disks.lock().unwrap() = disks;
        },
    );
}

fn load_images(window: &MainWindow, backend: &Arc<dyn InstallerBackend>, state: &Arc<AppState>) {
    let b = backend.clone();
    let s = state.clone();
    run_busy(
        window,
        move || b.scan_images().unwrap_or_default(),
        move |win, images| {
            let rows: Vec<ImageInfo> = images
                .iter()
                .map(|m| ImageInfo {
                    path: m.path.to_string_lossy().into_owned().into(),
                    filename: m.filename.clone().into(),
                    size: m.size_human().into(),
                    arch: m.arch.clone().into(),
                    artifact: m.artifact.clone().into(),
                    banner: m.banner.clone().into(),
                    version: m.version.clone().into(),
                    arch_match: m.arch_match,
                    superfloppy_available: m.superfloppy_available(),
                })
                .collect();
            win.set_images(slint::ModelRc::new(slint::VecModel::from(rows)));
            win.set_selected_image(-1);
            *s.images.lock().unwrap() = images;
        },
    );
}

fn format_sysinfo(info: &SystemInfo) -> String {
    format!(
        "CPU:     {}\nCores:   {}\nMemory:  {}\nArch:    {}\nKernel:  {}",
        info.cpu_model,
        info.cpu_cores,
        backend::human_size(info.mem_total),
        info.arch,
        info.kernel,
    )
}

/// `/etc/localtime` のシンボリックリンク先からタイムゾーンを推測する。
fn default_timezone() -> String {
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        // 例: /usr/share/zoneinfo/Asia/Tokyo → Asia/Tokyo
        let s = target.to_string_lossy();
        if let Some(idx) = s.find("zoneinfo/") {
            return s[idx + "zoneinfo/".len()..].to_string();
        }
    }
    "UTC".to_string()
}

/// `$LANG` または `/etc/locale.conf` からロケールを推測する。
fn default_locale() -> String {
    if let Ok(lang) = std::env::var("LANG") {
        if !lang.is_empty() {
            return lang;
        }
    }
    if let Ok(conf) = std::fs::read_to_string("/etc/locale.conf") {
        for line in conf.lines() {
            if let Some(v) = line.strip_prefix("LANG=") {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    "C".to_string()
}

/// 目標とする論理画面高さ(px)。物理解像度によらずこの高さになるようスケールを決める。
/// UI は概ねこの論理サイズを前提に設計してあるので、1024x768〜4K のどれでも
/// フォント・余白の比率が保たれ、極端な小ささやはみ出しを避けられる。
const DESIGN_HEIGHT: f32 = 720.0;

/// 生KMS表示時に適用すべきスケールファクターを算出する。
/// Slint の論理座標系はスケールファクターで物理ピクセルに変換されるため、
/// スケール = 物理高さ / 目標論理高さ とすれば論理画面サイズがほぼ一定になり、
/// 1024x768〜4K のどの物理解像度でもフォント・余白の比率が保たれる。
///
/// Wayland/X（開発時）はコンポジタのスケーリングに任せるので `None`。
/// `SLINT_SCALE_FACTOR` が明示指定されている場合もそちらを優先して `None`。
fn desired_kms_scale() -> Option<f32> {
    if std::env::var_os("SLINT_SCALE_FACTOR").is_some() {
        return None;
    }
    let kms = std::env::var("SLINT_BACKEND")
        .map(|b| b.contains("linuxkms"))
        .unwrap_or(false)
        || (std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none());
    if !kms {
        return None;
    }
    let (w, h) = primary_drm_resolution()?;
    // 小さすぎ・大きすぎを避けるためクランプし、見た目のため小数2桁に丸める。
    let scale = ((h as f32 / DESIGN_HEIGHT).clamp(1.0, 4.0) * 100.0).round() / 100.0;
    eprintln!("[genpack-install-gui] KMS display {w}x{h} -> scale factor {scale}");
    Some(scale)
}

/// 接続済み DRM コネクタの中から、最も解像度の高い（＝主）ディスプレイの物理解像度を返す。
fn primary_drm_resolution() -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // コネクタは card<N>-<CONNECTOR> の形式（例: card0-HDMI-A-1）。
        if !name.starts_with("card") || !name.contains('-') {
            continue;
        }
        let dir = entry.path();
        if std::fs::read_to_string(dir.join("status"))
            .map(|s| s.trim() != "connected")
            .unwrap_or(true)
        {
            continue;
        }
        // modes の先頭行が優先（ネイティブ）モード。
        let modes = std::fs::read_to_string(dir.join("modes")).unwrap_or_default();
        if let Some((w, h)) = modes.lines().next().and_then(parse_drm_mode) {
            if best.map_or(true, |(bw, bh)| {
                (w as u64 * h as u64) > (bw as u64 * bh as u64)
            }) {
                best = Some((w, h));
            }
        }
    }
    best
}

/// `"1920x1080"` や `"1920x1080i"` のようなモード文字列を (幅, 高さ) にパースする。
fn parse_drm_mode(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once('x')?;
    let w: u32 = w.parse().ok()?;
    // 末尾に付く 'i'（インターレース）などの非数字を落とす。
    let h: u32 = h
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some((w, h))
}
