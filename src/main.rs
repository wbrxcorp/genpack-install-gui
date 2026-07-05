//! genpack-install の GUI フロントエンド（Slint + Rust）。
//!
//! 画面遷移とバックエンド呼び出しの結線を行う。root 権限が必要な実処理は
//! `InstallerBackend` trait の背後に隠し、`--features mock` でモックに差し替える。

mod backend;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use backend::{InstallOptions, InstallerBackend, SystemInfo};
use slint_terminal::{slint_glue, Terminal};

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
    // --wait-for-display: 接続済みディスプレイが現れるまでブロックしてから起動する。
    // Slint(linuxkms) が要求する「Connected なコネクタ＋有効モード」を sysfs で先回り確認し、
    // ディスプレイ認識前に起動して即クラッシュするのを防ぐ（KMS 自動起動用）。
    // Slint 初期化より前に行う必要があるので main の先頭で処理する。
    if std::env::args().any(|a| a == "--wait-for-display") {
        wait_for_display_ready();
    }

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
    // インストーラ環境の root の SSH 鍵をインストール先へ引き継ぐ初期値にする。
    // 鍵があればチェックオン＋プリフィル、無ければチェックオフ（＝入力不要のサイン）。
    let ssh_keys = default_authorized_keys();
    window.set_install_ssh_key(!ssh_keys.is_empty());
    window.set_ssh_pubkey(ssh_keys.into());
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
    // 端末の駆動タイマは run() の間だけ生かす必要があるため main のスコープに束縛して持つ。
    let _term_timer = wire_terminal(&window);

    window.run()?;
    Ok(())
}

/// アプリ内ターミナル（slint-terminal クレート）を結線する。
///
/// 端末実体は Page::Terminal に居る間だけ遅延生成し、離脱・シェル終了で破棄する
/// （root シェルを常駐させない）。描画は 16ms タイマ（UI スレッド）で完結し、
/// 既存の run_busy/invoke_from_event_loop パターンとは独立に動く。
/// 返り値のタイマは呼び出し側で生存させること（drop すると停止する）。
fn wire_terminal(window: &MainWindow) -> slint::Timer {
    /// HeaderBar の高さ（ui/main.slint と一致させる）。端末描画領域はこの下の全域。
    const HEADER_LOGICAL_H: f32 = 56.0;

    /// 端末描画領域の物理ピクセルサイズ（ウィンドウ全体 − ヘッダ）。
    fn terminal_area_px(win: &MainWindow) -> (u32, u32) {
        let scale = win.window().scale_factor();
        let sz = win.window().size(); // 物理ピクセル
        let header = (HEADER_LOGICAL_H * scale) as u32;
        (sz.width, sz.height.saturating_sub(header))
    }

    let term: Rc<RefCell<Option<Terminal>>> = Rc::new(RefCell::new(None));

    // KMS 用ソフトキーリピートの状態。Slint の linuxkms バックエンドは libinput の
    // Pressed/Released をそのまま流すだけでリピートを合成しない（Wayland はコンポジタが合成）。
    // そこで「押下のたびに delay を初期値へ戻す」方式でリピートを補う。こうすると Wayland では
    // コンポジタ由来の連射が毎回 delay をリセットするためソフトリピートは発火せず、KMS
    // （連射なし）でだけ delay 消化後に発火する ＝ プラットフォーム判定が要らない。
    #[derive(Default)]
    struct Repeat {
        bytes: Vec<u8>, // リピート対象の入力バイト列
        key: String,    // 押下中のキー text（release 判定用）
        active: bool,
        delay: u32, // 次のリピートまでの残 tick
        // 同一キーが release を挟まず再入したら「プラットフォームが自前でリピートしている」
        // （＝Wayland）と判定し、以降ソフトリピートを恒久停止する（二重連射の防止）。
        platform_autorepeat: bool,
    }
    // 16ms tick 換算。初回リピートまで ~350ms、以降 ~48ms 間隔。
    const REPEAT_INITIAL_DELAY_TICKS: u32 = 22;
    const REPEAT_INTERVAL_TICKS: u32 = 3;
    let repeat: Rc<RefCell<Repeat>> = Rc::new(RefCell::new(Repeat::default()));

    // キー入力 → PTY。名前付き/方向/Ctrl/Alt は key_to_bytes が VT/C0 へ変換（未対応は None）。
    // 併せてソフトリピートの対象として記録し、初回遅延を張り直す。
    {
        let term = term.clone();
        let repeat = repeat.clone();
        window.on_term_key(move |text, ctrl, alt| {
            if let Some(bytes) = slint_glue::key_to_bytes(text.as_str(), ctrl, alt) {
                if let Some(t) = term.borrow().as_ref() {
                    t.feed_input(&bytes);
                }
                let mut r = repeat.borrow_mut();
                if r.active && r.key == text.as_str() {
                    // release を挟まず同じキーが再入 = コンポジタ由来の自動リピート。
                    r.platform_autorepeat = true;
                }
                r.bytes = bytes;
                r.key = text.to_string();
                r.active = true;
                r.delay = REPEAT_INITIAL_DELAY_TICKS;
            }
        });
    }

    // キー解放 → 同じキーならソフトリピートを止める。
    {
        let repeat = repeat.clone();
        window.on_term_key_released(move |text| {
            let mut r = repeat.borrow_mut();
            if r.key == text.as_str() {
                r.active = false;
            }
        });
    }

    // 毎フレーム駆動。端末ページに居る間だけ実体を持ち、離れたら破棄する。
    // グリッドサイズは changed コールバックに頼らず、毎tickウィンドウ物理サイズから
    // 算出して現在と異なれば resize する（初回フレームから正しい縦横比で描ける）。
    let timer = slint::Timer::default();
    {
        let term = term.clone();
        let repeat = repeat.clone();
        let w = window.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(16),
            move || {
                let Some(win) = w.upgrade() else { return };
                let mut guard = term.borrow_mut();
                if win.get_page() != Page::Terminal {
                    // 離脱時に破棄（poll の外なので on_exit との借用衝突なし）。
                    if guard.is_some() {
                        *guard = None;
                        repeat.borrow_mut().active = false;
                    }
                    return;
                }
                // 遅延生成（root シェルを常駐させないため）。
                if guard.is_none() {
                    // フォントは 20px。KMS 実機の物理解像度でちょうど読める大きさ
                    // （26px は大きすぎた）。Wayland では小さめに見えるが実運用は KMS。
                    let mut t = match Terminal::new(80, 24, 20.0, None) {
                        Ok(t) => t,
                        Err(e) => {
                            eprintln!("terminal: failed to start: {e}");
                            win.set_page(Page::DiskSelect);
                            return;
                        }
                    };
                    // シェル終了で installer 画面へ戻す（コールバック内で drop はしない）。
                    let w2 = w.clone();
                    t.set_on_exit(move |_code| {
                        if let Some(win) = w2.upgrade() {
                            win.set_page(Page::DiskSelect);
                        }
                    });
                    repeat.borrow_mut().active = false; // 前回の残留リピートを持ち越さない
                    *guard = Some(t);
                }
                let t = guard.as_mut().unwrap();
                // 描画領域に合わせてグリッドを追従（変化時のみ）。
                let (pw, ph) = terminal_area_px(&win);
                if pw > 0 && ph > 0 {
                    let (cols, rows) = t.cells_for_pixels(pw, ph);
                    if cols > 0 && rows > 0 && (cols, rows) != t.grid_size() {
                        let _ = t.resize(cols, rows);
                    }
                }
                // ソフトキーリピート（KMS 用。Wayland では delay が張り直され続け発火しない）。
                {
                    let mut r = repeat.borrow_mut();
                    if r.active && !r.platform_autorepeat && !r.bytes.is_empty() {
                        if r.delay > 0 {
                            r.delay -= 1;
                        } else {
                            t.feed_input(&r.bytes);
                            r.delay = REPEAT_INTERVAL_TICKS;
                        }
                    }
                }
                t.poll(); // ここで on_exit が発火し page を戻しうる
                if win.get_page() != Page::Terminal {
                    *guard = None; // exit 由来。破棄する
                    repeat.borrow_mut().active = false;
                    return;
                }
                if t.take_dirty() {
                    let (rgba, iw, ih) = t.render();
                    win.set_term_frame(slint_glue::rgba_to_image(rgba, iw, ih));
                }
            },
        );
    }
    timer
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
            // チェックが外れていれば引き継がない（空文字＝system.ini に書かない）。
            ssh_pubkey: if win.get_install_ssh_key() {
                win.get_ssh_pubkey().to_string()
            } else {
                String::new()
            },
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

/// インストーラ実行環境の root の `authorized_keys` を読み、インストール先へ引き継ぐ
/// SSH 公開鍵の初期値にする（空行・`#` 行は除く。読めなければ空）。
/// インストーラは root で動くので読める。dev/mock（非 root）では読めず空になる。
fn default_authorized_keys() -> String {
    std::fs::read_to_string("/root/.ssh/authorized_keys")
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
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

/// 接続済みディスプレイ（Slint linuxkms が起動できる状態）が現れるまでブロックする。
///
/// Slint の linuxkms バックエンドは起動時に `state()==Connected` のコネクタを探し
/// （`drmoutput.rs`）、無ければ "No connected display connector found" で失敗する。加えて
/// そのコネクタに有効なモードが必要。ここではその条件を、DRM マスターを取らずに sysfs
/// (`/sys/class/drm/*/status` と `modes`) で先回りポーリングする。`primary_drm_resolution()`
/// が Some を返す条件（接続済み＋モードあり）が、そのまま「Slint が起動に成功する条件」と
/// 一致する。ディスプレイの認識が起動より遅れても認識完了まで待つので、KMS 自動起動時の
/// 起動即クラッシュを防げる。
///
/// なお GPU/ドライバ自体の不備（EGL 初期化失敗など）は待っても直らない別問題であり、
/// 本関数の対象外（接続タイミングの競合のみを解消する）。
fn wait_for_display_ready() {
    let start = std::time::Instant::now();
    let mut last_log: Option<std::time::Instant> = None;
    loop {
        if primary_drm_resolution().is_some() {
            if last_log.is_some() {
                eprintln!(
                    "[genpack-install-gui] display ready after {:.1}s",
                    start.elapsed().as_secs_f32()
                );
            }
            return;
        }
        // 待機開始時と、その後およそ 10 秒ごとに 1 行だけ出す。
        let now = std::time::Instant::now();
        if last_log.is_none_or(|t| now.duration_since(t).as_secs() >= 10) {
            eprintln!("[genpack-install-gui] waiting for a connected display...");
            last_log = Some(now);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
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
